use alloy_primitives::B256;
use n42_primitives::{
    BlsSecretKey,
    consensus::{
        CommitVote, ConsensusMessage, Decide, NewView, PrepareQC, Proposal, QuorumCertificate,
        TimeoutMessage, ViewNumber, Vote,
    },
};
use std::collections::{HashMap, HashSet};
use tokio::sync::mpsc;

use crate::error::{ConsensusError, ConsensusResult};
use crate::validator::{EpochManager, LeaderSelector, ValidatorSet};
use super::pacemaker::Pacemaker;
use super::quorum::{
    VoteCollector, TimeoutCollector,
    signing_message, commit_signing_message, timeout_signing_message,
};
use super::round::{Phase, RoundState};

/// Events fed into the consensus engine.
#[derive(Debug)]
pub enum ConsensusEvent {
    /// A consensus message received from the network.
    Message(ConsensusMessage),
    /// A block has been executed and is ready for proposal.
    /// Contains the block hash.
    BlockReady(B256),
    /// Block data has been imported into the execution layer (follower only).
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
    /// Equivocation detected: a validator voted for two different blocks
    /// in the same view. This is evidence of Byzantine behavior.
    EquivocationDetected {
        view: ViewNumber,
        validator: u32,
        hash1: B256,
        hash2: B256,
    },
    /// An epoch transition has occurred. The validator set may have changed.
    /// Emitted at epoch boundaries when epochs are enabled (epoch_length > 0).
    EpochTransition {
        new_epoch: u64,
        validator_count: u32,
    },
}

/// Pending proposal awaiting block data import before voting (follower path).
#[derive(Debug, Clone)]
struct PendingProposal {
    view: ViewNumber,
    block_hash: B256,
}

/// Maximum number of views ahead a future message can be to be buffered.
/// Messages beyond this window trigger a QC-based view jump attempt.
/// Set to 50 to cover moderate desynchronization (several minutes of timeout progression)
/// without activating the heavier QC jump path.
const FUTURE_VIEW_WINDOW: u64 = 50;

/// Maximum number of buffered future-view messages.
/// When exceeded, the oldest (lowest view) messages are evicted.
const MAX_FUTURE_MESSAGES: usize = 64;

/// The HotStuff-2 consensus engine.
///
/// This is an event-driven state machine that processes consensus messages
/// and produces output actions. It does NOT run its own event loop—the
/// outer node is responsible for driving it via `process_event()` and
/// handling outputs.
///
/// ## Protocol Flow
///
/// ### Optimistic path (2 rounds):
/// 1. **Propose**: Leader broadcasts `Proposal{view, block_hash, justify_qc}`
/// 2. **Vote (Round 1)**: Validators verify & send `Vote{view, block_hash, sig}` to leader
/// 3. **PreCommit**: Leader collects 2f+1 votes → forms QC → broadcasts QC
/// 4. **CommitVote (Round 2)**: Validators send `CommitVote` to leader
/// 5. **Commit**: Leader collects 2f+1 commit votes → block committed
///
/// ### Timeout recovery (Round 3):
/// 1. Timer expires → validator broadcasts `Timeout{view, high_qc}`
/// 2. New leader collects 2f+1 timeouts → forms TC
/// 3. New leader broadcasts `NewView{view+1, TC}` → back to Propose
pub struct ConsensusEngine {
    /// This node's validator index.
    my_index: u32,
    /// This node's BLS secret key.
    secret_key: BlsSecretKey,
    /// Epoch manager for validator set transitions.
    epoch_manager: EpochManager,
    /// Round state tracking.
    round_state: RoundState,
    /// Pacemaker for timeout management.
    pacemaker: Pacemaker,
    /// Vote collector for the current view (Round 1: Prepare).
    vote_collector: Option<VoteCollector>,
    /// Vote collector for the current view (Round 2: Commit).
    commit_collector: Option<VoteCollector>,
    /// Timeout collector for the current view.
    timeout_collector: Option<TimeoutCollector>,
    /// The QC formed from Round 1 votes (used to send to validators).
    prepare_qc: Option<QuorumCertificate>,
    /// Saved PrepareQC from the previous view (chained mode).
    /// Piggybacked into the next Proposal to accelerate QC delivery.
    previous_prepare_qc: Option<QuorumCertificate>,
    /// Output channel for engine actions.
    output_tx: mpsc::Sender<EngineOutput>,
    /// Pending proposal awaiting block data import before voting.
    /// Set when process_proposal() defers the vote (follower path).
    pending_proposal: Option<PendingProposal>,
    /// Block hashes that have been imported but no matching proposal yet.
    /// Handles the case where BlockData arrives before Proposal.
    imported_blocks: HashSet<B256>,
    /// Tracks which block hash each validator voted for in the current view.
    /// Used to detect equivocation (same validator voting for different blocks).
    equivocation_tracker: HashMap<u32, B256>,
    /// Buffer for messages that arrive for future views (within FUTURE_VIEW_WINDOW).
    /// Instead of permanently rejecting slightly early messages, we buffer them
    /// and replay when the engine advances to the target view.
    future_msg_buffer: Vec<(ViewNumber, ConsensusMessage)>,
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
        let pacemaker = Pacemaker::new(base_timeout_ms, max_timeout_ms);

        Self {
            my_index,
            secret_key,
            epoch_manager,
            round_state: RoundState::new(),
            pacemaker,
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
    /// Used at startup when a `consensus_state.json` file is found, restoring
    /// the critical safety invariants (locked_qc, last_committed_qc) so that the
    /// node doesn't vote in ways that violate its previous locking commitments.
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
        let pacemaker = Pacemaker::new(base_timeout_ms, max_timeout_ms);

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
            pacemaker,
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

    /// Returns the current view number.
    pub fn current_view(&self) -> ViewNumber {
        self.round_state.current_view()
    }

    /// Returns the current phase.
    pub fn current_phase(&self) -> Phase {
        self.round_state.phase()
    }

    /// Returns a reference to the pacemaker (for timeout_sleep).
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

    /// Returns the number of validators in the current set.
    pub fn validator_count(&self) -> u32 {
        self.validator_set().len()
    }

    /// Returns a reference to the current validator set (convenience accessor).
    fn validator_set(&self) -> &ValidatorSet {
        self.epoch_manager.current_validator_set()
    }

    /// Returns a reference to the epoch manager.
    pub fn epoch_manager(&self) -> &EpochManager {
        &self.epoch_manager
    }

    /// Returns a mutable reference to the epoch manager.
    pub fn epoch_manager_mut(&mut self) -> &mut EpochManager {
        &mut self.epoch_manager
    }

    /// Checks if this node is the leader for the current view.
    pub fn is_current_leader(&self) -> bool {
        LeaderSelector::is_leader(
            self.my_index,
            self.round_state.current_view(),
            self.validator_set(),
        )
    }

    /// Returns the current locked QC (highest QC seen in a valid proposal).
    pub fn locked_qc(&self) -> &QuorumCertificate {
        self.round_state.locked_qc()
    }

    /// Returns the last committed QC.
    pub fn last_committed_qc(&self) -> &QuorumCertificate {
        self.round_state.last_committed_qc()
    }

    /// Returns the number of consecutive timeouts since the last progress.
    pub fn consecutive_timeouts(&self) -> u32 {
        self.round_state.consecutive_timeouts()
    }

    /// Processes a consensus event and generates outputs.
    pub fn process_event(&mut self, event: ConsensusEvent) -> ConsensusResult<()> {
        match event {
            ConsensusEvent::Message(msg) => self.process_message(msg),
            ConsensusEvent::BlockReady(block_hash) => self.on_block_ready(block_hash),
            ConsensusEvent::BlockImported(block_hash) => self.on_block_imported(block_hash),
        }
    }

    /// Handles a view timeout.
    pub fn on_timeout(&mut self) -> ConsensusResult<()> {
        let view = self.round_state.current_view();
        let already_timed_out = self.round_state.phase() == Phase::TimedOut;

        if already_timed_out {
            tracing::warn!(view, "view timed out (repeat, resetting pacemaker only)");
            // Already timed out in this view: own timeout was already broadcast and
            // added to the collector. Only reset the pacemaker deadline so the
            // orchestrator's select! loop doesn't spin.
            self.pacemaker.reset_for_view(view, self.round_state.consecutive_timeouts());
            return Ok(());
        }

        tracing::warn!(view, "view timed out");
        self.round_state.timeout();

        // Reset pacemaker with exponential backoff BEFORE processing.
        // Without this, the pacemaker deadline stays in the past and the
        // orchestrator's select! loop re-fires immediately, creating a tight
        // timeout loop (hundreds of timeouts per second instead of one per interval).
        // If process_timeout() forms a TC and calls advance_to_view(), the pacemaker
        // will be reset again for the new view — a harmless double reset.
        self.pacemaker.reset_for_view(view, self.round_state.consecutive_timeouts());

        // Clear pending block data state: block data may never arrive.
        // Similar to Tendermint's "prevote nil" — timeout implies giving up on this view.
        self.pending_proposal = None;
        self.imported_blocks.clear();

        // Initialize timeout collector for this view.
        self.timeout_collector =
            Some(TimeoutCollector::new(view, self.validator_set().len()));

        // Send timeout message
        let message = timeout_signing_message(view);
        let signature = self.secret_key.sign(&message);

        let timeout_msg = TimeoutMessage {
            view,
            high_qc: self.round_state.locked_qc().clone(),
            sender: self.my_index,
            signature,
        };

        self.emit(EngineOutput::BroadcastMessage(
            ConsensusMessage::Timeout(timeout_msg.clone()),
        ));

        // Process own timeout message
        self.process_timeout(timeout_msg)
    }

    // ── Message dispatch ──

    fn process_message(&mut self, msg: ConsensusMessage) -> ConsensusResult<()> {
        // Extract the message's view number for future-view buffering.
        let msg_view = Self::message_view(&msg);
        let current_view = self.round_state.current_view();

        // Buffer messages for future views (within window) instead of rejecting.
        // Decide and NewView are always exempt — they handle their own view logic.
        // Timeout messages within FUTURE_VIEW_WINDOW are exempt — process_timeout()
        // handles view sync (advance + TC formation) for these. Timeout messages
        // beyond FUTURE_VIEW_WINDOW go through the QC-based jump path below.
        if let Some(view) = msg_view {
            let is_exempt = matches!(msg, ConsensusMessage::Decide(_) | ConsensusMessage::NewView(_))
                || (matches!(msg, ConsensusMessage::Timeout(_)) && view <= current_view + FUTURE_VIEW_WINDOW);
            if view > current_view && !is_exempt {
                if view <= current_view + FUTURE_VIEW_WINDOW {
                    // Buffer the message for replay when we advance
                    if self.future_msg_buffer.len() >= MAX_FUTURE_MESSAGES {
                        // Evict the oldest (lowest view) entry
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
                // If the message carries a valid QC, jump to catch up with the network.
                // This breaks the deadlock where a recovering node's view is too old
                // to participate in consensus.
                if self.try_qc_view_jump(&msg, view) {
                    // Jump succeeded — re-evaluate the message against the new view
                    let new_current = self.round_state.current_view();
                    if view == new_current {
                        return self.dispatch_message(msg);
                    } else if view > new_current && view <= new_current + FUTURE_VIEW_WINDOW {
                        self.future_msg_buffer.push((view, msg));
                        return Ok(());
                    }
                    return Ok(());
                }
                // QC verification failed or no QC: silently discard
                return Ok(());
            }
        }

        self.dispatch_message(msg)
    }

    /// Extracts the view number from a consensus message (if applicable).
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

    /// Extracts a QC from a consensus message (if it carries one).
    /// Vote and CommitVote do not carry QCs; returns None for those.
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
    /// When a recovering node receives messages beyond FUTURE_VIEW_WINDOW, it
    /// extracts and verifies the QC from the message. If valid, the node jumps
    /// to `max(qc.view + 1, msg_view)`, breaking the deadlock where the node
    /// can't participate in consensus because its view is too old.
    ///
    /// Safety: A valid QC proves ≥quorum validators reached that view, so
    /// jumping does not violate HotStuff-2 safety. locked_qc only advances.
    fn try_qc_view_jump(&mut self, msg: &ConsensusMessage, msg_view: ViewNumber) -> bool {
        let current_view = self.round_state.current_view();

        // Extract QC; genesis QC (view 0) has no real signatures, skip it
        let qc = match Self::extract_qc_from_message(msg) {
            Some(qc) if qc.view > 0 => qc,
            _ => return false,
        };

        // Verify QC signature (Decide uses commit_signing_message, others use signing_message)
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

        // Compute target view: at least qc.view + 1, but msg_view if higher
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

        // Update locked_qc (only advances, never downgrades)
        self.round_state.update_locked_qc(qc);

        // Reset consecutive_timeouts so pacemaker uses base_timeout
        self.round_state.reset_consecutive_timeouts();

        // Request sync for missed blocks, then advance
        self.emit(EngineOutput::SyncRequired {
            local_view: current_view,
            target_view,
        });
        self.advance_to_view(target_view);
        // Use actual current_view after advance: buffered-message replay inside
        // advance_to_view may trigger nested advances (e.g., replayed Decide),
        // pushing the view beyond `target_view`.
        let actual_view = self.round_state.current_view();
        self.emit(EngineOutput::ViewChanged { new_view: actual_view });

        true
    }

    /// Dispatches a consensus message to the appropriate handler.
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

    // ── Proposal handling ──

    /// Called when this node (as leader) has a block ready to propose.
    fn on_block_ready(&mut self, block_hash: B256) -> ConsensusResult<()> {
        let view = self.round_state.current_view();

        if !self.is_current_leader() {
            tracing::debug!(view, "not leader, ignoring block ready");
            return Ok(());
        }

        if self.round_state.phase() != Phase::WaitingForProposal {
            tracing::debug!(view, phase = ?self.round_state.phase(), "not in proposal phase");
            return Ok(());
        }

        let justify_qc = self.round_state.locked_qc().clone();
        let message = signing_message(view, &block_hash);
        let signature = self.secret_key.sign(&message);

        let piggybacked_qc = self.previous_prepare_qc.take();

        let proposal = Proposal {
            view,
            block_hash,
            justify_qc,
            proposer: self.my_index,
            signature,
            prepare_qc: piggybacked_qc.clone(),
        };

        tracing::info!(
            view, %block_hash,
            chained = piggybacked_qc.is_some(),
            "proposing block"
        );

        // Initialize vote collectors for this view
        self.vote_collector =
            Some(VoteCollector::new(view, block_hash, self.validator_set().len()));
        self.commit_collector =
            Some(VoteCollector::new(view, block_hash, self.validator_set().len()));

        self.round_state.enter_voting();

        // Leader self-vote: GossipSub does not deliver messages back to the sender,
        // so the leader must add its own vote to the collector.
        let leader_vote_msg = signing_message(view, &block_hash);
        let leader_vote_sig = self.secret_key.sign(&leader_vote_msg);
        if let Some(ref mut collector) = self.vote_collector {
            let _ = collector.add_vote(self.my_index, leader_vote_sig);
        }

        self.emit(EngineOutput::BroadcastMessage(
            ConsensusMessage::Proposal(proposal),
        ));

        // Check if quorum already reached (single-validator scenario).
        self.try_form_prepare_qc()?;

        Ok(())
    }

    /// Processes a proposal from the leader.
    fn process_proposal(&mut self, proposal: Proposal) -> ConsensusResult<()> {
        let view = self.round_state.current_view();

        // Reject proposals for wrong view
        if proposal.view != view {
            return Err(ConsensusError::ViewMismatch {
                current: view,
                received: proposal.view,
            });
        }

        // Verify proposer is the expected leader
        let expected_leader =
            LeaderSelector::leader_for_view(view, self.validator_set());
        if proposal.proposer != expected_leader {
            return Err(ConsensusError::InvalidProposer {
                view,
                expected: expected_leader,
                actual: proposal.proposer,
            });
        }

        // Verify proposer's signature
        let pk = self.validator_set().get_public_key(proposal.proposer)?;
        let msg = signing_message(view, &proposal.block_hash);
        pk.verify(&msg, &proposal.signature).map_err(|_| {
            ConsensusError::InvalidSignature {
                view,
                validator_index: proposal.proposer,
            }
        })?;

        // Safety check: the proposal's justify_qc must extend our locked QC
        if !self.round_state.is_safe_to_vote(&proposal.justify_qc) {
            return Err(ConsensusError::SafetyViolation {
                qc_view: proposal.justify_qc.view,
                locked_view: self.round_state.locked_qc().view,
            });
        }

        // Update our locked QC if the proposal's justify_qc is higher
        self.round_state.update_locked_qc(&proposal.justify_qc);

        // Chained mode: process piggybacked PrepareQC if present.
        // This allows followers to receive the QC earlier, potentially
        // skipping the wait for the separate PrepareQC broadcast.
        if let Some(ref piggybacked_qc) = proposal.prepare_qc {
            match super::quorum::verify_qc(piggybacked_qc, self.validator_set()) {
                Ok(()) => {
                    tracing::debug!(
                        view,
                        qc_view = piggybacked_qc.view,
                        "accepted piggybacked PrepareQC from proposal"
                    );
                    self.round_state.update_locked_qc(piggybacked_qc);
                }
                Err(e) => {
                    // Invalid piggybacked QC is not fatal — the proposal itself
                    // was already validated. Log and continue without the QC.
                    tracing::warn!(
                        view,
                        error = %e,
                        "rejected invalid piggybacked PrepareQC, ignoring"
                    );
                }
            }
        }

        self.round_state.enter_voting();

        // Request block execution (orchestrator will import block data)
        self.emit(EngineOutput::ExecuteBlock(proposal.block_hash));

        // Check if the block has already been imported (BlockData arrived before Proposal).
        // Reference: Aptos Baby Raptr — "if local data exists, vote immediately".
        if self.imported_blocks.remove(&proposal.block_hash) {
            tracing::debug!(view, block_hash = %proposal.block_hash, "received valid proposal, block already imported, voting immediately");
            self.send_vote(view, proposal.block_hash)?;
        } else {
            // Defer the vote until BlockImported arrives.
            tracing::debug!(view, block_hash = %proposal.block_hash, "received valid proposal, deferring vote until block data imported");
            self.pending_proposal = Some(PendingProposal {
                view: proposal.view,
                block_hash: proposal.block_hash,
            });
        }

        Ok(())
    }

    // ── Vote handling (Round 1: Prepare) ──

    fn process_vote(&mut self, vote: Vote) -> ConsensusResult<()> {
        let view = self.round_state.current_view();

        if vote.view != view {
            return Err(ConsensusError::ViewMismatch {
                current: view,
                received: vote.view,
            });
        }

        // Only the leader processes votes
        if !self.is_current_leader() {
            return Ok(());
        }

        // Equivocation detection: check if this validator already voted for a
        // different block hash in the same view. This is evidence of Byzantine
        // behavior — the validator is attempting to double-vote.
        if let Some(&prev_hash) = self.equivocation_tracker.get(&vote.voter) {
            if prev_hash != vote.block_hash {
                tracing::warn!(
                    view,
                    validator = vote.voter,
                    %prev_hash,
                    new_hash = %vote.block_hash,
                    "equivocation detected: validator voted for two different blocks"
                );
                self.emit(EngineOutput::EquivocationDetected {
                    view,
                    validator: vote.voter,
                    hash1: prev_hash,
                    hash2: vote.block_hash,
                });
                return Ok(());
            }
        } else {
            self.equivocation_tracker.insert(vote.voter, vote.block_hash);
        }

        // Check block hash before verification (read-only borrow of collector)
        let expected_hash = match self.vote_collector.as_ref() {
            Some(c) => c.block_hash(),
            None => return Ok(()),
        };

        if vote.block_hash != expected_hash {
            tracing::debug!(view, voter = vote.voter, "ignoring vote for different block");
            return Ok(());
        }

        // Verify BLS signature before adding to collector (immutable borrow of epoch_manager)
        let pk = self.validator_set().get_public_key(vote.voter)?;
        let msg = signing_message(view, &vote.block_hash);
        pk.verify(&msg, &vote.signature).map_err(|_| {
            ConsensusError::InvalidSignature {
                view,
                validator_index: vote.voter,
            }
        })?;

        // Now take mutable borrow of collector
        let collector = match self.vote_collector.as_mut() {
            Some(c) => c,
            None => {
                tracing::warn!(view, "vote_collector not initialized, ignoring vote");
                return Ok(());
            }
        };
        collector.add_verified_vote(vote.voter, vote.signature)?;

        tracing::debug!(
            view,
            voter = vote.voter,
            count = collector.vote_count(),
            "received vote"
        );

        self.try_form_prepare_qc()
    }

    /// Attempts to form a PrepareQC from collected Round 1 votes.
    /// If quorum is reached, broadcasts the QC and transitions to PreCommit.
    /// The leader also self-votes for CommitVote (Round 2).
    fn try_form_prepare_qc(&mut self) -> ConsensusResult<()> {
        let view = self.round_state.current_view();

        let has_quorum = self
            .vote_collector
            .as_ref()
            .is_some_and(|c| c.has_quorum(self.validator_set().quorum_size()));

        if !has_quorum {
            return Ok(());
        }

        // Already formed QC for this view
        if self.prepare_qc.is_some() {
            return Ok(());
        }

        let collector = match self.vote_collector.as_ref() {
            Some(c) => c,
            None => {
                tracing::warn!(view, "vote_collector not initialized in try_form_prepare_qc");
                return Ok(());
            }
        };
        let qc = collector.build_qc(self.validator_set())?;

        tracing::info!(view, signers = qc.signer_count(), "QC formed, entering pre-commit");

        self.prepare_qc = Some(qc.clone());
        self.round_state.enter_pre_commit();
        self.round_state.update_locked_qc(&qc);

        // Capture block_hash before qc is moved into the PrepareQC message.
        let block_hash = qc.block_hash;

        // Broadcast PrepareQC so validators can send CommitVotes
        let prepare_qc_msg = PrepareQC {
            view,
            block_hash,
            qc,
        };
        self.emit(EngineOutput::BroadcastMessage(
            ConsensusMessage::PrepareQC(prepare_qc_msg),
        ));

        // Leader self-vote for CommitVote (Round 2)
        let commit_msg = commit_signing_message(view, &block_hash);
        let commit_sig = self.secret_key.sign(&commit_msg);
        if let Some(ref mut collector) = self.commit_collector {
            let _ = collector.add_vote(self.my_index, commit_sig);
        }

        // Check if commit quorum already reached (single-validator scenario)
        self.try_form_commit_qc()
    }

    // ── PrepareQC handling (Validators receive QC from leader) ──

    /// Processes a PrepareQC from the leader: validates the QC and sends a CommitVote.
    fn process_prepare_qc(&mut self, pqc: PrepareQC) -> ConsensusResult<()> {
        let view = self.round_state.current_view();

        if pqc.view != view {
            return Err(ConsensusError::ViewMismatch {
                current: view,
                received: pqc.view,
            });
        }

        // Verify the QC signatures
        super::quorum::verify_qc(&pqc.qc, self.validator_set())?;

        // Update locked QC
        self.round_state.update_locked_qc(&pqc.qc);
        self.round_state.enter_pre_commit();

        tracing::debug!(view, block_hash = %pqc.block_hash, "received valid PrepareQC, sending commit vote");

        // Send CommitVote (Round 2) to leader
        let commit_msg = commit_signing_message(view, &pqc.block_hash);
        let commit_sig = self.secret_key.sign(&commit_msg);

        let leader = LeaderSelector::leader_for_view(view, self.validator_set());

        let commit_vote = CommitVote {
            view,
            block_hash: pqc.block_hash,
            voter: self.my_index,
            signature: commit_sig,
        };

        self.emit(EngineOutput::SendToValidator(
            leader,
            ConsensusMessage::CommitVote(commit_vote),
        ));

        Ok(())
    }

    // ── Commit Vote handling (Round 2) ──

    fn process_commit_vote(&mut self, cv: CommitVote) -> ConsensusResult<()> {
        let view = self.round_state.current_view();

        if cv.view != view {
            return Err(ConsensusError::ViewMismatch {
                current: view,
                received: cv.view,
            });
        }

        if !self.is_current_leader() {
            return Ok(());
        }

        // Check block hash before verification (read-only borrow of collector)
        let expected_hash = match self.commit_collector.as_ref() {
            Some(c) => c.block_hash(),
            None => return Ok(()),
        };

        if cv.block_hash != expected_hash {
            tracing::debug!(view, voter = cv.voter, "ignoring commit vote for different block");
            return Ok(());
        }

        // Verify BLS signature (immutable borrow of epoch_manager)
        let pk = self.validator_set().get_public_key(cv.voter)?;
        let msg = commit_signing_message(view, &cv.block_hash);
        pk.verify(&msg, &cv.signature).map_err(|_| {
            ConsensusError::InvalidSignature {
                view,
                validator_index: cv.voter,
            }
        })?;

        // Now take mutable borrow of collector
        let collector = match self.commit_collector.as_mut() {
            Some(c) => c,
            None => {
                tracing::warn!(view, "commit_collector not initialized, ignoring commit vote");
                return Ok(());
            }
        };
        collector.add_verified_vote(cv.voter, cv.signature)?;

        tracing::debug!(
            view,
            voter = cv.voter,
            count = collector.vote_count(),
            "received commit vote"
        );

        self.try_form_commit_qc()
    }

    /// Attempts to form a Commit QC from collected Round 2 votes.
    /// Uses commit_signing_message for signature verification.
    fn try_form_commit_qc(&mut self) -> ConsensusResult<()> {
        let view = self.round_state.current_view();

        let has_quorum = self
            .commit_collector
            .as_ref()
            .is_some_and(|c| c.has_quorum(self.validator_set().quorum_size()));

        if !has_quorum {
            return Ok(());
        }

        // CommitVotes use "commit" prefix in their signing message.
        // Safety: has_quorum is true only when commit_collector is Some,
        // but we guard defensively to avoid panics in edge cases.
        let collector = match self.commit_collector.as_ref() {
            Some(c) => c,
            None => return Ok(()),
        };
        let block_hash = collector.block_hash();
        let commit_msg = commit_signing_message(view, &block_hash);
        let commit_qc = collector.build_qc_with_message(self.validator_set(), &commit_msg)?;

        tracing::info!(view, %block_hash, "block committed!");

        self.round_state.commit(commit_qc.clone());

        // Broadcast Decide to all followers so they can also commit and advance.
        let decide = Decide {
            view,
            block_hash,
            commit_qc: commit_qc.clone(),
        };
        self.emit(EngineOutput::BroadcastMessage(
            ConsensusMessage::Decide(decide),
        ));

        self.emit(EngineOutput::BlockCommitted {
            view,
            block_hash,
            commit_qc,
        });

        // Advance to next view
        let next_view = view.saturating_add(1);
        self.advance_to_view(next_view);

        Ok(())
    }

    // ── Decide handling ──

    /// Processes a Decide message from the leader.
    /// Verifies the CommitQC and advances the follower to the next view.
    ///
    /// If the Decide's view is far ahead of our current view (gap > 3),
    /// emits `SyncRequired` so the orchestrator can initiate block sync
    /// to catch up on missed blocks.
    fn process_decide(&mut self, decide: Decide) -> ConsensusResult<()> {
        let current_view = self.round_state.current_view();

        // Accept Decide for current or future views (catch-up scenario)
        if decide.view < current_view {
            tracing::debug!(
                decide_view = decide.view,
                current_view,
                "ignoring stale Decide"
            );
            return Ok(());
        }

        // Verify the CommitQC has sufficient signers (quorum check)
        let quorum_size = self.validator_set().quorum_size();
        if decide.commit_qc.signer_count() < quorum_size {
            return Err(ConsensusError::InsufficientVotes {
                view: decide.view,
                have: decide.commit_qc.signer_count(),
                need: quorum_size,
            });
        }

        // Verify QC view matches Decide view
        if decide.commit_qc.view != decide.view {
            return Err(ConsensusError::ViewMismatch {
                current: decide.view,
                received: decide.commit_qc.view,
            });
        }

        // Verify QC block_hash matches
        if decide.commit_qc.block_hash != decide.block_hash {
            return Err(ConsensusError::BlockHashMismatch {
                expected: decide.block_hash,
                got: decide.commit_qc.block_hash,
            });
        }

        // Verify CommitQC aggregate BLS signature (S1 fix).
        // Without this, a Byzantine leader could forge a Decide with a valid-looking
        // signer bitmap but an invalid aggregate signature.
        super::quorum::verify_commit_qc(&decide.commit_qc, self.validator_set())?;

        // Detect view gap: if we're more than 3 views behind, we've missed
        // blocks and need to sync them from peers before we can participate.
        const SYNC_GAP_THRESHOLD: u64 = 3;
        if decide.view > current_view + SYNC_GAP_THRESHOLD {
            tracing::warn!(
                current_view,
                decide_view = decide.view,
                gap = decide.view - current_view,
                "large view gap detected, requesting state sync"
            );
            self.emit(EngineOutput::SyncRequired {
                local_view: current_view,
                target_view: decide.view,
            });
        }

        tracing::info!(
            view = decide.view,
            %decide.block_hash,
            "received Decide, committing block"
        );

        // Commit and advance
        self.round_state.update_locked_qc(&decide.commit_qc);
        self.round_state.commit(decide.commit_qc.clone());

        self.emit(EngineOutput::BlockCommitted {
            view: decide.view,
            block_hash: decide.block_hash,
            commit_qc: decide.commit_qc,
        });

        let next_view = decide.view.saturating_add(1);
        self.advance_to_view(next_view);

        Ok(())
    }

    // ── Timeout handling (Round 3: ViewChange) ──

    fn process_timeout(&mut self, timeout: TimeoutMessage) -> ConsensusResult<()> {
        let view = self.round_state.current_view();

        // Reject stale timeouts from past views.
        if timeout.view < view {
            return Ok(());
        }

        // Future-view timeout: advance to that view to enable TC formation.
        //
        // After crash/recovery, nodes end up at different views due to independent
        // timeout progression during the stall. Without view synchronization,
        // they can never converge because TCs require quorum timeouts at the
        // SAME view. The standard HotStuff approach: when a validator provides
        // signed evidence that it has timed out at view V+k, we should join
        // that view's timeout collection.
        if timeout.view > view {
            if timeout.view > view + FUTURE_VIEW_WINDOW {
                return Ok(()); // Too far ahead, ignore.
            }

            // Verify BLS signature BEFORE advancing (prevents unauthenticated view jumps).
            let pk = self.validator_set().get_public_key(timeout.sender)?;
            let msg = timeout_signing_message(timeout.view);
            pk.verify(&msg, &timeout.signature).map_err(|_| {
                ConsensusError::InvalidSignature {
                    view: timeout.view,
                    validator_index: timeout.sender,
                }
            })?;

            tracing::info!(
                current_view = view,
                timeout_view = timeout.view,
                sender = timeout.sender,
                "advancing to higher timeout view for synchronization"
            );

            // Advance to the timeout's view. This resets phase to WaitingForProposal
            // and clears all collectors. We'll re-initialize them below.
            self.advance_to_view(timeout.view);

            // Enter TimedOut state and initialize timeout collector for this view.
            self.round_state.timeout();
            // Intentional second reset: advance_to_view already called
            // reset_for_view with consecutive_timeouts=0. After timeout()
            // increments the counter, this second reset applies the correct
            // exponential backoff based on the updated count.
            self.pacemaker.reset_for_view(
                timeout.view,
                self.round_state.consecutive_timeouts(),
            );
            // Conditional creation: advance_to_view replays buffered messages,
            // which may have already created and populated a timeout_collector
            // for this view. Overwriting it would discard those collected timeouts.
            let n_validators = self.validator_set().len();
            if self.timeout_collector.as_ref().map_or(true, |tc| tc.view() != timeout.view) {
                self.timeout_collector =
                    Some(TimeoutCollector::new(timeout.view, n_validators));
            }

            // Add the received (already verified) timeout to the collector.
            if let Some(ref mut collector) = self.timeout_collector {
                collector.add_verified_timeout(
                    timeout.sender,
                    timeout.signature.clone(),
                    timeout.high_qc.clone(),
                )?;
            }

            // Broadcast our own timeout for this view so other nodes can also
            // converge and form a TC.
            let own_msg = timeout_signing_message(timeout.view);
            let own_sig = self.secret_key.sign(&own_msg);
            let own_timeout = TimeoutMessage {
                view: timeout.view,
                high_qc: self.round_state.locked_qc().clone(),
                sender: self.my_index,
                signature: own_sig,
            };
            self.emit(EngineOutput::BroadcastMessage(
                ConsensusMessage::Timeout(own_timeout.clone()),
            ));

            // Process our own timeout (adds to collector, checks TC formation).
            // This is safe because process_timeout will now match on timeout.view == current view.
            return self.process_timeout(own_timeout);
        }

        // Current-view timeout processing.

        // Verify BLS signature on timeout message before adding to collector
        let pk = self.validator_set().get_public_key(timeout.sender)?;
        let msg = timeout_signing_message(view);
        pk.verify(&msg, &timeout.signature).map_err(|_| {
            ConsensusError::InvalidSignature {
                view,
                validator_index: timeout.sender,
            }
        })?;

        // Cache values before mutable borrow of timeout_collector
        let n_validators = self.validator_set().len();
        let quorum_size = self.validator_set().quorum_size();
        let next_view = view.saturating_add(1);
        let next_leader = LeaderSelector::leader_for_view(next_view, self.validator_set());

        let collector = self
            .timeout_collector
            .get_or_insert_with(|| TimeoutCollector::new(view, n_validators));

        collector.add_verified_timeout(
            timeout.sender,
            timeout.signature,
            timeout.high_qc,
        )?;

        tracing::debug!(
            view,
            sender = timeout.sender,
            count = collector.timeout_count(),
            "received timeout"
        );

        // Check if we should form a TC and trigger view change
        let should_form_tc = collector.has_quorum(quorum_size)
            && next_leader == self.my_index;

        // Drop the mutable borrow of collector before accessing validator_set
        if should_form_tc {
            self.try_form_tc_and_advance(view, next_view)?;
        }

        Ok(())
    }

    fn process_new_view(&mut self, nv: NewView) -> ConsensusResult<()> {
        let view = self.round_state.current_view();

        // Accept new view for future views
        if nv.view <= view {
            return Ok(());
        }

        // Verify the new leader is correct
        let expected_leader =
            LeaderSelector::leader_for_view(nv.view, self.validator_set());
        if nv.leader != expected_leader {
            return Err(ConsensusError::InvalidProposer {
                view: nv.view,
                expected: expected_leader,
                actual: nv.leader,
            });
        }

        // Verify leader's signature on the NewView message.
        // Without this, a Byzantine node could forge NewView to force view jumps.
        let pk = self.validator_set().get_public_key(nv.leader)?;
        let nv_msg = timeout_signing_message(nv.view);
        pk.verify(&nv_msg, &nv.signature).map_err(|_| {
            ConsensusError::InvalidSignature {
                view: nv.view,
                validator_index: nv.leader,
            }
        })?;

        // Verify TC: the timeout certificate must be for the previous view (nv.view - 1)
        // and must have valid aggregated signatures from a quorum of validators.
        if nv.timeout_cert.view != nv.view.saturating_sub(1) {
            return Err(ConsensusError::InvalidTC {
                view: nv.timeout_cert.view,
                reason: format!(
                    "TC view {} does not match expected view {} (nv.view - 1)",
                    nv.timeout_cert.view,
                    nv.view.saturating_sub(1)
                ),
            });
        }
        super::quorum::verify_tc(&nv.timeout_cert, self.validator_set())?;

        // Verify the TC's high_qc signature to prevent injection of forged QCs.
        // Genesis QC (view 0) is exempt — it has no real signatures.
        if nv.timeout_cert.high_qc.view > 0 {
            super::quorum::verify_qc(&nv.timeout_cert.high_qc, self.validator_set())?;
        }

        // Update locked QC from TC's high_qc if higher
        self.round_state.update_locked_qc(&nv.timeout_cert.high_qc);

        tracing::info!(
            old_view = view,
            new_view = nv.view,
            "received NewView, advancing"
        );

        self.advance_to_view(nv.view);
        // Use actual view: advance_to_view may replay buffered messages
        // that push the view beyond nv.view (e.g., replayed Decide).
        let actual_view = self.round_state.current_view();
        self.emit(EngineOutput::ViewChanged { new_view: actual_view });

        Ok(())
    }

    // ── Block data import handling ──

    /// Sends a Round 1 vote for the given view and block hash.
    /// Extracted from process_proposal() to support both immediate and deferred voting.
    fn send_vote(&mut self, view: ViewNumber, block_hash: B256) -> ConsensusResult<()> {
        let leader = LeaderSelector::leader_for_view(view, self.validator_set());
        let vote_msg = signing_message(view, &block_hash);
        let vote_sig = self.secret_key.sign(&vote_msg);

        let vote = Vote {
            view,
            block_hash,
            voter: self.my_index,
            signature: vote_sig,
        };

        self.emit(EngineOutput::SendToValidator(
            leader,
            ConsensusMessage::Vote(vote),
        ));

        Ok(())
    }

    /// Handles the BlockImported event from the orchestrator.
    ///
    /// Two message arrival orders are supported (reference: Aptos Baby Raptr):
    /// 1. Proposal first, BlockData later → pending_proposal set → vote now
    /// 2. BlockData first, Proposal later → cache in imported_blocks → vote when Proposal arrives
    fn on_block_imported(&mut self, block_hash: B256) -> ConsensusResult<()> {
        if let Some(pending) = self.pending_proposal.take() {
            if pending.block_hash == block_hash {
                // Case 1: Proposal arrived first, BlockData arrived now → send deferred vote
                tracing::debug!(
                    view = pending.view, %block_hash,
                    "block imported, sending deferred vote"
                );
                self.send_vote(pending.view, pending.block_hash)?;
            } else {
                // Hash mismatch: cache this import, restore the pending proposal
                if self.imported_blocks.len() < 32 {
                    self.imported_blocks.insert(block_hash);
                }
                self.pending_proposal = Some(pending);
            }
        } else {
            // Case 2: BlockData arrived first, no Proposal yet → cache for later
            if self.imported_blocks.len() < 32 {
                self.imported_blocks.insert(block_hash);
            }
        }
        Ok(())
    }

    // ── Internal helpers ──

    /// Builds a TC from the current timeout_collector, broadcasts NewView,
    /// and advances to `next_view`. Centralises the TC-formation logic used
    /// by process_timeout so the BUG-A fix (actual-view ViewChanged) is
    /// applied consistently.
    fn try_form_tc_and_advance(
        &mut self,
        current_view: ViewNumber,
        next_view: ViewNumber,
    ) -> ConsensusResult<()> {
        let tc = match self.timeout_collector.as_ref() {
            Some(c) => c.build_tc(self.validator_set())?,
            None => {
                tracing::warn!(
                    view = current_view,
                    "timeout_collector disappeared during TC formation"
                );
                return Ok(());
            }
        };

        tracing::info!(
            view = current_view,
            "TC formed, I am the new leader for view {}",
            next_view
        );

        // Update locked QC from TC's high_qc if higher
        self.round_state.update_locked_qc(&tc.high_qc);

        // Broadcast NewView to all validators
        let nv_message = timeout_signing_message(next_view);
        let nv_sig = self.secret_key.sign(&nv_message);

        let new_view = NewView {
            view: next_view,
            timeout_cert: tc,
            leader: self.my_index,
            signature: nv_sig,
        };

        self.emit(EngineOutput::BroadcastMessage(
            ConsensusMessage::NewView(new_view),
        ));

        self.advance_to_view(next_view);
        // Use actual view: advance_to_view may replay buffered messages
        // that push the view beyond next_view (e.g., replayed Decide).
        let actual_view = self.round_state.current_view();
        self.emit(EngineOutput::ViewChanged { new_view: actual_view });

        Ok(())
    }

    fn advance_to_view(&mut self, new_view: ViewNumber) {
        // Monotonicity guard: never regress the view. This can happen if a
        // buffered-message replay triggers a nested advance_to_view (e.g., a
        // replayed Decide commits and advances past `new_view`).
        if new_view <= self.round_state.current_view() {
            return;
        }

        // Chained mode: save the current PrepareQC for piggybacking into next Proposal.
        // Only save if we actually formed a QC this view (timeout views won't have one).
        self.previous_prepare_qc = self.prepare_qc.take();

        // Check epoch boundary: if epochs are enabled and we're crossing into
        // a new epoch, attempt to advance the validator set.
        if self.epoch_manager.epochs_enabled() && self.epoch_manager.is_epoch_boundary(new_view) {
            if self.epoch_manager.advance_epoch() {
                let new_epoch = self.epoch_manager.current_epoch();
                let validator_count = self.validator_set().len();
                tracing::info!(
                    new_epoch,
                    validator_count,
                    view = new_view,
                    "epoch transition at view boundary"
                );
                self.emit(EngineOutput::EpochTransition {
                    new_epoch,
                    validator_count,
                });
            }
        }

        self.round_state.advance_view(new_view);
        self.pacemaker.reset_for_view(
            new_view,
            self.round_state.consecutive_timeouts(),
        );
        self.vote_collector = None;
        self.commit_collector = None;
        self.timeout_collector = None;
        self.prepare_qc = None;
        self.pending_proposal = None;
        self.imported_blocks.clear();
        self.equivocation_tracker.clear();

        // Replay buffered messages for the new view and discard expired ones.
        // Two-phase approach: drain all, then partition into replay/keep/discard.
        let drained: Vec<(ViewNumber, ConsensusMessage)> =
            self.future_msg_buffer.drain(..).collect();

        let mut to_replay = Vec::new();
        for (view, msg) in drained {
            if view == new_view {
                to_replay.push(msg);
            } else if view > new_view && view <= new_view + FUTURE_VIEW_WINDOW {
                // Still in window — re-buffer
                self.future_msg_buffer.push((view, msg));
            }
            // else: expired — discard silently
        }

        if !to_replay.is_empty() {
            tracing::debug!(
                view = new_view,
                replaying = to_replay.len(),
                "replaying buffered future-view messages"
            );
        }

        for msg in to_replay {
            if let Err(e) = self.dispatch_message(msg) {
                tracing::debug!(view = new_view, error = %e, "buffered message replay failed");
            }
        }

        tracing::debug!(view = new_view, "advanced to new view");
    }

    fn emit(&self, output: EngineOutput) {
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

    /// Helper: create a test engine with `n` validators, returning the engine
    /// for validator at index `my_index`, all secret keys, and the output receiver.
    fn make_engine(
        n: usize,
        my_index: u32,
    ) -> (
        ConsensusEngine,
        Vec<BlsSecretKey>,
        ValidatorSet,
        mpsc::Receiver<EngineOutput>,
    ) {
        let sks: Vec<_> = (0..n).map(|_| BlsSecretKey::random().unwrap()).collect();
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

    #[test]
    fn test_engine_creation() {
        let (engine, _, _, _rx) = make_engine(4, 0);

        assert_eq!(engine.current_view(), 1, "initial view should be 1");
        assert_eq!(
            engine.current_phase(),
            Phase::WaitingForProposal,
            "initial phase should be WaitingForProposal"
        );
    }

    #[test]
    fn test_engine_is_leader() {
        // With 4 validators, view 1: leader is 1 % 4 = 1
        let (engine, _, _, _rx) = make_engine(4, 1);
        assert!(
            engine.is_current_leader(),
            "validator 1 should be leader at view 1"
        );

        let (engine, _, _, _rx) = make_engine(4, 0);
        assert!(
            !engine.is_current_leader(),
            "validator 0 should NOT be leader at view 1"
        );
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
        // Validator 0 is NOT the leader for view 1 (leader is 1)
        let (mut engine, _, _, _rx) = make_engine(4, 0);
        let block_hash = B256::repeat_byte(0xAA);

        // Should succeed but do nothing (not leader)
        engine
            .process_event(ConsensusEvent::BlockReady(block_hash))
            .expect("non-leader block ready should succeed");

        assert_eq!(
            engine.current_phase(),
            Phase::WaitingForProposal,
            "phase should remain WaitingForProposal for non-leader"
        );
    }

    #[test]
    fn test_engine_block_ready_as_leader() {
        // Single validator: always leader.
        // With self-voting, the entire 2-round consensus completes immediately:
        // BlockReady → Propose → self-vote → QC → self-CommitVote → Commit → next view
        let (mut engine, _, _, mut rx) = make_engine(1, 0);
        let block_hash = B256::repeat_byte(0xBB);

        engine
            .process_event(ConsensusEvent::BlockReady(block_hash))
            .expect("leader block ready should succeed");

        // Single-validator: consensus completes in one call, advancing to next view
        assert_eq!(
            engine.current_view(),
            2,
            "should advance to view 2 after single-validator consensus"
        );
        assert_eq!(
            engine.current_phase(),
            Phase::WaitingForProposal,
            "should be WaitingForProposal in new view"
        );

        // Collect all outputs
        let mut outputs = vec![];
        while let Ok(output) = rx.try_recv() {
            outputs.push(output);
        }

        // Should have emitted: Proposal, PrepareQC, Decide, BlockCommitted
        let has_proposal = outputs
            .iter()
            .any(|o| matches!(o, EngineOutput::BroadcastMessage(ConsensusMessage::Proposal(_))));
        assert!(has_proposal, "should broadcast a Proposal");

        let has_prepare_qc = outputs
            .iter()
            .any(|o| matches!(o, EngineOutput::BroadcastMessage(ConsensusMessage::PrepareQC(_))));
        assert!(has_prepare_qc, "should broadcast a PrepareQC");

        let has_decide = outputs
            .iter()
            .any(|o| matches!(o, EngineOutput::BroadcastMessage(ConsensusMessage::Decide(_))));
        assert!(has_decide, "should broadcast a Decide");

        let has_committed = outputs
            .iter()
            .any(|o| matches!(o, EngineOutput::BlockCommitted { block_hash: h, .. } if *h == block_hash));
        assert!(has_committed, "should emit BlockCommitted");
    }

    #[test]
    fn test_engine_proposal_wrong_view() {
        let (mut engine, sks, _, _rx) = make_engine(4, 0);

        // Create a proposal with a past view (0 < current_view=1).
        // Past-view messages bypass the future-view buffer and go directly
        // to dispatch_message, which rejects them via ViewMismatch.
        let msg = signing_message(0, &B256::repeat_byte(0xCC));
        let sig = sks[1].sign(&msg);

        let proposal = Proposal {
            view: 0,
            block_hash: B256::repeat_byte(0xCC),
            justify_qc: QuorumCertificate::genesis(),
            proposer: 1,
            signature: sig,
            prepare_qc: None,
        };

        let result = engine.process_event(ConsensusEvent::Message(
            ConsensusMessage::Proposal(proposal),
        ));
        assert!(result.is_err(), "proposal with wrong view should be rejected");
    }

    #[test]
    fn test_engine_proposal_wrong_proposer() {
        let (mut engine, sks, _, _rx) = make_engine(4, 0);

        // View 1: leader should be validator 1, but we set proposer to 0
        let msg = signing_message(1, &B256::repeat_byte(0xDD));
        let sig = sks[0].sign(&msg);

        let proposal = Proposal {
            view: 1,
            block_hash: B256::repeat_byte(0xDD),
            justify_qc: QuorumCertificate::genesis(),
            proposer: 0, // Wrong! Leader should be 1
            signature: sig,
            prepare_qc: None,
        };

        let result = engine.process_event(ConsensusEvent::Message(
            ConsensusMessage::Proposal(proposal),
        ));
        assert!(result.is_err(), "proposal from wrong proposer should be rejected");

        match result.unwrap_err() {
            ConsensusError::InvalidProposer { expected, actual, .. } => {
                assert_eq!(expected, 1);
                assert_eq!(actual, 0);
            }
            other => panic!("expected InvalidProposer, got: {:?}", other),
        }
    }

    #[test]
    fn test_engine_valid_proposal_defers_vote() {
        let (mut engine, sks, _, mut rx) = make_engine(4, 0);

        // View 1: leader is validator 1
        let block_hash = B256::repeat_byte(0xEE);
        let msg = signing_message(1, &block_hash);
        let sig = sks[1].sign(&msg);

        let proposal = Proposal {
            view: 1,
            block_hash,
            justify_qc: QuorumCertificate::genesis(),
            proposer: 1,
            signature: sig,
            prepare_qc: None,
        };

        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Proposal(
                proposal,
            )))
            .expect("valid proposal should be accepted");

        assert_eq!(engine.current_phase(), Phase::Voting);

        // Should have emitted ExecuteBlock but NOT a vote yet (deferred)
        let mut outputs = vec![];
        while let Ok(output) = rx.try_recv() {
            outputs.push(output);
        }

        let has_execute = outputs
            .iter()
            .any(|o| matches!(o, EngineOutput::ExecuteBlock(h) if *h == block_hash));
        assert!(has_execute, "should request block execution");

        let has_vote = outputs
            .iter()
            .any(|o| matches!(o, EngineOutput::SendToValidator(_, ConsensusMessage::Vote(_))));
        assert!(!has_vote, "should NOT send vote before block is imported");

        // Verify pending_proposal is set
        assert!(engine.pending_proposal.is_some(), "should have pending proposal");

        // Now send BlockImported → should trigger deferred vote
        engine
            .process_event(ConsensusEvent::BlockImported(block_hash))
            .expect("BlockImported should succeed");

        let mut outputs = vec![];
        while let Ok(output) = rx.try_recv() {
            outputs.push(output);
        }

        let has_vote = outputs
            .iter()
            .any(|o| matches!(o, EngineOutput::SendToValidator(_, ConsensusMessage::Vote(_))));
        assert!(has_vote, "should send vote after block is imported");

        // pending_proposal should be cleared
        assert!(engine.pending_proposal.is_none(), "pending proposal should be cleared");
    }

    #[test]
    fn test_engine_block_imported_before_proposal() {
        let (mut engine, sks, _, mut rx) = make_engine(4, 0);
        let block_hash = B256::repeat_byte(0xEE);

        // BlockData arrives BEFORE Proposal
        engine
            .process_event(ConsensusEvent::BlockImported(block_hash))
            .expect("BlockImported before proposal should succeed");

        // Should be cached in imported_blocks
        assert!(engine.imported_blocks.contains(&block_hash), "block should be cached");

        // Now Proposal arrives
        let msg = signing_message(1, &block_hash);
        let sig = sks[1].sign(&msg);
        let proposal = Proposal {
            view: 1,
            block_hash,
            justify_qc: QuorumCertificate::genesis(),
            proposer: 1,
            signature: sig,
            prepare_qc: None,
        };

        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Proposal(proposal)))
            .expect("valid proposal should be accepted");

        // Should have voted immediately (block was already imported)
        let mut outputs = vec![];
        while let Ok(output) = rx.try_recv() {
            outputs.push(output);
        }

        let has_vote = outputs
            .iter()
            .any(|o| matches!(o, EngineOutput::SendToValidator(_, ConsensusMessage::Vote(_))));
        assert!(has_vote, "should send vote immediately when block was already imported");

        // No pending proposal
        assert!(engine.pending_proposal.is_none(), "should not have pending proposal");
        // Block should be removed from cache
        assert!(!engine.imported_blocks.contains(&block_hash), "block should be removed from cache");
    }

    #[test]
    fn test_engine_timeout_clears_pending_proposal() {
        let (mut engine, sks, _, mut rx) = make_engine(4, 0);
        let block_hash = B256::repeat_byte(0xEE);

        // Receive a valid proposal (defers vote)
        let msg = signing_message(1, &block_hash);
        let sig = sks[1].sign(&msg);
        let proposal = Proposal {
            view: 1,
            block_hash,
            justify_qc: QuorumCertificate::genesis(),
            proposer: 1,
            signature: sig,
            prepare_qc: None,
        };
        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Proposal(proposal)))
            .expect("proposal should succeed");
        while rx.try_recv().is_ok() {}

        assert!(engine.pending_proposal.is_some(), "should have pending proposal");

        // Timeout → pending should be cleared
        engine.on_timeout().expect("timeout should succeed");
        assert!(engine.pending_proposal.is_none(), "timeout should clear pending proposal");
    }

    #[test]
    fn test_engine_timeout() {
        let (mut engine, _, _, mut rx) = make_engine(4, 0);

        engine.on_timeout().expect("timeout should succeed");

        assert_eq!(engine.current_phase(), Phase::TimedOut);

        // Should have broadcast a Timeout message
        let output = rx.try_recv().expect("should have timeout output");
        assert!(
            matches!(
                output,
                EngineOutput::BroadcastMessage(ConsensusMessage::Timeout(_))
            ),
            "should broadcast a Timeout message"
        );
    }

    #[test]
    fn test_engine_vote_non_leader_ignored() {
        let (mut engine, sks, _, _rx) = make_engine(4, 0);

        // Validator 0 is NOT the leader for view 1, so votes should be ignored
        let msg = signing_message(1, &B256::repeat_byte(0xAA));
        let sig = sks[1].sign(&msg);

        let vote = Vote {
            view: 1,
            block_hash: B256::repeat_byte(0xAA),
            voter: 1,
            signature: sig,
        };

        // Should succeed (silently ignored)
        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Vote(vote)))
            .expect("vote to non-leader should not error");
    }

    #[test]
    fn test_engine_pacemaker_accessible() {
        let (engine, _, _, _rx) = make_engine(1, 0);
        let pacemaker = engine.pacemaker();

        assert!(
            !pacemaker.is_timed_out(),
            "pacemaker should not be timed out initially"
        );
    }

    /// Full two-round consensus test with 4 validators.
    /// Simulates: Leader proposes → 3 validators vote → QC formed → PrepareQC broadcast
    /// → 3 validators send CommitVote → block committed.
    #[test]
    fn test_full_consensus_4_validators() {
        use crate::protocol::quorum::commit_signing_message;

        // View 1: leader is validator 1 (1 % 4)
        let (mut engine, sks, _vs, mut rx) = make_engine(4, 1);
        let block_hash = B256::repeat_byte(0xF1);
        let view = 1u64;

        // Step 1: Leader proposes
        engine
            .process_event(ConsensusEvent::BlockReady(block_hash))
            .expect("block ready should succeed");

        // Leader is in Voting phase (self-vote added but quorum=3, only 1 vote so far)
        assert_eq!(engine.current_phase(), Phase::Voting);

        // Drain outputs so far
        let mut outputs = vec![];
        while let Ok(o) = rx.try_recv() {
            outputs.push(o);
        }
        assert!(outputs.iter().any(|o| matches!(o, EngineOutput::BroadcastMessage(ConsensusMessage::Proposal(_)))));

        // Step 2: Two more validators send votes (need 3 total for quorum, leader already has 1)
        for i in [0u32, 2] {
            let msg = signing_message(view, &block_hash);
            let sig = sks[i as usize].sign(&msg);
            let vote = Vote { view, block_hash, voter: i, signature: sig };
            engine
                .process_event(ConsensusEvent::Message(ConsensusMessage::Vote(vote)))
                .expect("vote should succeed");
        }

        // After 3 votes (leader + 2), QC should be formed → PreCommit phase
        // But leader also self-voted for CommitVote, so now we check commit collector
        // The engine should have broadcast PrepareQC
        outputs.clear();
        while let Ok(o) = rx.try_recv() {
            outputs.push(o);
        }
        let has_prepare_qc = outputs.iter().any(|o| matches!(o, EngineOutput::BroadcastMessage(ConsensusMessage::PrepareQC(_))));
        assert!(has_prepare_qc, "leader should broadcast PrepareQC after forming QC");

        // Step 3: Two more validators send commit votes (leader already self-voted)
        for i in [0u32, 2] {
            let msg = commit_signing_message(view, &block_hash);
            let sig = sks[i as usize].sign(&msg);
            let cv = CommitVote { view, block_hash, voter: i, signature: sig };
            engine
                .process_event(ConsensusEvent::Message(ConsensusMessage::CommitVote(cv)))
                .expect("commit vote should succeed");
        }

        // Block should be committed, engine advances to view 2
        assert_eq!(engine.current_view(), 2, "should advance to view 2");
        assert_eq!(engine.current_phase(), Phase::WaitingForProposal);

        outputs.clear();
        while let Ok(o) = rx.try_recv() {
            outputs.push(o);
        }
        let has_decide = outputs.iter().any(|o| matches!(o, EngineOutput::BroadcastMessage(ConsensusMessage::Decide(_))));
        assert!(has_decide, "should broadcast Decide");
        let has_committed = outputs.iter().any(|o| matches!(o, EngineOutput::BlockCommitted { block_hash: h, view: v, .. } if *h == block_hash && *v == view));
        assert!(has_committed, "should emit BlockCommitted");
    }

    /// Test that validators correctly process PrepareQC and send CommitVote.
    #[test]
    fn test_validator_receives_prepare_qc() {
        use crate::protocol::quorum::VoteCollector;

        // Validator 0, view 1, leader is validator 1
        let (mut engine, sks, vs, mut rx) = make_engine(4, 0);
        let block_hash = B256::repeat_byte(0xF2);
        let view = 1u64;

        // First, the validator receives a valid proposal from the leader
        let prop_msg = signing_message(view, &block_hash);
        let prop_sig = sks[1].sign(&prop_msg);
        let proposal = Proposal {
            view,
            block_hash,
            justify_qc: QuorumCertificate::genesis(),
            proposer: 1,
            signature: prop_sig,
            prepare_qc: None,
        };
        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Proposal(proposal)))
            .expect("proposal should succeed");

        // Simulate block data import (required before vote is sent)
        engine
            .process_event(ConsensusEvent::BlockImported(block_hash))
            .expect("BlockImported should succeed");

        // Drain outputs
        while rx.try_recv().is_ok() {}

        // Build a valid QC from 3 validators
        let mut collector = VoteCollector::new(view, block_hash, vs.len());
        for i in 0..3u32 {
            let msg = signing_message(view, &block_hash);
            let sig = sks[i as usize].sign(&msg);
            collector.add_vote(i, sig).unwrap();
        }
        let qc = collector.build_qc(&vs).unwrap();

        // Validator receives PrepareQC
        let prepare_qc = PrepareQC { view, block_hash, qc };
        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::PrepareQC(prepare_qc)))
            .expect("PrepareQC should succeed");

        assert_eq!(engine.current_phase(), Phase::PreCommit);

        // Should have sent CommitVote to leader (validator 1)
        let mut outputs = vec![];
        while let Ok(o) = rx.try_recv() {
            outputs.push(o);
        }
        let has_commit_vote = outputs.iter().any(|o| matches!(o, EngineOutput::SendToValidator(1, ConsensusMessage::CommitVote(cv)) if cv.view == view && cv.block_hash == block_hash));
        assert!(has_commit_vote, "validator should send CommitVote to leader");
    }

    // ── Level 2: Early signature verification tests ──

    /// Votes with invalid BLS signatures should be rejected before entering the collector.
    #[test]
    fn test_process_vote_rejects_invalid_signature() {
        // Leader is validator 1 at view 1 (1 % 4)
        let (mut engine, sks, _, mut rx) = make_engine(4, 1);
        let block_hash = B256::repeat_byte(0xA1);
        let view = 1u64;

        // Leader proposes
        engine
            .process_event(ConsensusEvent::BlockReady(block_hash))
            .expect("block ready should succeed");
        while rx.try_recv().is_ok() {}

        // Send a vote with a signature signed over wrong message
        let wrong_msg = signing_message(99, &block_hash);
        let bad_sig = sks[0].sign(&wrong_msg);
        let vote = Vote {
            view,
            block_hash,
            voter: 0,
            signature: bad_sig,
        };

        let result = engine.process_event(ConsensusEvent::Message(
            ConsensusMessage::Vote(vote),
        ));
        assert!(result.is_err(), "vote with invalid signature should be rejected");
        match result.unwrap_err() {
            ConsensusError::InvalidSignature { view: v, validator_index } => {
                assert_eq!(v, view);
                assert_eq!(validator_index, 0);
            }
            other => panic!("expected InvalidSignature, got: {:?}", other),
        }
    }

    /// Votes for a different block hash should be silently ignored.
    #[test]
    fn test_process_vote_ignores_wrong_block() {
        let (mut engine, sks, _, mut rx) = make_engine(4, 1);
        let block_hash = B256::repeat_byte(0xA2);
        let wrong_block = B256::repeat_byte(0xFF);
        let view = 1u64;

        engine
            .process_event(ConsensusEvent::BlockReady(block_hash))
            .expect("block ready should succeed");
        while rx.try_recv().is_ok() {}

        // Vote for a different block
        let msg = signing_message(view, &wrong_block);
        let sig = sks[0].sign(&msg);
        let vote = Vote {
            view,
            block_hash: wrong_block,
            voter: 0,
            signature: sig,
        };

        // Should succeed (silently ignored, not an error)
        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Vote(vote)))
            .expect("vote for wrong block should be silently ignored");

        // Engine should still be in Voting phase (no quorum progress)
        assert_eq!(engine.current_phase(), Phase::Voting);
    }

    /// Commit votes with invalid BLS signatures should be rejected.
    #[test]
    fn test_process_commit_vote_rejects_invalid_signature() {
        use crate::protocol::quorum::commit_signing_message;
        use n42_primitives::consensus::CommitVote;

        let (mut engine, sks, _vs, mut rx) = make_engine(4, 1);
        let block_hash = B256::repeat_byte(0xA3);
        let view = 1u64;

        // Run through proposal + votes to reach PreCommit phase
        engine
            .process_event(ConsensusEvent::BlockReady(block_hash))
            .expect("block ready");
        while rx.try_recv().is_ok() {}

        // Two votes from non-leaders to form QC (leader self-voted already)
        for i in [0u32, 2] {
            let msg = signing_message(view, &block_hash);
            let sig = sks[i as usize].sign(&msg);
            let vote = Vote { view, block_hash, voter: i, signature: sig };
            engine
                .process_event(ConsensusEvent::Message(ConsensusMessage::Vote(vote)))
                .expect("vote should succeed");
        }
        while rx.try_recv().is_ok() {}

        // Now submit a commit vote with wrong signature
        let wrong_msg = commit_signing_message(99, &block_hash);
        let bad_sig = sks[0].sign(&wrong_msg);
        let cv = CommitVote {
            view,
            block_hash,
            voter: 0,
            signature: bad_sig,
        };

        let result = engine.process_event(ConsensusEvent::Message(
            ConsensusMessage::CommitVote(cv),
        ));
        assert!(result.is_err(), "commit vote with invalid signature should be rejected");
        match result.unwrap_err() {
            ConsensusError::InvalidSignature { view: v, validator_index } => {
                assert_eq!(v, view);
                assert_eq!(validator_index, 0);
            }
            other => panic!("expected InvalidSignature, got: {:?}", other),
        }
    }

    /// Timeout messages with invalid BLS signatures should be rejected.
    #[test]
    fn test_process_timeout_rejects_invalid_signature() {
        use crate::protocol::quorum::timeout_signing_message;

        let (mut engine, sks, _, mut rx) = make_engine(4, 0);
        let view = 1u64;

        // Trigger timeout first so the engine enters TimedOut phase
        engine.on_timeout().expect("timeout should succeed");
        while rx.try_recv().is_ok() {}

        // Send a timeout with wrong signature
        let wrong_msg = timeout_signing_message(999);
        let bad_sig = sks[2].sign(&wrong_msg);
        let timeout = TimeoutMessage {
            view,
            high_qc: QuorumCertificate::genesis(),
            sender: 2,
            signature: bad_sig,
        };

        let result = engine.process_event(ConsensusEvent::Message(
            ConsensusMessage::Timeout(timeout),
        ));
        assert!(result.is_err(), "timeout with invalid signature should be rejected");
        match result.unwrap_err() {
            ConsensusError::InvalidSignature { view: v, validator_index } => {
                assert_eq!(v, view);
                assert_eq!(validator_index, 2);
            }
            other => panic!("expected InvalidSignature, got: {:?}", other),
        }
    }

    /// Full consensus should succeed even when f validators send invalid signatures.
    /// Valid validators' votes should still reach quorum.
    #[test]
    fn test_consensus_succeeds_despite_invalid_votes() {
        use crate::protocol::quorum::commit_signing_message;
        use n42_primitives::consensus::CommitVote;

        // 4 validators, f=1, quorum=3. Leader is validator 1.
        let (mut engine, sks, _vs, mut rx) = make_engine(4, 1);
        let block_hash = B256::repeat_byte(0xA5);
        let view = 1u64;

        // Step 1: Leader proposes (self-votes)
        engine
            .process_event(ConsensusEvent::BlockReady(block_hash))
            .expect("block ready");
        while rx.try_recv().is_ok() {}

        // Step 2: Validator 0 sends a valid vote
        let msg = signing_message(view, &block_hash);
        let sig = sks[0].sign(&msg);
        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Vote(Vote {
                view,
                block_hash,
                voter: 0,
                signature: sig,
            })))
            .expect("valid vote from 0");

        // Step 3: Validator 3 sends an INVALID vote (wrong signature) — rejected
        let bad_msg = signing_message(99, &block_hash);
        let bad_sig = sks[3].sign(&bad_msg);
        let result = engine.process_event(ConsensusEvent::Message(ConsensusMessage::Vote(Vote {
            view,
            block_hash,
            voter: 3,
            signature: bad_sig,
        })));
        assert!(result.is_err(), "invalid vote should be rejected");

        // Step 4: Validator 2 sends a valid vote → quorum reached (1+0+2 = 3)
        let msg = signing_message(view, &block_hash);
        let sig = sks[2].sign(&msg);
        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Vote(Vote {
                view,
                block_hash,
                voter: 2,
                signature: sig,
            })))
            .expect("valid vote from 2");

        // QC should be formed now (PrepareQC broadcast)
        let mut outputs = vec![];
        while let Ok(o) = rx.try_recv() {
            outputs.push(o);
        }
        let has_prepare_qc = outputs.iter().any(|o| matches!(o, EngineOutput::BroadcastMessage(ConsensusMessage::PrepareQC(_))));
        assert!(has_prepare_qc, "QC should form despite one invalid vote");

        // Step 5: Valid commit votes from 0 and 2 to reach commit quorum
        for i in [0u32, 2] {
            let msg = commit_signing_message(view, &block_hash);
            let sig = sks[i as usize].sign(&msg);
            let cv = CommitVote { view, block_hash, voter: i, signature: sig };
            engine
                .process_event(ConsensusEvent::Message(ConsensusMessage::CommitVote(cv)))
                .expect("commit vote should succeed");
        }

        // Block should be committed
        assert_eq!(engine.current_view(), 2, "should advance to view 2");
        let mut outputs = vec![];
        while let Ok(o) = rx.try_recv() {
            outputs.push(o);
        }
        let has_decide = outputs.iter().any(|o| matches!(o, EngineOutput::BroadcastMessage(ConsensusMessage::Decide(_))));
        assert!(has_decide, "should broadcast Decide despite invalid vote from validator 3");
        let has_committed = outputs.iter().any(|o| matches!(o, EngineOutput::BlockCommitted { .. }));
        assert!(has_committed, "block should be committed despite invalid vote from validator 3");
    }

    // ── Decide message tests ──

    /// Helper: build a valid CommitQC for testing Decide messages.
    fn build_test_commit_qc(
        view: ViewNumber,
        block_hash: B256,
        sks: &[BlsSecretKey],
        vs: &ValidatorSet,
        signers: &[u32],
    ) -> QuorumCertificate {
        use crate::protocol::quorum::{VoteCollector, commit_signing_message};
        let mut collector = VoteCollector::new(view, block_hash, vs.len());
        for &i in signers {
            let msg = commit_signing_message(view, &block_hash);
            let sig = sks[i as usize].sign(&msg);
            collector.add_vote(i, sig).unwrap();
        }
        collector.build_qc_with_message(vs, &commit_signing_message(view, &block_hash)).unwrap()
    }

    /// Follower correctly commits and advances view when receiving a valid Decide.
    #[test]
    fn test_decide_advances_follower() {
        use n42_primitives::consensus::Decide;

        // 4 validators, follower is validator 0, view 1
        let (mut engine, sks, vs, mut rx) = make_engine(4, 0);
        let block_hash = B256::repeat_byte(0xD1);
        let view = 1u64;

        assert_eq!(engine.current_view(), 1);

        // Build a valid CommitQC with 3 signers (quorum)
        let commit_qc = build_test_commit_qc(view, block_hash, &sks, &vs, &[0, 1, 2]);

        let decide = Decide {
            view,
            block_hash,
            commit_qc,
        };

        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Decide(decide)))
            .expect("valid Decide should succeed");

        // Follower should advance to view 2
        assert_eq!(engine.current_view(), 2, "follower should advance to view 2");
        assert_eq!(engine.current_phase(), Phase::WaitingForProposal);

        // Should have emitted BlockCommitted
        let mut outputs = vec![];
        while let Ok(o) = rx.try_recv() {
            outputs.push(o);
        }
        let has_committed = outputs.iter().any(|o| matches!(
            o,
            EngineOutput::BlockCommitted { block_hash: h, view: v, .. }
            if *h == block_hash && *v == view
        ));
        assert!(has_committed, "follower should emit BlockCommitted after Decide");
    }

    /// Stale Decide (view < current) should be silently ignored.
    #[test]
    fn test_decide_stale_ignored() {
        use n42_primitives::consensus::Decide;

        let (mut engine, sks, vs, mut rx) = make_engine(4, 0);
        let block_hash = B256::repeat_byte(0xD2);

        // Advance engine to view 5 by processing valid Decide messages for views 1..=4.
        for v in 1u64..=4 {
            let bh = B256::repeat_byte(v as u8);
            let cqc = build_test_commit_qc(v, bh, &sks, &vs, &[0, 1, 2]);
            let decide = Decide { view: v, block_hash: bh, commit_qc: cqc };
            let _ = engine.process_event(ConsensusEvent::Message(
                ConsensusMessage::Decide(decide),
            ));
            while rx.try_recv().is_ok() {}
        }
        assert_eq!(engine.current_view(), 5);

        // Send a Decide for view 3 (stale)
        let commit_qc = build_test_commit_qc(3, block_hash, &sks, &vs, &[0, 1, 2]);
        let decide = Decide {
            view: 3,
            block_hash,
            commit_qc,
        };

        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Decide(decide)))
            .expect("stale Decide should be ignored without error");

        // Engine should still be at view 5
        assert_eq!(engine.current_view(), 5, "stale Decide should not change view");
    }

    /// Decide with insufficient quorum in CommitQC should be rejected.
    #[test]
    fn test_decide_invalid_qc_rejected() {
        use bitvec::prelude::*;
        use n42_primitives::consensus::Decide;

        // 4 validators, f=1, quorum=3
        let (mut engine, sks, _vs, _rx) = make_engine(4, 0);
        let block_hash = B256::repeat_byte(0xD3);
        let view = 1u64;

        // Manually construct a QC with only 2 signers (below quorum of 3).
        // We can't use VoteCollector::build_qc_with_message because it also
        // enforces quorum. Instead, build the QC struct directly.
        let mut signers = bitvec![u8, Msb0; 0; 4];
        signers.set(0, true);
        signers.set(1, true);
        // 2 signers out of 4 (quorum = 3)

        let weak_qc = QuorumCertificate {
            view,
            block_hash,
            aggregate_signature: sks[0].sign(b"dummy"),
            signers,
        };

        let decide = Decide {
            view,
            block_hash,
            commit_qc: weak_qc,
        };

        let result = engine.process_event(ConsensusEvent::Message(
            ConsensusMessage::Decide(decide),
        ));
        assert!(result.is_err(), "Decide with insufficient quorum should be rejected");
        match result.unwrap_err() {
            ConsensusError::InsufficientVotes { have, need, .. } => {
                assert_eq!(have, 2);
                assert_eq!(need, 3);
            }
            other => panic!("expected InsufficientVotes, got: {:?}", other),
        }
    }

    /// Decide with a forged CommitQC signature should be rejected (S1 fix).
    /// The QC has correct signer count, view, and block_hash but the aggregate
    /// signature does not match the commit_signing_message.
    #[test]
    fn test_decide_rejects_forged_signature() {
        use bitvec::prelude::*;
        use n42_primitives::consensus::Decide;

        let (mut engine, sks, _vs, _rx) = make_engine(4, 0);
        let block_hash = B256::repeat_byte(0xD4);
        let view = 1u64;

        // Build a QC with 3 signers but signed with WRONG message (prepare format instead of commit)
        let wrong_msg = crate::protocol::quorum::signing_message(view, &block_hash);
        let sigs: Vec<_> = (0..3u32).map(|i| sks[i as usize].sign(&wrong_msg)).collect();
        let sig_refs: Vec<_> = sigs.iter().collect();
        let agg_sig = n42_primitives::bls::AggregateSignature::aggregate(&sig_refs).unwrap();

        let mut signers = bitvec![u8, Msb0; 0; 4];
        signers.set(0, true);
        signers.set(1, true);
        signers.set(2, true);

        let forged_qc = QuorumCertificate {
            view,
            block_hash,
            aggregate_signature: agg_sig,
            signers,
        };

        let decide = Decide {
            view,
            block_hash,
            commit_qc: forged_qc,
        };

        let result = engine.process_event(ConsensusEvent::Message(
            ConsensusMessage::Decide(decide),
        ));
        assert!(result.is_err(), "Decide with forged CommitQC signature should be rejected");
        match result.unwrap_err() {
            ConsensusError::InvalidQC { view: v, reason } => {
                assert_eq!(v, view);
                assert!(reason.contains("commit QC"), "error should mention commit QC, got: {reason}");
            }
            other => panic!("expected InvalidQC, got: {:?}", other),
        }
    }

    #[test]
    fn test_block_imported_hash_mismatch_caches_and_restores() {
        let (mut engine, sks, _, mut rx) = make_engine(4, 0);
        let proposal_hash = B256::repeat_byte(0xAA);
        let other_hash = B256::repeat_byte(0xBB);

        // Receive a valid proposal → defers vote
        let msg = signing_message(1, &proposal_hash);
        let sig = sks[1].sign(&msg);
        let proposal = Proposal {
            view: 1,
            block_hash: proposal_hash,
            justify_qc: QuorumCertificate::genesis(),
            proposer: 1,
            signature: sig,
            prepare_qc: None,
        };
        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Proposal(proposal)))
            .expect("proposal should succeed");
        while rx.try_recv().is_ok() {}
        assert!(engine.pending_proposal.is_some());

        // BlockImported arrives but for a DIFFERENT hash
        engine
            .process_event(ConsensusEvent::BlockImported(other_hash))
            .expect("mismatched BlockImported should succeed");

        // pending_proposal should be restored (not consumed)
        assert!(engine.pending_proposal.is_some(), "pending proposal should be restored");
        assert_eq!(
            engine.pending_proposal.as_ref().unwrap().block_hash,
            proposal_hash
        );
        // The other hash should be cached for future use
        assert!(
            engine.imported_blocks.contains(&other_hash),
            "mismatched hash should be cached in imported_blocks"
        );
    }

    #[test]
    fn test_imported_blocks_capacity_bound() {
        let (mut engine, _sks, _, _rx) = make_engine(4, 0);

        // Insert 32 blocks (at the limit)
        for i in 0..32u8 {
            engine
                .process_event(ConsensusEvent::BlockImported(B256::repeat_byte(i)))
                .expect("BlockImported should succeed");
        }
        assert_eq!(engine.imported_blocks.len(), 32);

        // 33rd should be silently dropped (capacity guard)
        engine
            .process_event(ConsensusEvent::BlockImported(B256::repeat_byte(0xFF)))
            .expect("BlockImported at capacity should succeed");
        assert_eq!(engine.imported_blocks.len(), 32, "should not exceed 32 entries");
        assert!(
            !engine.imported_blocks.contains(&B256::repeat_byte(0xFF)),
            "overflow entry should be dropped"
        );
    }

    // ── QC-based view jump tests ──

    /// Helper: build a valid PrepareQC (signing_message-based) for testing.
    fn build_test_prepare_qc(
        view: ViewNumber,
        block_hash: B256,
        sks: &[BlsSecretKey],
        vs: &ValidatorSet,
        signers: &[u32],
    ) -> QuorumCertificate {
        use crate::protocol::quorum::{VoteCollector, signing_message};
        let mut collector = VoteCollector::new(view, block_hash, vs.len());
        for &i in signers {
            let msg = signing_message(view, &block_hash);
            let sig = sks[i as usize].sign(&msg);
            collector.add_vote(i, sig).unwrap();
        }
        collector.build_qc(vs).unwrap()
    }

    /// Far-future Proposal with valid QC triggers view jump.
    #[test]
    fn test_far_future_proposal_triggers_view_jump() {
        // Engine at view 1, receive Proposal at view 100 (far beyond window=50)
        let (mut engine, sks, vs, mut rx) = make_engine(4, 0);
        assert_eq!(engine.current_view(), 1);

        let far_view = 100u64;
        let block_hash = B256::repeat_byte(0xF0);

        // Build a valid QC for view 99 (justify_qc in the proposal)
        let justify_qc = build_test_prepare_qc(99, B256::repeat_byte(0xEF), &sks, &vs, &[0, 1, 2]);

        // Leader for view 100: 100 % 4 = 0
        let msg = signing_message(far_view, &block_hash);
        let sig = sks[0].sign(&msg);
        let proposal = Proposal {
            view: far_view,
            block_hash,
            justify_qc,
            proposer: 0,
            signature: sig,
            prepare_qc: None,
        };

        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Proposal(proposal)))
            .expect("far-future proposal with valid QC should trigger view jump");

        // Engine should have jumped to view 100 (max(99+1, 100) = 100)
        assert_eq!(engine.current_view(), far_view, "should jump to view 100");

        // Should have emitted SyncRequired and ViewChanged
        let mut outputs = vec![];
        while let Ok(o) = rx.try_recv() {
            outputs.push(o);
        }
        let has_sync = outputs.iter().any(|o| matches!(
            o,
            EngineOutput::SyncRequired { local_view: 1, target_view: 100 }
        ));
        assert!(has_sync, "should emit SyncRequired");
        let has_view_changed = outputs.iter().any(|o| matches!(
            o,
            EngineOutput::ViewChanged { new_view: 100 }
        ));
        assert!(has_view_changed, "should emit ViewChanged");
    }

    /// Far-future Timeout with valid high_qc triggers view jump.
    #[test]
    fn test_far_future_timeout_triggers_view_jump() {
        let (mut engine, sks, vs, mut rx) = make_engine(4, 0);
        assert_eq!(engine.current_view(), 1);

        let far_view = 200u64;
        // Build a valid QC for view 190
        let high_qc = build_test_prepare_qc(190, B256::repeat_byte(0xBE), &sks, &vs, &[0, 1, 2]);

        let msg = crate::protocol::quorum::timeout_signing_message(far_view);
        let sig = sks[2].sign(&msg);
        let timeout = TimeoutMessage {
            view: far_view,
            high_qc,
            sender: 2,
            signature: sig,
        };

        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Timeout(timeout)))
            .expect("far-future timeout with valid QC should trigger view jump");

        // target = max(190+1, 200) = 200
        assert_eq!(engine.current_view(), 200, "should jump to view 200");
        while rx.try_recv().is_ok() {}
    }

    /// Far-future Vote (no QC) should be silently dropped.
    #[test]
    fn test_far_future_vote_without_qc_is_dropped() {
        let (mut engine, sks, _, _rx) = make_engine(4, 0);
        assert_eq!(engine.current_view(), 1);

        let far_view = 100u64;
        let block_hash = B256::repeat_byte(0xAA);
        let msg = signing_message(far_view, &block_hash);
        let sig = sks[2].sign(&msg);

        let vote = Vote {
            view: far_view,
            block_hash,
            voter: 2,
            signature: sig,
        };

        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Vote(vote)))
            .expect("far-future vote should be silently dropped");

        // Engine should remain at view 1
        assert_eq!(engine.current_view(), 1, "vote without QC should not trigger view jump");
    }

    /// Far-future message with invalid QC should be silently dropped.
    #[test]
    fn test_far_future_invalid_qc_is_dropped() {
        use bitvec::prelude::*;

        let (mut engine, sks, _, _rx) = make_engine(4, 0);
        assert_eq!(engine.current_view(), 1);

        let far_view = 100u64;
        let block_hash = B256::repeat_byte(0xBB);

        // Build a forged QC: valid-looking bitmap but wrong aggregate signature
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

        // Leader for view 100: 100 % 4 = 0
        let msg = signing_message(far_view, &block_hash);
        let sig = sks[0].sign(&msg);
        let proposal = Proposal {
            view: far_view,
            block_hash,
            justify_qc: forged_qc,
            proposer: 0,
            signature: sig,
            prepare_qc: None,
        };

        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Proposal(proposal)))
            .expect("far-future proposal with invalid QC should be silently dropped");

        assert_eq!(engine.current_view(), 1, "invalid QC should not trigger view jump");
    }

    /// Far-future message with genesis QC (view 0) should be dropped.
    #[test]
    fn test_far_future_genesis_qc_is_dropped() {
        let (mut engine, sks, _, _rx) = make_engine(4, 0);
        assert_eq!(engine.current_view(), 1);

        let far_view = 100u64;
        let block_hash = B256::repeat_byte(0xCC);

        // Proposal with genesis QC (view 0) — no real signatures to verify
        let msg = signing_message(far_view, &block_hash);
        let sig = sks[0].sign(&msg);
        let proposal = Proposal {
            view: far_view,
            block_hash,
            justify_qc: QuorumCertificate::genesis(),
            proposer: 0,
            signature: sig,
            prepare_qc: None,
        };

        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Proposal(proposal)))
            .expect("far-future proposal with genesis QC should be silently dropped");

        assert_eq!(engine.current_view(), 1, "genesis QC should not trigger view jump");
    }

    /// View jump should reset consecutive_timeouts to 0.
    #[test]
    fn test_view_jump_resets_consecutive_timeouts() {
        let (mut engine, sks, vs, mut rx) = make_engine(4, 0);

        // Trigger a timeout to set consecutive_timeouts > 0.
        // on_timeout() is idempotent per view, so only one fires.
        engine.on_timeout().expect("timeout 1");
        while rx.try_recv().is_ok() {}
        assert!(engine.consecutive_timeouts() >= 1);

        let far_view = 100u64;
        let block_hash = B256::repeat_byte(0xDD);
        let justify_qc = build_test_prepare_qc(99, B256::repeat_byte(0xDE), &sks, &vs, &[0, 1, 2]);

        // Leader for view 100: 100 % 4 = 0
        let msg = signing_message(far_view, &block_hash);
        let sig = sks[0].sign(&msg);
        let proposal = Proposal {
            view: far_view,
            block_hash,
            justify_qc,
            proposer: 0,
            signature: sig,
            prepare_qc: None,
        };

        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Proposal(proposal)))
            .expect("should trigger view jump");

        assert_eq!(engine.current_view(), 100);
        assert_eq!(
            engine.consecutive_timeouts(), 0,
            "consecutive_timeouts should be 0 after view jump"
        );
    }

    /// View jump should update locked_qc to the QC's view.
    #[test]
    fn test_view_jump_updates_locked_qc() {
        let (mut engine, sks, vs, mut rx) = make_engine(4, 0);
        assert_eq!(engine.locked_qc().view, 0, "initially locked on genesis");

        let far_view = 100u64;
        let block_hash = B256::repeat_byte(0xEE);
        let justify_qc = build_test_prepare_qc(95, B256::repeat_byte(0x95), &sks, &vs, &[0, 1, 2]);

        let msg = signing_message(far_view, &block_hash);
        let sig = sks[0].sign(&msg);
        let proposal = Proposal {
            view: far_view,
            block_hash,
            justify_qc,
            proposer: 0,
            signature: sig,
            prepare_qc: None,
        };

        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Proposal(proposal)))
            .expect("should trigger view jump");

        while rx.try_recv().is_ok() {}
        assert_eq!(engine.locked_qc().view, 95, "locked_qc should update to QC's view (95)");
    }

    /// View jump should NOT downgrade locked_qc.
    /// Uses a Timeout message to trigger jump (Timeout dispatch only checks current view
    /// match, avoiding the SafetyViolation that Proposal dispatch would trigger).
    #[test]
    fn test_view_jump_does_not_downgrade_locked_qc() {
        let (mut engine, sks, vs, mut rx) = make_engine(4, 0);

        // Advance to view 3 via commits, setting locked_qc along the way
        let commit_qc = build_test_commit_qc(1, B256::repeat_byte(0x01), &sks, &vs, &[0, 1, 2]);
        let decide = Decide { view: 1, block_hash: B256::repeat_byte(0x01), commit_qc };
        engine.process_event(ConsensusEvent::Message(ConsensusMessage::Decide(decide))).unwrap();
        while rx.try_recv().is_ok() {}

        let commit_qc2 = build_test_commit_qc(2, B256::repeat_byte(0x02), &sks, &vs, &[0, 1, 2]);
        let decide2 = Decide { view: 2, block_hash: B256::repeat_byte(0x02), commit_qc: commit_qc2 };
        engine.process_event(ConsensusEvent::Message(ConsensusMessage::Decide(decide2))).unwrap();
        while rx.try_recv().is_ok() {}

        // Manually set locked_qc to view 50
        let high_qc = build_test_prepare_qc(50, B256::repeat_byte(0x50), &sks, &vs, &[0, 1, 2]);
        engine.round_state.update_locked_qc(&high_qc);
        assert_eq!(engine.locked_qc().view, 50);

        // Send a far-future Timeout with a *lower* high_qc (view 30).
        // The QC is valid so the jump succeeds, but locked_qc should NOT downgrade.
        let far_view = 200u64;
        let low_qc = build_test_prepare_qc(30, B256::repeat_byte(0x30), &sks, &vs, &[0, 1, 2]);
        let msg = crate::protocol::quorum::timeout_signing_message(far_view);
        let sig = sks[2].sign(&msg);
        let timeout = TimeoutMessage {
            view: far_view,
            high_qc: low_qc,
            sender: 2,
            signature: sig,
        };

        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Timeout(timeout)))
            .expect("should trigger view jump via timeout");

        while rx.try_recv().is_ok() {}
        assert_eq!(engine.current_view(), 200, "should jump to view 200");
        assert_eq!(
            engine.locked_qc().view, 50,
            "locked_qc should NOT downgrade from 50 to 30"
        );
    }

    /// New window=50 should buffer messages within range.
    #[test]
    fn test_increased_future_view_window_buffers_50() {
        let (mut engine, sks, _, _rx) = make_engine(4, 0);
        assert_eq!(engine.current_view(), 1);

        // View 51 = 1 + 50, should be buffered (within window)
        let view = 51u64;
        let block_hash = B256::repeat_byte(0xFF);
        let msg = signing_message(view, &block_hash);
        let sig = sks[2].sign(&msg);
        let vote = Vote {
            view,
            block_hash,
            voter: 2,
            signature: sig,
        };

        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Vote(vote)))
            .expect("vote at view 51 should be buffered");

        assert_eq!(engine.future_msg_buffer.len(), 1, "should have 1 buffered message");
        assert_eq!(engine.future_msg_buffer[0].0, 51, "buffered message should be at view 51");
    }

    /// Decide should still use its direct bypass path (not the view jump path).
    #[test]
    fn test_decide_bypasses_view_jump_path() {
        let (mut engine, sks, vs, mut rx) = make_engine(4, 0);
        assert_eq!(engine.current_view(), 1);

        // Send a Decide far in the future (view 100)
        let far_view = 100u64;
        let block_hash = B256::repeat_byte(0xAB);
        let commit_qc = build_test_commit_qc(far_view, block_hash, &sks, &vs, &[0, 1, 2]);
        let decide = Decide {
            view: far_view,
            block_hash,
            commit_qc,
        };

        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Decide(decide)))
            .expect("Decide should use direct bypass path");

        // Decide bypasses the future-view buffer entirely → advances to 101
        assert_eq!(engine.current_view(), 101, "Decide should advance view to 101");

        let mut outputs = vec![];
        while let Ok(o) = rx.try_recv() {
            outputs.push(o);
        }
        // Should have SyncRequired (gap > 3) and BlockCommitted
        let has_sync = outputs.iter().any(|o| matches!(o, EngineOutput::SyncRequired { .. }));
        assert!(has_sync, "far-future Decide should emit SyncRequired");
        let has_committed = outputs.iter().any(|o| matches!(o, EngineOutput::BlockCommitted { .. }));
        assert!(has_committed, "Decide should emit BlockCommitted");
    }

    // ══════════════════════════════════════════════════════════════════
    // Audit-round tests (BUG-A/B/C verification + coverage)
    // ══════════════════════════════════════════════════════════════════

    /// Test 1: Second on_timeout in the same view is a no-op (no extra
    /// consecutive_timeouts increment, no second Timeout broadcast).
    #[test]
    fn test_on_timeout_repeat_is_noop() {
        let (mut engine, _, _, mut rx) = make_engine(4, 0);

        // First timeout
        engine.on_timeout().expect("first timeout");
        let t1 = engine.consecutive_timeouts();
        let mut outputs = vec![];
        while let Ok(o) = rx.try_recv() { outputs.push(o); }
        let broadcast_count_1 = outputs.iter()
            .filter(|o| matches!(o, EngineOutput::BroadcastMessage(ConsensusMessage::Timeout(_))))
            .count();
        assert_eq!(broadcast_count_1, 1, "first timeout should broadcast exactly 1 Timeout");

        // Second timeout (repeat) — should NOT increment or broadcast
        engine.on_timeout().expect("repeat timeout");
        assert_eq!(
            engine.consecutive_timeouts(), t1,
            "repeat on_timeout should not increment consecutive_timeouts"
        );
        let mut outputs = vec![];
        while let Ok(o) = rx.try_recv() { outputs.push(o); }
        let broadcast_count_2 = outputs.iter()
            .filter(|o| matches!(o, EngineOutput::BroadcastMessage(ConsensusMessage::Timeout(_))))
            .count();
        assert_eq!(broadcast_count_2, 0, "repeat on_timeout should not broadcast");
    }

    /// Test 2: Timeout at exactly the FUTURE_VIEW_WINDOW boundary is processed.
    #[test]
    fn test_process_timeout_at_window_boundary() {
        use crate::protocol::quorum::timeout_signing_message;

        let (mut engine, sks, _, mut rx) = make_engine(4, 0);
        assert_eq!(engine.current_view(), 1);

        // timeout.view = 1 + FUTURE_VIEW_WINDOW = 51 (exactly at boundary)
        let boundary_view = 1 + FUTURE_VIEW_WINDOW;
        let msg = timeout_signing_message(boundary_view);
        let sig = sks[2].sign(&msg);
        let timeout = TimeoutMessage {
            view: boundary_view,
            high_qc: QuorumCertificate::genesis(),
            sender: 2,
            signature: sig,
        };

        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Timeout(timeout)))
            .expect("timeout at window boundary should be accepted");

        // Engine should have advanced to the boundary view
        assert_eq!(
            engine.current_view(), boundary_view,
            "should advance to boundary view"
        );
        while rx.try_recv().is_ok() {}
    }

    /// Test 3: Timeout beyond FUTURE_VIEW_WINDOW is silently dropped.
    #[test]
    fn test_process_timeout_beyond_window_dropped() {
        use crate::protocol::quorum::timeout_signing_message;

        let (mut engine, sks, _, _rx) = make_engine(4, 0);
        assert_eq!(engine.current_view(), 1);

        // timeout.view = 1 + FUTURE_VIEW_WINDOW + 1 = 52 (beyond boundary)
        let beyond_view = 1 + FUTURE_VIEW_WINDOW + 1;
        let msg = timeout_signing_message(beyond_view);
        let sig = sks[2].sign(&msg);
        let timeout = TimeoutMessage {
            view: beyond_view,
            high_qc: QuorumCertificate::genesis(),
            sender: 2,
            signature: sig,
        };

        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Timeout(timeout)))
            .expect("timeout beyond window should be silently dropped");

        assert_eq!(engine.current_view(), 1, "view should not change");
    }

    /// Test 4: Four validators, 3 external timeouts → TC formed + NewView broadcast.
    #[test]
    fn test_tc_formation_from_external_timeouts() {
        use crate::protocol::quorum::timeout_signing_message;

        // 4 validators, quorum = 3. View 1 leader is 1, so next leader for view 2 is 2.
        // We run on node 2 so it becomes the TC-forming leader.
        let (mut engine, sks, _, mut rx) = make_engine(4, 2);
        assert_eq!(engine.current_view(), 1);

        // First, timeout on our own node so it enters TimedOut phase + sends own Timeout
        engine.on_timeout().expect("own timeout");
        while rx.try_recv().is_ok() {}

        // Send timeout from node 0 (already verified: signature matches)
        let msg = timeout_signing_message(1);
        let sig0 = sks[0].sign(&msg);
        let timeout0 = TimeoutMessage {
            view: 1,
            high_qc: QuorumCertificate::genesis(),
            sender: 0,
            signature: sig0,
        };
        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Timeout(timeout0)))
            .expect("timeout from 0");

        // Send timeout from node 1 → should reach quorum (self + 0 + 1 = 3)
        let sig1 = sks[1].sign(&msg);
        let timeout1 = TimeoutMessage {
            view: 1,
            high_qc: QuorumCertificate::genesis(),
            sender: 1,
            signature: sig1,
        };
        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Timeout(timeout1)))
            .expect("timeout from 1 should form TC");

        // Collect outputs — should have NewView broadcast
        let mut outputs = vec![];
        while let Ok(o) = rx.try_recv() { outputs.push(o); }
        let has_new_view = outputs.iter().any(|o| matches!(
            o, EngineOutput::BroadcastMessage(ConsensusMessage::NewView(_))
        ));
        assert!(has_new_view, "should broadcast NewView after TC formation");

        // Engine should advance to view 2
        assert_eq!(engine.current_view(), 2, "should advance to view 2 after TC");
    }

    /// Test 5: Non-leader node should NOT broadcast NewView even with quorum timeouts.
    #[test]
    fn test_tc_non_leader_no_newview() {
        use crate::protocol::quorum::timeout_signing_message;

        // 4 validators, view 1. Next leader (view 2) = 2 % 4 = 2.
        // We run on node 0 — NOT the next leader.
        let (mut engine, sks, _, mut rx) = make_engine(4, 0);

        engine.on_timeout().expect("own timeout");
        while rx.try_recv().is_ok() {}

        // Deliver timeouts from nodes 1 and 2 → quorum reached (0 + 1 + 2 = 3)
        let msg = timeout_signing_message(1);
        for &i in &[1u32, 2] {
            let sig = sks[i as usize].sign(&msg);
            let timeout = TimeoutMessage {
                view: 1,
                high_qc: QuorumCertificate::genesis(),
                sender: i,
                signature: sig,
            };
            engine
                .process_event(ConsensusEvent::Message(ConsensusMessage::Timeout(timeout)))
                .expect("timeout should succeed");
        }

        let mut outputs = vec![];
        while let Ok(o) = rx.try_recv() { outputs.push(o); }

        // Should NOT have NewView (we're not the next leader)
        let has_new_view = outputs.iter().any(|o| matches!(
            o, EngineOutput::BroadcastMessage(ConsensusMessage::NewView(_))
        ));
        assert!(!has_new_view, "non-leader should NOT broadcast NewView");

        // Engine should still be at view 1 (no advance without NewView processing)
        assert_eq!(engine.current_view(), 1, "non-leader should remain at view 1");
    }

    /// Test 6: Buffered messages are replayed when engine advances to their view.
    /// Uses a Vote message (which IS buffered, unlike Decide/NewView which are exempt).
    #[test]
    fn test_advance_to_view_replays_buffered() {
        let (mut engine, sks, vs, mut rx) = make_engine(4, 0);
        assert_eq!(engine.current_view(), 1);

        // Buffer a Vote for view 5 (within FUTURE_VIEW_WINDOW, non-exempt)
        let view5_hash = B256::repeat_byte(0x55);
        let msg = signing_message(5, &view5_hash);
        let sig = sks[2].sign(&msg);
        let vote = n42_primitives::consensus::Vote {
            view: 5,
            block_hash: view5_hash,
            voter: 2,
            signature: sig,
        };
        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Vote(vote)))
            .expect("future Vote should be buffered");

        assert_eq!(engine.current_view(), 1, "should still be at view 1");
        assert_eq!(engine.future_msg_buffer.len(), 1, "should have 1 buffered message");
        assert_eq!(engine.future_msg_buffer[0].0, 5, "buffered at view 5");

        // Advance to view 5 by processing Decide messages for views 1-4
        for v in 1u64..=4 {
            let bh = B256::repeat_byte(v as u8);
            let cqc = build_test_commit_qc(v, bh, &sks, &vs, &[0, 1, 2]);
            let d = n42_primitives::consensus::Decide { view: v, block_hash: bh, commit_qc: cqc };
            let _ = engine.process_event(ConsensusEvent::Message(ConsensusMessage::Decide(d)));
            while rx.try_recv().is_ok() {}
        }

        // After advancing to view 5, the buffered Vote should have been replayed.
        // The buffer should now be empty (view 5 message consumed).
        assert_eq!(engine.current_view(), 5, "should be at view 5");
        assert!(
            engine.future_msg_buffer.is_empty(),
            "buffer should be empty after replay"
        );
    }

    /// Test 7 (BUG-B verification): advance_to_view with a lower view is a no-op.
    #[test]
    fn test_advance_to_view_monotonic() {
        let (mut engine, sks, vs, mut rx) = make_engine(4, 0);

        // Advance to view 10 via Decide messages
        for v in 1u64..=9 {
            let bh = B256::repeat_byte(v as u8);
            let cqc = build_test_commit_qc(v, bh, &sks, &vs, &[0, 1, 2]);
            let d = n42_primitives::consensus::Decide { view: v, block_hash: bh, commit_qc: cqc };
            let _ = engine.process_event(ConsensusEvent::Message(ConsensusMessage::Decide(d)));
        }
        while rx.try_recv().is_ok() {}
        assert_eq!(engine.current_view(), 10);

        // Call advance_to_view with a lower view — should be no-op
        engine.advance_to_view(5);
        assert_eq!(engine.current_view(), 10, "advance_to_view(5) should be no-op when at view 10");

        // Call advance_to_view with the same view — should be no-op
        engine.advance_to_view(10);
        assert_eq!(engine.current_view(), 10, "advance_to_view(10) should be no-op when at view 10");

        // Call advance_to_view with a higher view — should advance
        engine.advance_to_view(15);
        assert_eq!(engine.current_view(), 15, "advance_to_view(15) should advance to 15");
    }

    /// Test 8: Buffer eviction removes messages with the lowest view when full.
    /// Only views within FUTURE_VIEW_WINDOW are buffered; messages beyond are
    /// handled by the QC-jump path. We send multiple messages per view to
    /// exceed MAX_FUTURE_MESSAGES within the 50-view window.
    #[test]
    fn test_buffer_eviction_removes_lowest_view() {
        let (mut engine, sks, _, _rx) = make_engine(4, 0);
        assert_eq!(engine.current_view(), 1);

        // Window allows views 2..=51. Send 2 Votes per view (from different
        // validators) to fill the buffer: 2 * 33 = 66 > MAX_FUTURE_MESSAGES(64).
        let voters = [1u32, 2];
        for v in 2u64..=34 {
            for &voter in &voters {
                let block_hash = B256::repeat_byte(v as u8);
                let msg = signing_message(v, &block_hash);
                let sig = sks[voter as usize].sign(&msg);
                let vote = n42_primitives::consensus::Vote {
                    view: v,
                    block_hash,
                    voter,
                    signature: sig,
                };
                engine
                    .process_event(ConsensusEvent::Message(ConsensusMessage::Vote(vote)))
                    .expect("buffering should succeed");
            }
        }

        // Buffer should be capped at MAX_FUTURE_MESSAGES
        assert_eq!(
            engine.future_msg_buffer.len(),
            MAX_FUTURE_MESSAGES,
            "buffer should be exactly at MAX_FUTURE_MESSAGES"
        );

        // Lowest view in buffer should be >= 3 (view 2 entries should have been evicted)
        let min_view = engine.future_msg_buffer.iter().map(|(v, _)| *v).min().unwrap_or(0);
        assert!(
            min_view >= 3,
            "lowest view in buffer should be >= 3 after eviction, got {min_view}"
        );
    }

    /// Test 9: QC jump emits SyncRequired with correct local_view and target_view.
    #[test]
    fn test_qc_jump_emits_sync_required() {
        let (mut engine, sks, vs, mut rx) = make_engine(4, 0);
        assert_eq!(engine.current_view(), 1);

        let far_view = 80u64;
        let block_hash = B256::repeat_byte(0xAB);
        let justify_qc = build_test_prepare_qc(79, B256::repeat_byte(0x79), &sks, &vs, &[0, 1, 2]);

        // Leader for view 80: 80 % 4 = 0
        let msg = signing_message(far_view, &block_hash);
        let sig = sks[0].sign(&msg);
        let proposal = Proposal {
            view: far_view,
            block_hash,
            justify_qc,
            proposer: 0,
            signature: sig,
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
        assert!(sync.is_some(), "should emit SyncRequired");
        let (local, target) = sync.unwrap();
        assert_eq!(local, 1, "local_view should be 1 (pre-jump)");
        assert_eq!(target, 80, "target_view should be 80 (max of qc.view+1=80, msg_view=80)");
    }

    /// Test 10 (BUG-A verification): ViewChanged after advance_to_view reports
    /// the actual current_view(), not a pre-cached target value.
    ///
    /// This validates the BUG-A fix: all three callsites now do
    ///   `let actual = self.round_state.current_view();`
    ///   `self.emit(ViewChanged { new_view: actual });`
    /// instead of using the pre-advance target value.
    ///
    /// We trigger a QC-based view jump and verify the ViewChanged payload
    /// matches current_view() exactly.
    #[test]
    fn test_stale_view_changed_prevented() {
        let (mut engine, sks, vs, mut rx) = make_engine(4, 0);
        assert_eq!(engine.current_view(), 1);

        // Trigger a QC-based view jump to view 80 via a far-future Proposal.
        let justify_qc = build_test_prepare_qc(79, B256::repeat_byte(0x79), &sks, &vs, &[0, 1, 2]);
        // Leader for view 80: 80 % 4 = 0
        let msg = signing_message(80, &B256::repeat_byte(0xAB));
        let sig = sks[0].sign(&msg);
        let proposal = Proposal {
            view: 80,
            block_hash: B256::repeat_byte(0xAB),
            justify_qc,
            proposer: 0,
            signature: sig,
            prepare_qc: None,
        };

        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Proposal(proposal)))
            .expect("should trigger QC jump to view 80");

        let actual_view = engine.current_view();
        assert!(actual_view >= 80, "should be at view >= 80, got {actual_view}");

        // Collect all ViewChanged events
        let mut outputs = vec![];
        while let Ok(o) = rx.try_recv() { outputs.push(o); }

        let view_changed_values: Vec<ViewNumber> = outputs.iter().filter_map(|o| match o {
            EngineOutput::ViewChanged { new_view } => Some(*new_view),
            _ => None,
        }).collect();

        // There should be at least one ViewChanged
        assert!(
            !view_changed_values.is_empty(),
            "should have emitted at least one ViewChanged"
        );

        // The last ViewChanged should match the actual current_view.
        // Before BUG-A fix, this would report the pre-advance target (80)
        // even if advance_to_view's replay pushed the view further.
        let last_vc = *view_changed_values.last().unwrap();
        assert_eq!(
            last_vc, actual_view,
            "ViewChanged should report actual current_view ({}), got {}",
            actual_view, last_vc
        );
    }
}
