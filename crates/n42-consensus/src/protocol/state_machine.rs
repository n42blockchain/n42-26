use alloy_primitives::B256;
use n42_primitives::{
    BlsSecretKey,
    consensus::{ConsensusMessage, QuorumCertificate, ViewNumber},
};
use std::collections::{HashMap, HashSet};
use tokio::sync::mpsc;

use crate::error::ConsensusResult;
use crate::validator::{EpochManager, LeaderSelector, ValidatorSet};
use super::pacemaker::Pacemaker;
use super::quorum::{TimeoutCollector, VoteCollector};
use super::round::{Phase, RoundState};

/// Maximum number of views ahead a future message can be to be buffered.
/// Messages beyond this window trigger a QC-based view jump attempt.
pub(super) const FUTURE_VIEW_WINDOW: u64 = 50;

/// Maximum number of buffered future-view messages.
/// When exceeded, the oldest (lowest view) messages are evicted.
const MAX_FUTURE_MESSAGES: usize = 64;

/// Events fed into the consensus engine.
#[derive(Debug)]
pub enum ConsensusEvent {
    /// A consensus message received from the network.
    Message(ConsensusMessage),
    /// A block has been executed and is ready for proposal (leader path).
    BlockReady(B256),
    /// Block data has been imported into the execution layer (follower path).
    /// Triggers the deferred vote for the pending proposal.
    BlockImported(B256),
}

/// Actions the consensus engine requests from the outer node.
#[derive(Debug)]
pub enum EngineOutput {
    /// Broadcast a consensus message to the network.
    BroadcastMessage(ConsensusMessage),
    /// Send a message to a specific validator (e.g., vote to leader).
    SendToValidator(u32, ConsensusMessage),
    /// Request block execution for a proposed block hash.
    ExecuteBlock(B256),
    /// A block has been committed at the given view.
    BlockCommitted {
        view: ViewNumber,
        block_hash: B256,
        commit_qc: QuorumCertificate,
    },
    /// View change occurred; new view started.
    ViewChanged {
        new_view: ViewNumber,
    },
    /// Node is behind: detected a view gap larger than threshold.
    /// The orchestrator should initiate state sync to catch up.
    SyncRequired {
        local_view: ViewNumber,
        target_view: ViewNumber,
    },
    /// Equivocation detected: a validator voted for two different blocks in the same view.
    EquivocationDetected {
        view: ViewNumber,
        validator: u32,
        hash1: B256,
        hash2: B256,
    },
    /// An epoch transition has occurred. The validator set may have changed.
    EpochTransition {
        new_epoch: u64,
        validator_count: u32,
    },
}

/// Pending proposal awaiting block data import before voting (follower path).
#[derive(Debug, Clone)]
pub(super) struct PendingProposal {
    pub(super) view: ViewNumber,
    pub(super) block_hash: B256,
}

/// The HotStuff-2 consensus engine.
///
/// An event-driven state machine that processes consensus messages and produces
/// output actions. The outer node drives it via `process_event()` and `on_timeout()`.
///
/// ## Protocol Flow
///
/// ### Optimistic path (2 rounds):
/// 1. **Propose**: Leader broadcasts `Proposal{view, block_hash, justify_qc}`
/// 2. **Vote (Round 1)**: Validators verify and send `Vote` to leader
/// 3. **PreCommit**: Leader forms QC, broadcasts `PrepareQC`
/// 4. **CommitVote (Round 2)**: Validators send `CommitVote` to leader
/// 5. **Commit**: Leader forms CommitQC, broadcasts `Decide`, advances view
///
/// ### Timeout recovery (Round 3):
/// 1. Timer expires → validator broadcasts `Timeout{view, high_qc}`
/// 2. New leader collects 2f+1 timeouts → forms TC
/// 3. New leader broadcasts `NewView{view+1, TC}` → back to Propose
pub struct ConsensusEngine {
    pub(super) my_index: u32,
    pub(super) secret_key: BlsSecretKey,
    pub(super) epoch_manager: EpochManager,
    pub(super) round_state: RoundState,
    pub(super) pacemaker: Pacemaker,
    pub(super) vote_collector: Option<VoteCollector>,
    pub(super) commit_collector: Option<VoteCollector>,
    pub(super) timeout_collector: Option<TimeoutCollector>,
    /// QC formed from Round 1 votes (used to broadcast PrepareQC).
    pub(super) prepare_qc: Option<QuorumCertificate>,
    /// Saved PrepareQC piggybacked into the next Proposal (chained mode).
    pub(super) previous_prepare_qc: Option<QuorumCertificate>,
    pub(super) output_tx: mpsc::Sender<EngineOutput>,
    /// Pending proposal awaiting block data import (follower path).
    pub(super) pending_proposal: Option<PendingProposal>,
    /// Block hashes imported before their matching proposal arrived.
    pub(super) imported_blocks: HashSet<B256>,
    /// Tracks which block hash each validator voted for (equivocation detection).
    pub(super) equivocation_tracker: HashMap<u32, B256>,
    /// Buffer for messages arriving for future views (within FUTURE_VIEW_WINDOW).
    pub(super) future_msg_buffer: Vec<(ViewNumber, ConsensusMessage)>,
}

impl ConsensusEngine {
    /// Creates a new consensus engine.
    pub fn new(
        my_index: u32,
        secret_key: BlsSecretKey,
        validator_set: ValidatorSet,
        base_timeout_ms: u64,
        max_timeout_ms: u64,
        output_tx: mpsc::Sender<EngineOutput>,
    ) -> Self {
        Self::with_epoch_manager(
            my_index,
            secret_key,
            EpochManager::new(validator_set),
            base_timeout_ms,
            max_timeout_ms,
            output_tx,
        )
    }

    /// Creates a new consensus engine with an EpochManager for dynamic validator sets.
    pub fn with_epoch_manager(
        my_index: u32,
        secret_key: BlsSecretKey,
        epoch_manager: EpochManager,
        base_timeout_ms: u64,
        max_timeout_ms: u64,
        output_tx: mpsc::Sender<EngineOutput>,
    ) -> Self {
        Self {
            my_index,
            secret_key,
            epoch_manager,
            round_state: RoundState::new(),
            pacemaker: Pacemaker::new(base_timeout_ms, max_timeout_ms),
            vote_collector: None,
            commit_collector: None,
            timeout_collector: None,
            prepare_qc: None,
            previous_prepare_qc: None,
            output_tx,
            pending_proposal: None,
            imported_blocks: HashSet::new(),
            equivocation_tracker: HashMap::new(),
            future_msg_buffer: Vec::new(),
        }
    }

    /// Creates a consensus engine with recovered state from a persisted snapshot.
    ///
    /// Restores safety invariants (locked_qc, last_committed_qc) so the node
    /// resumes with its previous locking commitments intact.
    pub fn with_recovered_state(
        my_index: u32,
        secret_key: BlsSecretKey,
        epoch_manager: EpochManager,
        base_timeout_ms: u64,
        max_timeout_ms: u64,
        output_tx: mpsc::Sender<EngineOutput>,
        recovered_view: ViewNumber,
        locked_qc: QuorumCertificate,
        last_committed_qc: QuorumCertificate,
        consecutive_timeouts: u32,
    ) -> Self {
        Self {
            my_index,
            secret_key,
            epoch_manager,
            round_state: RoundState::from_snapshot(
                recovered_view,
                locked_qc,
                last_committed_qc,
                consecutive_timeouts,
            ),
            pacemaker: Pacemaker::new(base_timeout_ms, max_timeout_ms),
            vote_collector: None,
            commit_collector: None,
            timeout_collector: None,
            prepare_qc: None,
            previous_prepare_qc: None,
            output_tx,
            pending_proposal: None,
            imported_blocks: HashSet::new(),
            equivocation_tracker: HashMap::new(),
            future_msg_buffer: Vec::new(),
        }
    }

    // ── Public accessors ──

    pub fn current_view(&self) -> ViewNumber {
        self.round_state.current_view()
    }

    pub fn current_phase(&self) -> Phase {
        self.round_state.phase()
    }

    pub fn pacemaker(&self) -> &Pacemaker {
        &self.pacemaker
    }

    /// Returns a mutable reference to the pacemaker.
    ///
    /// Used by the orchestrator to extend the deadline at startup,
    /// allowing time for GossipSub mesh formation before the first proposal.
    pub fn pacemaker_mut(&mut self) -> &mut Pacemaker {
        &mut self.pacemaker
    }

    pub fn validator_count(&self) -> u32 {
        self.validator_set().len()
    }

    pub fn epoch_manager(&self) -> &EpochManager {
        &self.epoch_manager
    }

    pub fn epoch_manager_mut(&mut self) -> &mut EpochManager {
        &mut self.epoch_manager
    }

    pub fn is_current_leader(&self) -> bool {
        LeaderSelector::is_leader(
            self.my_index,
            self.round_state.current_view(),
            self.validator_set(),
        )
    }

    pub fn locked_qc(&self) -> &QuorumCertificate {
        self.round_state.locked_qc()
    }

    pub fn last_committed_qc(&self) -> &QuorumCertificate {
        self.round_state.last_committed_qc()
    }

    pub fn consecutive_timeouts(&self) -> u32 {
        self.round_state.consecutive_timeouts()
    }

    // ── Event processing ──

    /// Processes a consensus event and generates outputs.
    pub fn process_event(&mut self, event: ConsensusEvent) -> ConsensusResult<()> {
        match event {
            ConsensusEvent::Message(msg) => self.process_message(msg),
            ConsensusEvent::BlockReady(block_hash) => self.on_block_ready(block_hash),
            ConsensusEvent::BlockImported(block_hash) => self.on_block_imported(block_hash),
        }
    }

    // ── Message dispatch ──

    fn process_message(&mut self, msg: ConsensusMessage) -> ConsensusResult<()> {
        let msg_view = Self::message_view(&msg);
        let current_view = self.round_state.current_view();

        // Buffer messages for future views (within window) instead of rejecting them.
        // Decide and NewView are always exempt — they handle their own view logic.
        // Timeout messages within FUTURE_VIEW_WINDOW are also exempt — process_timeout
        // handles view synchronization for these.
        if let Some(view) = msg_view {
            let is_exempt = matches!(msg, ConsensusMessage::Decide(_) | ConsensusMessage::NewView(_))
                || (matches!(msg, ConsensusMessage::Timeout(_)) && view <= current_view + FUTURE_VIEW_WINDOW);

            if view > current_view && !is_exempt {
                if view <= current_view + FUTURE_VIEW_WINDOW {
                    if self.future_msg_buffer.len() >= MAX_FUTURE_MESSAGES {
                        // Evict the oldest (lowest view) entry.
                        if let Some(min_idx) = self.future_msg_buffer
                            .iter()
                            .enumerate()
                            .min_by_key(|(_, (v, _))| *v)
                            .map(|(i, _)| i)
                        {
                            self.future_msg_buffer.swap_remove(min_idx);
                        }
                    }
                    tracing::debug!(
                        current_view,
                        msg_view = view,
                        buffered = self.future_msg_buffer.len(),
                        "buffering future-view message"
                    );
                    self.future_msg_buffer.push((view, msg));
                    return Ok(());
                }

                // Beyond FUTURE_VIEW_WINDOW: attempt QC-based view jump.
                if self.try_qc_view_jump(&msg, view) {
                    let new_current = self.round_state.current_view();
                    if view == new_current {
                        return self.dispatch_message(msg);
                    } else if view > new_current && view <= new_current + FUTURE_VIEW_WINDOW {
                        self.future_msg_buffer.push((view, msg));
                    }
                }
                return Ok(());
            }
        }

        // Silently discard stale messages (view < current_view).
        // With GossipSub multi-path delivery, messages may arrive via multiple mesh routes;
        // after the first copy advances our view, subsequent copies are dropped early.
        // Decide and NewView are exempt as they handle past-view logic internally.
        if let Some(view) = msg_view {
            if view < current_view
                && !matches!(msg, ConsensusMessage::Decide(_) | ConsensusMessage::NewView(_))
            {
                tracing::trace!(current_view, msg_view = view, "discarding stale consensus message");
                return Ok(());
            }
        }

        self.dispatch_message(msg)
    }

    fn dispatch_message(&mut self, msg: ConsensusMessage) -> ConsensusResult<()> {
        match msg {
            ConsensusMessage::Proposal(p) => self.process_proposal(p),
            ConsensusMessage::Vote(v) => self.process_vote(v),
            ConsensusMessage::CommitVote(cv) => self.process_commit_vote(cv),
            ConsensusMessage::PrepareQC(pqc) => self.process_prepare_qc(pqc),
            ConsensusMessage::Timeout(t) => self.process_timeout(t),
            ConsensusMessage::NewView(nv) => self.process_new_view(nv),
            ConsensusMessage::Decide(d) => self.process_decide(d),
        }
    }

    fn message_view(msg: &ConsensusMessage) -> Option<ViewNumber> {
        match msg {
            ConsensusMessage::Proposal(p) => Some(p.view),
            ConsensusMessage::Vote(v) => Some(v.view),
            ConsensusMessage::CommitVote(cv) => Some(cv.view),
            ConsensusMessage::PrepareQC(pqc) => Some(pqc.view),
            ConsensusMessage::Timeout(t) => Some(t.view),
            ConsensusMessage::NewView(nv) => Some(nv.view),
            ConsensusMessage::Decide(d) => Some(d.view),
        }
    }

    fn extract_qc_from_message(msg: &ConsensusMessage) -> Option<&QuorumCertificate> {
        match msg {
            ConsensusMessage::Proposal(p) => Some(&p.justify_qc),
            ConsensusMessage::Timeout(t) => Some(&t.high_qc),
            ConsensusMessage::Decide(d) => Some(&d.commit_qc),
            ConsensusMessage::PrepareQC(pqc) => Some(&pqc.qc),
            ConsensusMessage::NewView(nv) => Some(&nv.timeout_cert.high_qc),
            ConsensusMessage::Vote(_) | ConsensusMessage::CommitVote(_) => None,
        }
    }

    /// Attempts a QC-based view jump when a far-future message arrives.
    ///
    /// Extracts and verifies the QC from the message. If valid, jumps to
    /// `max(qc.view + 1, msg_view)`. This breaks the deadlock where a recovering
    /// node's view is too old to participate in consensus.
    ///
    /// Safety: A valid QC proves ≥quorum validators reached that view, so
    /// jumping does not violate HotStuff-2 safety. locked_qc only advances.
    fn try_qc_view_jump(&mut self, msg: &ConsensusMessage, msg_view: ViewNumber) -> bool {
        let current_view = self.round_state.current_view();

        // Genesis QC (view 0) has no real signatures — skip it.
        let qc = match Self::extract_qc_from_message(msg) {
            Some(qc) if qc.view > 0 => qc,
            _ => return false,
        };

        // Decide uses commit_signing_message; all others use signing_message.
        let verify_result = if matches!(msg, ConsensusMessage::Decide(_)) {
            super::quorum::verify_commit_qc(qc, self.validator_set())
        } else {
            super::quorum::verify_qc(qc, self.validator_set())
        };
        if verify_result.is_err() {
            tracing::debug!(
                current_view,
                msg_view,
                qc_view = qc.view,
                "QC view jump failed: invalid QC signature"
            );
            return false;
        }

        let target_view = (qc.view + 1).max(msg_view);
        if target_view <= current_view {
            return false;
        }

        tracing::info!(
            current_view,
            target_view,
            qc_view = qc.view,
            msg_view,
            "QC-based view jump: recovering node catching up to network"
        );

        self.round_state.update_locked_qc(qc);
        self.round_state.reset_consecutive_timeouts();

        self.emit(EngineOutput::SyncRequired { local_view: current_view, target_view });
        self.advance_to_view(target_view);

        // Use actual view after advance: buffered-message replay may push view beyond target_view.
        let actual_view = self.round_state.current_view();
        self.emit(EngineOutput::ViewChanged { new_view: actual_view });

        true
    }

    // ── Internal helpers ──

    pub(super) fn advance_to_view(&mut self, new_view: ViewNumber) {
        // Monotonicity guard: never regress the view. This can happen when buffered-message
        // replay triggers a nested advance_to_view (e.g., a replayed Decide).
        if new_view <= self.round_state.current_view() {
            return;
        }

        // Save current PrepareQC for piggybacking into the next Proposal (chained mode).
        self.previous_prepare_qc = self.prepare_qc.take();

        // Check epoch boundary.
        if self.epoch_manager.epochs_enabled()
            && self.epoch_manager.is_epoch_boundary(new_view)
            && self.epoch_manager.advance_epoch()
        {
            let new_epoch = self.epoch_manager.current_epoch();
            let validator_count = self.validator_set().len();
            tracing::info!(new_epoch, validator_count, view = new_view, "epoch transition at view boundary");
            self.emit(EngineOutput::EpochTransition { new_epoch, validator_count });
        }

        self.round_state.advance_view(new_view);
        self.pacemaker.reset_for_view(new_view, self.round_state.consecutive_timeouts());
        self.vote_collector = None;
        self.commit_collector = None;
        self.timeout_collector = None;
        self.prepare_qc = None;
        self.pending_proposal = None;
        self.imported_blocks.clear();
        self.equivocation_tracker.clear();

        // Replay buffered messages for the new view; re-buffer those still in window.
        let drained: Vec<(ViewNumber, ConsensusMessage)> = self.future_msg_buffer.drain(..).collect();
        let mut to_replay = Vec::new();
        for (view, msg) in drained {
            if view == new_view {
                to_replay.push(msg);
            } else if view > new_view && view <= new_view + FUTURE_VIEW_WINDOW {
                self.future_msg_buffer.push((view, msg));
            }
            // else: expired — discard silently
        }

        if !to_replay.is_empty() {
            tracing::debug!(view = new_view, replaying = to_replay.len(), "replaying buffered future-view messages");
        }

        for msg in to_replay {
            if let Err(e) = self.dispatch_message(msg) {
                tracing::debug!(view = new_view, error = %e, "buffered message replay failed");
            }
        }

        tracing::debug!(view = new_view, "advanced to new view");
    }

    pub(super) fn validator_set(&self) -> &ValidatorSet {
        self.epoch_manager.current_validator_set()
    }

    pub(super) fn emit(&self, output: EngineOutput) {
        if self.output_tx.try_send(output).is_err() {
            tracing::error!("consensus output channel closed or full");
        }
    }
}

impl std::fmt::Debug for ConsensusEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConsensusEngine")
            .field("my_index", &self.my_index)
            .field("view", &self.round_state.current_view())
            .field("phase", &self.round_state.phase())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Address;
    use n42_chainspec::ValidatorInfo;
    use n42_primitives::consensus::{CommitVote, Decide, Vote};

    fn make_engine(
        n: usize,
        my_index: u32,
    ) -> (
        ConsensusEngine,
        Vec<n42_primitives::BlsSecretKey>,
        ValidatorSet,
        mpsc::Receiver<EngineOutput>,
    ) {
        let sks: Vec<_> = (0..n)
            .map(|_| n42_primitives::BlsSecretKey::random().unwrap())
            .collect();
        let infos: Vec<_> = sks
            .iter()
            .enumerate()
            .map(|(i, sk)| ValidatorInfo {
                address: Address::with_last_byte(i as u8),
                bls_public_key: sk.public_key(),
            })
            .collect();
        let f = ((n as u32).saturating_sub(1)) / 3;
        let vs = ValidatorSet::new(&infos, f);

        let (output_tx, output_rx) = mpsc::channel(1024);
        let engine = ConsensusEngine::new(
            my_index,
            sks[my_index as usize].clone(),
            vs.clone(),
            60000,
            120000,
            output_tx,
        );

        (engine, sks, vs, output_rx)
    }

    fn build_test_commit_qc(
        view: ViewNumber,
        block_hash: B256,
        sks: &[n42_primitives::BlsSecretKey],
        vs: &ValidatorSet,
        signers: &[u32],
    ) -> QuorumCertificate {
        use crate::protocol::quorum::{VoteCollector, commit_signing_message};
        let mut collector = VoteCollector::new(view, block_hash, vs.len());
        for &i in signers {
            let msg = commit_signing_message(view, &block_hash);
            collector.add_vote(i, sks[i as usize].sign(&msg)).unwrap();
        }
        collector.build_qc_with_message(vs, &commit_signing_message(view, &block_hash)).unwrap()
    }

    fn build_test_prepare_qc(
        view: ViewNumber,
        block_hash: B256,
        sks: &[n42_primitives::BlsSecretKey],
        vs: &ValidatorSet,
        signers: &[u32],
    ) -> QuorumCertificate {
        use crate::protocol::quorum::{VoteCollector, signing_message};
        let mut collector = VoteCollector::new(view, block_hash, vs.len());
        for &i in signers {
            let msg = signing_message(view, &block_hash);
            collector.add_vote(i, sks[i as usize].sign(&msg)).unwrap();
        }
        collector.build_qc(vs).unwrap()
    }

    #[test]
    fn test_engine_creation() {
        let (engine, _, _, _rx) = make_engine(4, 0);
        assert_eq!(engine.current_view(), 1);
        assert_eq!(engine.current_phase(), Phase::WaitingForProposal);
    }

    #[test]
    fn test_engine_is_leader() {
        let (engine, _, _, _rx) = make_engine(4, 1);
        assert!(engine.is_current_leader(), "validator 1 should be leader at view 1");

        let (engine, _, _, _rx) = make_engine(4, 0);
        assert!(!engine.is_current_leader(), "validator 0 should NOT be leader at view 1");
    }

    #[test]
    fn test_engine_debug() {
        let (engine, _, _, _rx) = make_engine(1, 0);
        let debug_str = format!("{:?}", engine);
        assert!(debug_str.contains("ConsensusEngine"));
        assert!(debug_str.contains("my_index"));
    }

    #[test]
    fn test_engine_block_ready_non_leader() {
        let (mut engine, _, _, _rx) = make_engine(4, 0);
        engine
            .process_event(ConsensusEvent::BlockReady(B256::repeat_byte(0xAA)))
            .expect("non-leader block ready should succeed");
        assert_eq!(engine.current_phase(), Phase::WaitingForProposal);
    }

    #[test]
    fn test_engine_block_ready_as_leader() {
        let (mut engine, _, _, mut rx) = make_engine(1, 0);
        let block_hash = B256::repeat_byte(0xBB);

        engine
            .process_event(ConsensusEvent::BlockReady(block_hash))
            .expect("leader block ready should succeed");

        // Single-validator: consensus completes in one call, advancing to view 2.
        assert_eq!(engine.current_view(), 2);
        assert_eq!(engine.current_phase(), Phase::WaitingForProposal);

        let mut outputs = vec![];
        while let Ok(output) = rx.try_recv() {
            outputs.push(output);
        }

        assert!(outputs.iter().any(|o| matches!(o, EngineOutput::BroadcastMessage(ConsensusMessage::Proposal(_)))));
        assert!(outputs.iter().any(|o| matches!(o, EngineOutput::BroadcastMessage(ConsensusMessage::PrepareQC(_)))));
        assert!(outputs.iter().any(|o| matches!(o, EngineOutput::BroadcastMessage(ConsensusMessage::Decide(_)))));
        assert!(outputs.iter().any(|o| matches!(o, EngineOutput::BlockCommitted { block_hash: h, .. } if *h == block_hash)));
    }

    #[test]
    fn test_engine_proposal_wrong_view() {
        let (mut engine, sks, _, _rx) = make_engine(4, 0);

        use crate::protocol::quorum::signing_message;
        let msg = signing_message(0, &B256::repeat_byte(0xCC));
        let sig = sks[1].sign(&msg);
        let proposal = n42_primitives::consensus::Proposal {
            view: 0,
            block_hash: B256::repeat_byte(0xCC),
            justify_qc: QuorumCertificate::genesis(),
            proposer: 1,
            signature: sig,
            prepare_qc: None,
        };

        // Stale proposals are silently discarded by the pre-filter.
        let result = engine.process_event(ConsensusEvent::Message(ConsensusMessage::Proposal(proposal)));
        assert!(result.is_ok());
    }

    #[test]
    fn test_engine_proposal_wrong_proposer() {
        use crate::protocol::quorum::signing_message;
        let (mut engine, sks, _, _rx) = make_engine(4, 0);

        let msg = signing_message(1, &B256::repeat_byte(0xDD));
        let sig = sks[0].sign(&msg);
        let proposal = n42_primitives::consensus::Proposal {
            view: 1,
            block_hash: B256::repeat_byte(0xDD),
            justify_qc: QuorumCertificate::genesis(),
            proposer: 0,
            signature: sig,
            prepare_qc: None,
        };

        let result = engine.process_event(ConsensusEvent::Message(ConsensusMessage::Proposal(proposal)));
        assert!(result.is_err());
        match result.unwrap_err() {
            crate::error::ConsensusError::InvalidProposer { expected, actual, .. } => {
                assert_eq!(expected, 1);
                assert_eq!(actual, 0);
            }
            other => panic!("expected InvalidProposer, got: {:?}", other),
        }
    }

    #[test]
    fn test_engine_valid_proposal_defers_vote() {
        use crate::protocol::quorum::signing_message;
        let (mut engine, sks, _, mut rx) = make_engine(4, 0);

        let block_hash = B256::repeat_byte(0xEE);
        let msg = signing_message(1, &block_hash);
        let proposal = n42_primitives::consensus::Proposal {
            view: 1,
            block_hash,
            justify_qc: QuorumCertificate::genesis(),
            proposer: 1,
            signature: sks[1].sign(&msg),
            prepare_qc: None,
        };

        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Proposal(proposal)))
            .expect("valid proposal should be accepted");

        assert_eq!(engine.current_phase(), Phase::Voting);

        let mut outputs = vec![];
        while let Ok(output) = rx.try_recv() {
            outputs.push(output);
        }

        assert!(outputs.iter().any(|o| matches!(o, EngineOutput::ExecuteBlock(h) if *h == block_hash)));
        assert!(!outputs.iter().any(|o| matches!(o, EngineOutput::SendToValidator(_, ConsensusMessage::Vote(_)))));
        assert!(engine.pending_proposal.is_some());

        // BlockImported triggers the deferred vote.
        engine
            .process_event(ConsensusEvent::BlockImported(block_hash))
            .expect("BlockImported should succeed");

        let mut outputs = vec![];
        while let Ok(output) = rx.try_recv() {
            outputs.push(output);
        }

        assert!(outputs.iter().any(|o| matches!(o, EngineOutput::SendToValidator(_, ConsensusMessage::Vote(_)))));
        assert!(engine.pending_proposal.is_none());
    }

    #[test]
    fn test_engine_block_imported_before_proposal() {
        use crate::protocol::quorum::signing_message;
        let (mut engine, sks, _, mut rx) = make_engine(4, 0);
        let block_hash = B256::repeat_byte(0xEE);

        engine
            .process_event(ConsensusEvent::BlockImported(block_hash))
            .expect("BlockImported before proposal should succeed");

        assert!(engine.imported_blocks.contains(&block_hash));

        let msg = signing_message(1, &block_hash);
        let proposal = n42_primitives::consensus::Proposal {
            view: 1,
            block_hash,
            justify_qc: QuorumCertificate::genesis(),
            proposer: 1,
            signature: sks[1].sign(&msg),
            prepare_qc: None,
        };

        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Proposal(proposal)))
            .expect("valid proposal should be accepted");

        let mut outputs = vec![];
        while let Ok(output) = rx.try_recv() {
            outputs.push(output);
        }

        assert!(outputs.iter().any(|o| matches!(o, EngineOutput::SendToValidator(_, ConsensusMessage::Vote(_)))));
        assert!(engine.pending_proposal.is_none());
        assert!(!engine.imported_blocks.contains(&block_hash));
    }

    #[test]
    fn test_engine_timeout_clears_pending_proposal() {
        use crate::protocol::quorum::signing_message;
        let (mut engine, sks, _, mut rx) = make_engine(4, 0);
        let block_hash = B256::repeat_byte(0xEE);

        let msg = signing_message(1, &block_hash);
        let proposal = n42_primitives::consensus::Proposal {
            view: 1,
            block_hash,
            justify_qc: QuorumCertificate::genesis(),
            proposer: 1,
            signature: sks[1].sign(&msg),
            prepare_qc: None,
        };
        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Proposal(proposal)))
            .expect("proposal should succeed");
        while rx.try_recv().is_ok() {}

        assert!(engine.pending_proposal.is_some());

        engine.on_timeout().expect("timeout should succeed");
        assert!(engine.pending_proposal.is_none());
    }

    #[test]
    fn test_engine_timeout() {
        let (mut engine, _, _, mut rx) = make_engine(4, 0);

        engine.on_timeout().expect("timeout should succeed");

        assert_eq!(engine.current_phase(), Phase::TimedOut);
        let output = rx.try_recv().expect("should have timeout output");
        assert!(matches!(output, EngineOutput::BroadcastMessage(ConsensusMessage::Timeout(_))));
    }

    #[test]
    fn test_engine_vote_non_leader_ignored() {
        use crate::protocol::quorum::signing_message;
        let (mut engine, sks, _, _rx) = make_engine(4, 0);

        let msg = signing_message(1, &B256::repeat_byte(0xAA));
        let vote = Vote {
            view: 1,
            block_hash: B256::repeat_byte(0xAA),
            voter: 1,
            signature: sks[1].sign(&msg),
        };

        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Vote(vote)))
            .expect("vote to non-leader should not error");
    }

    #[test]
    fn test_engine_pacemaker_accessible() {
        let (engine, _, _, _rx) = make_engine(1, 0);
        assert!(!engine.pacemaker().is_timed_out());
    }

    #[test]
    fn test_full_consensus_4_validators() {
        use crate::protocol::quorum::{commit_signing_message, signing_message};

        let (mut engine, sks, _vs, mut rx) = make_engine(4, 1);
        let block_hash = B256::repeat_byte(0xF1);
        let view = 1u64;

        engine
            .process_event(ConsensusEvent::BlockReady(block_hash))
            .expect("block ready should succeed");

        assert_eq!(engine.current_phase(), Phase::Voting);
        while rx.try_recv().is_ok() {}

        // Two more validators vote (leader already has 1 self-vote).
        for i in [0u32, 2] {
            let msg = signing_message(view, &block_hash);
            let vote = Vote { view, block_hash, voter: i, signature: sks[i as usize].sign(&msg) };
            engine
                .process_event(ConsensusEvent::Message(ConsensusMessage::Vote(vote)))
                .expect("vote should succeed");
        }

        let mut outputs = vec![];
        while let Ok(o) = rx.try_recv() { outputs.push(o); }
        assert!(outputs.iter().any(|o| matches!(o, EngineOutput::BroadcastMessage(ConsensusMessage::PrepareQC(_)))));

        // Two commit votes (leader already self-voted for Round 2).
        for i in [0u32, 2] {
            let msg = commit_signing_message(view, &block_hash);
            let cv = CommitVote { view, block_hash, voter: i, signature: sks[i as usize].sign(&msg) };
            engine
                .process_event(ConsensusEvent::Message(ConsensusMessage::CommitVote(cv)))
                .expect("commit vote should succeed");
        }

        assert_eq!(engine.current_view(), 2);
        assert_eq!(engine.current_phase(), Phase::WaitingForProposal);

        outputs.clear();
        while let Ok(o) = rx.try_recv() { outputs.push(o); }
        assert!(outputs.iter().any(|o| matches!(o, EngineOutput::BroadcastMessage(ConsensusMessage::Decide(_)))));
        assert!(outputs.iter().any(|o| matches!(o, EngineOutput::BlockCommitted { block_hash: h, view: v, .. } if *h == block_hash && *v == view)));
    }

    #[test]
    fn test_validator_receives_prepare_qc() {
        use crate::protocol::quorum::{VoteCollector, signing_message};

        let (mut engine, sks, vs, mut rx) = make_engine(4, 0);
        let block_hash = B256::repeat_byte(0xF2);
        let view = 1u64;

        let prop_msg = signing_message(view, &block_hash);
        let proposal = n42_primitives::consensus::Proposal {
            view,
            block_hash,
            justify_qc: QuorumCertificate::genesis(),
            proposer: 1,
            signature: sks[1].sign(&prop_msg),
            prepare_qc: None,
        };
        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Proposal(proposal)))
            .expect("proposal should succeed");
        engine
            .process_event(ConsensusEvent::BlockImported(block_hash))
            .expect("BlockImported should succeed");
        while rx.try_recv().is_ok() {}

        let mut collector = VoteCollector::new(view, block_hash, vs.len());
        for i in 0..3u32 {
            let msg = signing_message(view, &block_hash);
            collector.add_vote(i, sks[i as usize].sign(&msg)).unwrap();
        }
        let qc = collector.build_qc(&vs).unwrap();

        let prepare_qc = n42_primitives::consensus::PrepareQC { view, block_hash, qc };
        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::PrepareQC(prepare_qc)))
            .expect("PrepareQC should succeed");

        assert_eq!(engine.current_phase(), Phase::PreCommit);

        let mut outputs = vec![];
        while let Ok(o) = rx.try_recv() { outputs.push(o); }
        let has_commit_vote = outputs.iter().any(|o| matches!(
            o,
            EngineOutput::SendToValidator(1, ConsensusMessage::CommitVote(cv))
            if cv.view == view && cv.block_hash == block_hash
        ));
        assert!(has_commit_vote, "validator should send CommitVote to leader");
    }

    #[test]
    fn test_process_vote_rejects_invalid_signature() {
        use crate::protocol::quorum::signing_message;

        let (mut engine, sks, _, mut rx) = make_engine(4, 1);
        let block_hash = B256::repeat_byte(0xA1);
        let view = 1u64;

        engine
            .process_event(ConsensusEvent::BlockReady(block_hash))
            .expect("block ready should succeed");
        while rx.try_recv().is_ok() {}

        let wrong_msg = signing_message(99, &block_hash);
        let vote = Vote {
            view,
            block_hash,
            voter: 0,
            signature: sks[0].sign(&wrong_msg),
        };

        let result = engine.process_event(ConsensusEvent::Message(ConsensusMessage::Vote(vote)));
        assert!(result.is_err());
        match result.unwrap_err() {
            crate::error::ConsensusError::InvalidSignature { view: v, validator_index } => {
                assert_eq!(v, view);
                assert_eq!(validator_index, 0);
            }
            other => panic!("expected InvalidSignature, got: {:?}", other),
        }
    }

    #[test]
    fn test_process_vote_ignores_wrong_block() {
        use crate::protocol::quorum::signing_message;

        let (mut engine, sks, _, mut rx) = make_engine(4, 1);
        let block_hash = B256::repeat_byte(0xA2);
        let wrong_block = B256::repeat_byte(0xFF);
        let view = 1u64;

        engine
            .process_event(ConsensusEvent::BlockReady(block_hash))
            .expect("block ready should succeed");
        while rx.try_recv().is_ok() {}

        let msg = signing_message(view, &wrong_block);
        let vote = Vote {
            view,
            block_hash: wrong_block,
            voter: 0,
            signature: sks[0].sign(&msg),
        };

        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Vote(vote)))
            .expect("vote for wrong block should be silently ignored");

        assert_eq!(engine.current_phase(), Phase::Voting);
    }

    #[test]
    fn test_process_commit_vote_rejects_invalid_signature() {
        use crate::protocol::quorum::{commit_signing_message, signing_message};

        let (mut engine, sks, _vs, mut rx) = make_engine(4, 1);
        let block_hash = B256::repeat_byte(0xA3);
        let view = 1u64;

        engine
            .process_event(ConsensusEvent::BlockReady(block_hash))
            .expect("block ready");
        while rx.try_recv().is_ok() {}

        for i in [0u32, 2] {
            let msg = signing_message(view, &block_hash);
            let vote = Vote { view, block_hash, voter: i, signature: sks[i as usize].sign(&msg) };
            engine
                .process_event(ConsensusEvent::Message(ConsensusMessage::Vote(vote)))
                .expect("vote should succeed");
        }
        while rx.try_recv().is_ok() {}

        let wrong_msg = commit_signing_message(99, &block_hash);
        let cv = CommitVote {
            view,
            block_hash,
            voter: 0,
            signature: sks[0].sign(&wrong_msg),
        };

        let result = engine.process_event(ConsensusEvent::Message(ConsensusMessage::CommitVote(cv)));
        assert!(result.is_err());
        match result.unwrap_err() {
            crate::error::ConsensusError::InvalidSignature { view: v, validator_index } => {
                assert_eq!(v, view);
                assert_eq!(validator_index, 0);
            }
            other => panic!("expected InvalidSignature, got: {:?}", other),
        }
    }

    #[test]
    fn test_process_timeout_rejects_invalid_signature() {
        use crate::protocol::quorum::timeout_signing_message;
        use n42_primitives::consensus::TimeoutMessage;

        let (mut engine, sks, _, mut rx) = make_engine(4, 0);
        let view = 1u64;

        engine.on_timeout().expect("timeout should succeed");
        while rx.try_recv().is_ok() {}

        let wrong_msg = timeout_signing_message(999);
        let timeout = TimeoutMessage {
            view,
            high_qc: QuorumCertificate::genesis(),
            sender: 2,
            signature: sks[2].sign(&wrong_msg),
        };

        let result = engine.process_event(ConsensusEvent::Message(ConsensusMessage::Timeout(timeout)));
        assert!(result.is_err());
        match result.unwrap_err() {
            crate::error::ConsensusError::InvalidSignature { view: v, validator_index } => {
                assert_eq!(v, view);
                assert_eq!(validator_index, 2);
            }
            other => panic!("expected InvalidSignature, got: {:?}", other),
        }
    }

    #[test]
    fn test_consensus_succeeds_despite_invalid_votes() {
        use crate::protocol::quorum::{commit_signing_message, signing_message};

        let (mut engine, sks, _vs, mut rx) = make_engine(4, 1);
        let block_hash = B256::repeat_byte(0xA5);
        let view = 1u64;

        engine
            .process_event(ConsensusEvent::BlockReady(block_hash))
            .expect("block ready");
        while rx.try_recv().is_ok() {}

        let msg = signing_message(view, &block_hash);
        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Vote(Vote {
                view,
                block_hash,
                voter: 0,
                signature: sks[0].sign(&msg),
            })))
            .expect("valid vote from 0");

        // Invalid vote from validator 3
        let bad_msg = signing_message(99, &block_hash);
        let result = engine.process_event(ConsensusEvent::Message(ConsensusMessage::Vote(Vote {
            view,
            block_hash,
            voter: 3,
            signature: sks[3].sign(&bad_msg),
        })));
        assert!(result.is_err());

        // Valid vote from validator 2 → quorum (leader + 0 + 2 = 3)
        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Vote(Vote {
                view,
                block_hash,
                voter: 2,
                signature: sks[2].sign(&msg),
            })))
            .expect("valid vote from 2");

        let mut outputs = vec![];
        while let Ok(o) = rx.try_recv() { outputs.push(o); }
        assert!(outputs.iter().any(|o| matches!(o, EngineOutput::BroadcastMessage(ConsensusMessage::PrepareQC(_)))));

        for i in [0u32, 2] {
            let msg = commit_signing_message(view, &block_hash);
            let cv = CommitVote { view, block_hash, voter: i, signature: sks[i as usize].sign(&msg) };
            engine
                .process_event(ConsensusEvent::Message(ConsensusMessage::CommitVote(cv)))
                .expect("commit vote should succeed");
        }

        assert_eq!(engine.current_view(), 2);
        let mut outputs = vec![];
        while let Ok(o) = rx.try_recv() { outputs.push(o); }
        assert!(outputs.iter().any(|o| matches!(o, EngineOutput::BroadcastMessage(ConsensusMessage::Decide(_)))));
        assert!(outputs.iter().any(|o| matches!(o, EngineOutput::BlockCommitted { .. })));
    }

    #[test]
    fn test_decide_advances_follower() {
        let (mut engine, sks, vs, mut rx) = make_engine(4, 0);
        let block_hash = B256::repeat_byte(0xD1);
        let view = 1u64;

        let commit_qc = build_test_commit_qc(view, block_hash, &sks, &vs, &[0, 1, 2]);
        let decide = Decide { view, block_hash, commit_qc };

        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Decide(decide)))
            .expect("valid Decide should succeed");

        assert_eq!(engine.current_view(), 2);
        assert_eq!(engine.current_phase(), Phase::WaitingForProposal);

        let mut outputs = vec![];
        while let Ok(o) = rx.try_recv() { outputs.push(o); }
        let has_committed = outputs.iter().any(|o| matches!(
            o,
            EngineOutput::BlockCommitted { block_hash: h, view: v, .. }
            if *h == block_hash && *v == view
        ));
        assert!(has_committed);
    }

    #[test]
    fn test_decide_stale_ignored() {
        let (mut engine, sks, vs, mut rx) = make_engine(4, 0);
        let block_hash = B256::repeat_byte(0xD2);

        for v in 1u64..=4 {
            let bh = B256::repeat_byte(v as u8);
            let cqc = build_test_commit_qc(v, bh, &sks, &vs, &[0, 1, 2]);
            let _ = engine.process_event(ConsensusEvent::Message(
                ConsensusMessage::Decide(Decide { view: v, block_hash: bh, commit_qc: cqc }),
            ));
            while rx.try_recv().is_ok() {}
        }
        assert_eq!(engine.current_view(), 5);

        let commit_qc = build_test_commit_qc(3, block_hash, &sks, &vs, &[0, 1, 2]);
        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Decide(Decide {
                view: 3,
                block_hash,
                commit_qc,
            })))
            .expect("stale Decide should be ignored without error");

        assert_eq!(engine.current_view(), 5);
    }

    #[test]
    fn test_decide_invalid_qc_rejected() {
        use bitvec::prelude::*;

        let (mut engine, sks, _vs, _rx) = make_engine(4, 0);
        let block_hash = B256::repeat_byte(0xD3);
        let view = 1u64;

        let mut signers = bitvec![u8, Msb0; 0; 4];
        signers.set(0, true);
        signers.set(1, true);

        let weak_qc = QuorumCertificate {
            view,
            block_hash,
            aggregate_signature: sks[0].sign(b"dummy"),
            signers,
        };

        let result = engine.process_event(ConsensusEvent::Message(
            ConsensusMessage::Decide(Decide { view, block_hash, commit_qc: weak_qc }),
        ));
        assert!(result.is_err());
        match result.unwrap_err() {
            crate::error::ConsensusError::InsufficientVotes { have, need, .. } => {
                assert_eq!(have, 2);
                assert_eq!(need, 3);
            }
            other => panic!("expected InsufficientVotes, got: {:?}", other),
        }
    }

    #[test]
    fn test_decide_rejects_forged_signature() {
        use bitvec::prelude::*;
        use crate::protocol::quorum::signing_message;

        let (mut engine, sks, _vs, _rx) = make_engine(4, 0);
        let block_hash = B256::repeat_byte(0xD4);
        let view = 1u64;

        let wrong_msg = signing_message(view, &block_hash);
        let sigs: Vec<_> = (0..3u32).map(|i| sks[i as usize].sign(&wrong_msg)).collect();
        let sig_refs: Vec<_> = sigs.iter().collect();
        let agg_sig = n42_primitives::bls::AggregateSignature::aggregate(&sig_refs).unwrap();

        let mut signers = bitvec![u8, Msb0; 0; 4];
        signers.set(0, true);
        signers.set(1, true);
        signers.set(2, true);

        let forged_qc = QuorumCertificate { view, block_hash, aggregate_signature: agg_sig, signers };
        let result = engine.process_event(ConsensusEvent::Message(
            ConsensusMessage::Decide(Decide { view, block_hash, commit_qc: forged_qc }),
        ));
        assert!(result.is_err());
        match result.unwrap_err() {
            crate::error::ConsensusError::InvalidQC { view: v, reason } => {
                assert_eq!(v, view);
                assert!(reason.contains("commit QC"), "got: {reason}");
            }
            other => panic!("expected InvalidQC, got: {:?}", other),
        }
    }

    #[test]
    fn test_block_imported_hash_mismatch_caches_and_restores() {
        use crate::protocol::quorum::signing_message;

        let (mut engine, sks, _, mut rx) = make_engine(4, 0);
        let proposal_hash = B256::repeat_byte(0xAA);
        let other_hash = B256::repeat_byte(0xBB);

        let msg = signing_message(1, &proposal_hash);
        let proposal = n42_primitives::consensus::Proposal {
            view: 1,
            block_hash: proposal_hash,
            justify_qc: QuorumCertificate::genesis(),
            proposer: 1,
            signature: sks[1].sign(&msg),
            prepare_qc: None,
        };
        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Proposal(proposal)))
            .expect("proposal should succeed");
        while rx.try_recv().is_ok() {}
        assert!(engine.pending_proposal.is_some());

        engine
            .process_event(ConsensusEvent::BlockImported(other_hash))
            .expect("mismatched BlockImported should succeed");

        assert!(engine.pending_proposal.is_some());
        assert_eq!(engine.pending_proposal.as_ref().unwrap().block_hash, proposal_hash);
        assert!(engine.imported_blocks.contains(&other_hash));
    }

    #[test]
    fn test_imported_blocks_capacity_bound() {
        let (mut engine, _sks, _, _rx) = make_engine(4, 0);

        for i in 0..32u8 {
            engine
                .process_event(ConsensusEvent::BlockImported(B256::repeat_byte(i)))
                .expect("BlockImported should succeed");
        }
        assert_eq!(engine.imported_blocks.len(), 32);

        engine
            .process_event(ConsensusEvent::BlockImported(B256::repeat_byte(0xFF)))
            .expect("BlockImported at capacity should succeed");
        assert_eq!(engine.imported_blocks.len(), 32);
        assert!(!engine.imported_blocks.contains(&B256::repeat_byte(0xFF)));
    }

    #[test]
    fn test_far_future_proposal_triggers_view_jump() {
        let (mut engine, sks, vs, mut rx) = make_engine(4, 0);
        assert_eq!(engine.current_view(), 1);

        let far_view = 100u64;
        let block_hash = B256::repeat_byte(0xF0);
        let justify_qc = build_test_prepare_qc(99, B256::repeat_byte(0xEF), &sks, &vs, &[0, 1, 2]);

        use crate::protocol::quorum::signing_message;
        let msg = signing_message(far_view, &block_hash);
        let proposal = n42_primitives::consensus::Proposal {
            view: far_view,
            block_hash,
            justify_qc,
            proposer: 0,
            signature: sks[0].sign(&msg),
            prepare_qc: None,
        };

        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Proposal(proposal)))
            .expect("far-future proposal with valid QC should trigger view jump");

        assert_eq!(engine.current_view(), far_view);

        let mut outputs = vec![];
        while let Ok(o) = rx.try_recv() { outputs.push(o); }
        assert!(outputs.iter().any(|o| matches!(o, EngineOutput::SyncRequired { local_view: 1, target_view: 100 })));
        assert!(outputs.iter().any(|o| matches!(o, EngineOutput::ViewChanged { new_view: 100 })));
    }

    #[test]
    fn test_far_future_timeout_triggers_view_jump() {
        use crate::protocol::quorum::timeout_signing_message;
        use n42_primitives::consensus::TimeoutMessage;

        let (mut engine, sks, vs, mut rx) = make_engine(4, 0);
        assert_eq!(engine.current_view(), 1);

        let far_view = 200u64;
        let high_qc = build_test_prepare_qc(190, B256::repeat_byte(0xBE), &sks, &vs, &[0, 1, 2]);

        let msg = timeout_signing_message(far_view);
        let timeout = TimeoutMessage {
            view: far_view,
            high_qc,
            sender: 2,
            signature: sks[2].sign(&msg),
        };

        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Timeout(timeout)))
            .expect("far-future timeout with valid QC should trigger view jump");

        assert_eq!(engine.current_view(), 200);
        while rx.try_recv().is_ok() {}
    }

    #[test]
    fn test_far_future_vote_without_qc_is_dropped() {
        use crate::protocol::quorum::signing_message;

        let (mut engine, sks, _, _rx) = make_engine(4, 0);
        assert_eq!(engine.current_view(), 1);

        let far_view = 100u64;
        let block_hash = B256::repeat_byte(0xAA);
        let msg = signing_message(far_view, &block_hash);
        let vote = Vote { view: far_view, block_hash, voter: 2, signature: sks[2].sign(&msg) };

        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Vote(vote)))
            .expect("far-future vote should be silently dropped");

        assert_eq!(engine.current_view(), 1);
    }

    #[test]
    fn test_far_future_invalid_qc_is_dropped() {
        use bitvec::prelude::*;
        use crate::protocol::quorum::signing_message;

        let (mut engine, sks, _, _rx) = make_engine(4, 0);
        assert_eq!(engine.current_view(), 1);

        let far_view = 100u64;
        let block_hash = B256::repeat_byte(0xBB);

        let wrong_msg = signing_message(99, &B256::repeat_byte(0xFF));
        let sigs: Vec<_> = (0..3u32).map(|i| sks[i as usize].sign(&wrong_msg)).collect();
        let sig_refs: Vec<_> = sigs.iter().collect();
        let agg_sig = n42_primitives::bls::AggregateSignature::aggregate(&sig_refs).unwrap();

        let mut signers = bitvec![u8, Msb0; 0; 4];
        signers.set(0, true);
        signers.set(1, true);
        signers.set(2, true);

        let forged_qc = QuorumCertificate {
            view: 99,
            block_hash: B256::repeat_byte(0xEE),
            aggregate_signature: agg_sig,
            signers,
        };

        let msg = signing_message(far_view, &block_hash);
        let proposal = n42_primitives::consensus::Proposal {
            view: far_view,
            block_hash,
            justify_qc: forged_qc,
            proposer: 0,
            signature: sks[0].sign(&msg),
            prepare_qc: None,
        };

        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Proposal(proposal)))
            .expect("far-future proposal with invalid QC should be silently dropped");

        assert_eq!(engine.current_view(), 1);
    }

    #[test]
    fn test_far_future_genesis_qc_is_dropped() {
        use crate::protocol::quorum::signing_message;

        let (mut engine, sks, _, _rx) = make_engine(4, 0);
        let far_view = 100u64;
        let block_hash = B256::repeat_byte(0xCC);

        let msg = signing_message(far_view, &block_hash);
        let proposal = n42_primitives::consensus::Proposal {
            view: far_view,
            block_hash,
            justify_qc: QuorumCertificate::genesis(),
            proposer: 0,
            signature: sks[0].sign(&msg),
            prepare_qc: None,
        };

        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Proposal(proposal)))
            .expect("far-future proposal with genesis QC should be silently dropped");

        assert_eq!(engine.current_view(), 1);
    }

    #[test]
    fn test_view_jump_resets_consecutive_timeouts() {
        use crate::protocol::quorum::signing_message;

        let (mut engine, sks, vs, mut rx) = make_engine(4, 0);

        engine.on_timeout().expect("timeout");
        while rx.try_recv().is_ok() {}
        assert!(engine.consecutive_timeouts() >= 1);

        let far_view = 100u64;
        let block_hash = B256::repeat_byte(0xDD);
        let justify_qc = build_test_prepare_qc(99, B256::repeat_byte(0xDE), &sks, &vs, &[0, 1, 2]);

        let msg = signing_message(far_view, &block_hash);
        let proposal = n42_primitives::consensus::Proposal {
            view: far_view,
            block_hash,
            justify_qc,
            proposer: 0,
            signature: sks[0].sign(&msg),
            prepare_qc: None,
        };

        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Proposal(proposal)))
            .expect("should trigger view jump");

        assert_eq!(engine.current_view(), 100);
        assert_eq!(engine.consecutive_timeouts(), 0);
    }

    #[test]
    fn test_view_jump_updates_locked_qc() {
        use crate::protocol::quorum::signing_message;

        let (mut engine, sks, vs, mut rx) = make_engine(4, 0);
        assert_eq!(engine.locked_qc().view, 0);

        let far_view = 100u64;
        let block_hash = B256::repeat_byte(0xEE);
        let justify_qc = build_test_prepare_qc(95, B256::repeat_byte(0x95), &sks, &vs, &[0, 1, 2]);

        let msg = signing_message(far_view, &block_hash);
        let proposal = n42_primitives::consensus::Proposal {
            view: far_view,
            block_hash,
            justify_qc,
            proposer: 0,
            signature: sks[0].sign(&msg),
            prepare_qc: None,
        };

        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Proposal(proposal)))
            .expect("should trigger view jump");

        while rx.try_recv().is_ok() {}
        assert_eq!(engine.locked_qc().view, 95);
    }

    #[test]
    fn test_view_jump_does_not_downgrade_locked_qc() {
        use crate::protocol::quorum::timeout_signing_message;
        use n42_primitives::consensus::TimeoutMessage;

        let (mut engine, sks, vs, mut rx) = make_engine(4, 0);

        let commit_qc = build_test_commit_qc(1, B256::repeat_byte(0x01), &sks, &vs, &[0, 1, 2]);
        let _ = engine.process_event(ConsensusEvent::Message(ConsensusMessage::Decide(Decide {
            view: 1, block_hash: B256::repeat_byte(0x01), commit_qc,
        })));
        while rx.try_recv().is_ok() {}

        let commit_qc2 = build_test_commit_qc(2, B256::repeat_byte(0x02), &sks, &vs, &[0, 1, 2]);
        let _ = engine.process_event(ConsensusEvent::Message(ConsensusMessage::Decide(Decide {
            view: 2, block_hash: B256::repeat_byte(0x02), commit_qc: commit_qc2,
        })));
        while rx.try_recv().is_ok() {}

        let high_qc = build_test_prepare_qc(50, B256::repeat_byte(0x50), &sks, &vs, &[0, 1, 2]);
        engine.round_state.update_locked_qc(&high_qc);
        assert_eq!(engine.locked_qc().view, 50);

        let far_view = 200u64;
        let low_qc = build_test_prepare_qc(30, B256::repeat_byte(0x30), &sks, &vs, &[0, 1, 2]);
        let msg = timeout_signing_message(far_view);
        let timeout = TimeoutMessage {
            view: far_view,
            high_qc: low_qc,
            sender: 2,
            signature: sks[2].sign(&msg),
        };

        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Timeout(timeout)))
            .expect("should trigger view jump via timeout");

        while rx.try_recv().is_ok() {}
        assert_eq!(engine.current_view(), 200);
        assert_eq!(engine.locked_qc().view, 50, "locked_qc should NOT downgrade from 50 to 30");
    }

    #[test]
    fn test_increased_future_view_window_buffers_50() {
        use crate::protocol::quorum::signing_message;

        let (mut engine, sks, _, _rx) = make_engine(4, 0);
        assert_eq!(engine.current_view(), 1);

        let view = 51u64;
        let block_hash = B256::repeat_byte(0xFF);
        let msg = signing_message(view, &block_hash);
        let vote = Vote { view, block_hash, voter: 2, signature: sks[2].sign(&msg) };

        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Vote(vote)))
            .expect("vote at view 51 should be buffered");

        assert_eq!(engine.future_msg_buffer.len(), 1);
        assert_eq!(engine.future_msg_buffer[0].0, 51);
    }

    #[test]
    fn test_decide_bypasses_view_jump_path() {
        let (mut engine, sks, vs, mut rx) = make_engine(4, 0);
        assert_eq!(engine.current_view(), 1);

        let far_view = 100u64;
        let block_hash = B256::repeat_byte(0xAB);
        let commit_qc = build_test_commit_qc(far_view, block_hash, &sks, &vs, &[0, 1, 2]);
        let decide = Decide { view: far_view, block_hash, commit_qc };

        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Decide(decide)))
            .expect("Decide should use direct bypass path");

        assert_eq!(engine.current_view(), 101);

        let mut outputs = vec![];
        while let Ok(o) = rx.try_recv() { outputs.push(o); }
        assert!(outputs.iter().any(|o| matches!(o, EngineOutput::SyncRequired { .. })));
        assert!(outputs.iter().any(|o| matches!(o, EngineOutput::BlockCommitted { .. })));
    }

    #[test]
    fn test_on_timeout_repeat_is_noop() {
        let (mut engine, _, _, mut rx) = make_engine(4, 0);

        engine.on_timeout().expect("first timeout");
        let t1 = engine.consecutive_timeouts();
        let mut outputs = vec![];
        while let Ok(o) = rx.try_recv() { outputs.push(o); }
        let broadcast_count_1 = outputs.iter()
            .filter(|o| matches!(o, EngineOutput::BroadcastMessage(ConsensusMessage::Timeout(_))))
            .count();
        assert_eq!(broadcast_count_1, 1);

        engine.on_timeout().expect("repeat timeout");
        assert_eq!(engine.consecutive_timeouts(), t1);
        let mut outputs = vec![];
        while let Ok(o) = rx.try_recv() { outputs.push(o); }
        let broadcast_count_2 = outputs.iter()
            .filter(|o| matches!(o, EngineOutput::BroadcastMessage(ConsensusMessage::Timeout(_))))
            .count();
        assert_eq!(broadcast_count_2, 0);
    }

    #[test]
    fn test_process_timeout_at_window_boundary() {
        use crate::protocol::quorum::timeout_signing_message;
        use n42_primitives::consensus::TimeoutMessage;

        let (mut engine, sks, _, mut rx) = make_engine(4, 0);
        assert_eq!(engine.current_view(), 1);

        let boundary_view = 1 + FUTURE_VIEW_WINDOW;
        let msg = timeout_signing_message(boundary_view);
        let timeout = TimeoutMessage {
            view: boundary_view,
            high_qc: QuorumCertificate::genesis(),
            sender: 2,
            signature: sks[2].sign(&msg),
        };

        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Timeout(timeout)))
            .expect("timeout at window boundary should be accepted");

        assert_eq!(engine.current_view(), boundary_view);
        while rx.try_recv().is_ok() {}
    }

    #[test]
    fn test_process_timeout_beyond_window_dropped() {
        use crate::protocol::quorum::timeout_signing_message;
        use n42_primitives::consensus::TimeoutMessage;

        let (mut engine, sks, _, _rx) = make_engine(4, 0);
        assert_eq!(engine.current_view(), 1);

        let beyond_view = 1 + FUTURE_VIEW_WINDOW + 1;
        let msg = timeout_signing_message(beyond_view);
        let timeout = TimeoutMessage {
            view: beyond_view,
            high_qc: QuorumCertificate::genesis(),
            sender: 2,
            signature: sks[2].sign(&msg),
        };

        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Timeout(timeout)))
            .expect("timeout beyond window should be silently dropped");

        assert_eq!(engine.current_view(), 1);
    }

    #[test]
    fn test_tc_formation_from_external_timeouts() {
        use crate::protocol::quorum::timeout_signing_message;
        use n42_primitives::consensus::TimeoutMessage;

        // View 1 leader = 1, next leader (view 2) = 2. Run on node 2.
        let (mut engine, sks, _, mut rx) = make_engine(4, 2);
        assert_eq!(engine.current_view(), 1);

        engine.on_timeout().expect("own timeout");
        while rx.try_recv().is_ok() {}

        let msg = timeout_signing_message(1);
        let timeout0 = TimeoutMessage {
            view: 1,
            high_qc: QuorumCertificate::genesis(),
            sender: 0,
            signature: sks[0].sign(&msg),
        };
        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Timeout(timeout0)))
            .expect("timeout from 0");

        let timeout1 = TimeoutMessage {
            view: 1,
            high_qc: QuorumCertificate::genesis(),
            sender: 1,
            signature: sks[1].sign(&msg),
        };
        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Timeout(timeout1)))
            .expect("timeout from 1 should form TC");

        let mut outputs = vec![];
        while let Ok(o) = rx.try_recv() { outputs.push(o); }
        assert!(outputs.iter().any(|o| matches!(o, EngineOutput::BroadcastMessage(ConsensusMessage::NewView(_)))));
        assert_eq!(engine.current_view(), 2);
    }

    #[test]
    fn test_tc_non_leader_no_newview() {
        use crate::protocol::quorum::timeout_signing_message;
        use n42_primitives::consensus::TimeoutMessage;

        // Next leader (view 2) = 2. Run on node 0 — NOT the next leader.
        let (mut engine, sks, _, mut rx) = make_engine(4, 0);

        engine.on_timeout().expect("own timeout");
        while rx.try_recv().is_ok() {}

        let msg = timeout_signing_message(1);
        for &i in &[1u32, 2] {
            let timeout = TimeoutMessage {
                view: 1,
                high_qc: QuorumCertificate::genesis(),
                sender: i,
                signature: sks[i as usize].sign(&msg),
            };
            engine
                .process_event(ConsensusEvent::Message(ConsensusMessage::Timeout(timeout)))
                .expect("timeout should succeed");
        }

        let mut outputs = vec![];
        while let Ok(o) = rx.try_recv() { outputs.push(o); }
        assert!(!outputs.iter().any(|o| matches!(o, EngineOutput::BroadcastMessage(ConsensusMessage::NewView(_)))));
        assert_eq!(engine.current_view(), 1);
    }

    #[test]
    fn test_advance_to_view_replays_buffered() {
        use crate::protocol::quorum::signing_message;

        let (mut engine, sks, vs, mut rx) = make_engine(4, 0);
        assert_eq!(engine.current_view(), 1);

        let view5_hash = B256::repeat_byte(0x55);
        let msg = signing_message(5, &view5_hash);
        let vote = Vote { view: 5, block_hash: view5_hash, voter: 2, signature: sks[2].sign(&msg) };
        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Vote(vote)))
            .expect("future Vote should be buffered");

        assert_eq!(engine.current_view(), 1);
        assert_eq!(engine.future_msg_buffer.len(), 1);
        assert_eq!(engine.future_msg_buffer[0].0, 5);

        for v in 1u64..=4 {
            let bh = B256::repeat_byte(v as u8);
            let cqc = build_test_commit_qc(v, bh, &sks, &vs, &[0, 1, 2]);
            let _ = engine.process_event(ConsensusEvent::Message(
                ConsensusMessage::Decide(Decide { view: v, block_hash: bh, commit_qc: cqc }),
            ));
            while rx.try_recv().is_ok() {}
        }

        assert_eq!(engine.current_view(), 5);
        assert!(engine.future_msg_buffer.is_empty());
    }

    #[test]
    fn test_advance_to_view_monotonic() {
        let (mut engine, sks, vs, mut rx) = make_engine(4, 0);

        for v in 1u64..=9 {
            let bh = B256::repeat_byte(v as u8);
            let cqc = build_test_commit_qc(v, bh, &sks, &vs, &[0, 1, 2]);
            let _ = engine.process_event(ConsensusEvent::Message(
                ConsensusMessage::Decide(Decide { view: v, block_hash: bh, commit_qc: cqc }),
            ));
        }
        while rx.try_recv().is_ok() {}
        assert_eq!(engine.current_view(), 10);

        engine.advance_to_view(5);
        assert_eq!(engine.current_view(), 10);

        engine.advance_to_view(10);
        assert_eq!(engine.current_view(), 10);

        engine.advance_to_view(15);
        assert_eq!(engine.current_view(), 15);
    }

    #[test]
    fn test_buffer_eviction_removes_lowest_view() {
        use crate::protocol::quorum::signing_message;

        let (mut engine, sks, _, _rx) = make_engine(4, 0);
        assert_eq!(engine.current_view(), 1);

        let voters = [1u32, 2];
        for v in 2u64..=34 {
            for &voter in &voters {
                let block_hash = B256::repeat_byte(v as u8);
                let msg = signing_message(v, &block_hash);
                let vote = Vote { view: v, block_hash, voter, signature: sks[voter as usize].sign(&msg) };
                engine
                    .process_event(ConsensusEvent::Message(ConsensusMessage::Vote(vote)))
                    .expect("buffering should succeed");
            }
        }

        assert_eq!(engine.future_msg_buffer.len(), MAX_FUTURE_MESSAGES);

        let min_view = engine.future_msg_buffer.iter().map(|(v, _)| *v).min().unwrap_or(0);
        assert!(min_view >= 3, "lowest view in buffer should be >= 3 after eviction, got {min_view}");
    }

    #[test]
    fn test_qc_jump_emits_sync_required() {
        use crate::protocol::quorum::signing_message;

        let (mut engine, sks, vs, mut rx) = make_engine(4, 0);
        assert_eq!(engine.current_view(), 1);

        let far_view = 80u64;
        let block_hash = B256::repeat_byte(0xAB);
        let justify_qc = build_test_prepare_qc(79, B256::repeat_byte(0x79), &sks, &vs, &[0, 1, 2]);

        let msg = signing_message(far_view, &block_hash);
        let proposal = n42_primitives::consensus::Proposal {
            view: far_view,
            block_hash,
            justify_qc,
            proposer: 0,
            signature: sks[0].sign(&msg),
            prepare_qc: None,
        };

        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Proposal(proposal)))
            .expect("far-future proposal should trigger QC jump");

        let mut outputs = vec![];
        while let Ok(o) = rx.try_recv() { outputs.push(o); }

        let sync = outputs.iter().find_map(|o| match o {
            EngineOutput::SyncRequired { local_view, target_view } => Some((*local_view, *target_view)),
            _ => None,
        });
        assert!(sync.is_some());
        let (local, target) = sync.unwrap();
        assert_eq!(local, 1);
        assert_eq!(target, 80);
    }

    #[test]
    fn test_stale_view_changed_prevented() {
        use crate::protocol::quorum::signing_message;

        let (mut engine, sks, vs, mut rx) = make_engine(4, 0);
        assert_eq!(engine.current_view(), 1);

        let justify_qc = build_test_prepare_qc(79, B256::repeat_byte(0x79), &sks, &vs, &[0, 1, 2]);
        let msg = signing_message(80, &B256::repeat_byte(0xAB));
        let proposal = n42_primitives::consensus::Proposal {
            view: 80,
            block_hash: B256::repeat_byte(0xAB),
            justify_qc,
            proposer: 0,
            signature: sks[0].sign(&msg),
            prepare_qc: None,
        };

        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Proposal(proposal)))
            .expect("should trigger QC jump to view 80");

        let actual_view = engine.current_view();
        assert!(actual_view >= 80);

        let mut outputs = vec![];
        while let Ok(o) = rx.try_recv() { outputs.push(o); }

        let view_changed_values: Vec<ViewNumber> = outputs.iter().filter_map(|o| match o {
            EngineOutput::ViewChanged { new_view } => Some(*new_view),
            _ => None,
        }).collect();

        assert!(!view_changed_values.is_empty());

        let last_vc = *view_changed_values.last().unwrap();
        assert_eq!(
            last_vc, actual_view,
            "ViewChanged should report actual current_view ({}), got {}",
            actual_view, last_vc
        );
    }
}
