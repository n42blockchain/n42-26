use alloy_primitives::B256;
use n42_primitives::{
    BlsSecretKey,
    consensus::{ConsensusMessage, QuorumCertificate, ViewNumber},
};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;

use super::bounded_fifo::BoundedFifoMap;
use super::pacemaker::Pacemaker;
use super::quorum::{
    TimeoutCollector, VoteCollector, commit_signing_message, newview_signing_message,
    signing_message, timeout_signing_message,
};
use super::round::{Phase, RoundState};
use crate::error::ConsensusResult;
use crate::validator::{EpochManager, LeaderSelector, ValidatorSet};
use crate::vote_log::{NoopVoteLog, VoteLogWriter};

/// Per-view timing tracker for diagnosing consensus commit latency.
#[derive(Debug, Clone)]
pub struct ViewTiming {
    /// When this view started (advance_to_view or engine creation).
    pub view_start: Instant,
    /// Leader: when proposal was broadcast.
    pub proposal_sent: Option<Instant>,
    /// Follower: when proposal was received.
    pub proposal_received: Option<Instant>,
    /// Follower: when vote was sent to leader.
    pub vote_sent: Option<Instant>,
    /// Leader: when PrepareQC was formed (Round 1 quorum reached).
    pub prepare_qc_formed: Option<Instant>,
    /// Follower: when PrepareQC was received and CommitVote sent.
    pub commit_vote_sent: Option<Instant>,
    /// Leader: when CommitQC was formed (Round 2 quorum reached = commit).
    pub commit_qc_formed: Option<Instant>,
    /// Number of Round 1 votes collected when PrepareQC formed.
    pub prepare_vote_count: u32,
    /// Number of Round 2 votes collected when CommitQC formed.
    pub commit_vote_count: u32,
}

impl ViewTiming {
    fn new() -> Self {
        Self {
            view_start: Instant::now(),
            proposal_sent: None,
            proposal_received: None,
            vote_sent: None,
            prepare_qc_formed: None,
            commit_vote_sent: None,
            commit_qc_formed: None,
            prepare_vote_count: 0,
            commit_vote_count: 0,
        }
    }

    /// Returns a human-readable summary of the timing breakdown.
    pub fn summary(&self) -> String {
        let ms = |opt: Option<Instant>| -> String {
            opt.map(|t| format!("{}ms", t.duration_since(self.view_start).as_millis()))
                .unwrap_or_else(|| "-".to_string())
        };

        // Calculate inter-stage deltas
        let prepare_delta = self.prepare_qc_formed.and_then(|pqc| {
            self.proposal_sent
                .map(|p| pqc.duration_since(p).as_millis())
        });
        let commit_delta = self.commit_qc_formed.and_then(|cqc| {
            self.prepare_qc_formed
                .map(|pqc| cqc.duration_since(pqc).as_millis())
        });
        let vote_delta = self.vote_sent.and_then(|v| {
            self.proposal_received
                .map(|p| v.duration_since(p).as_millis())
        });
        let total = self
            .commit_qc_formed
            .map(|t| t.duration_since(self.view_start).as_millis());

        let d = |opt: Option<u128>| -> String {
            opt.map(|v| format!("{v}ms"))
                .unwrap_or_else(|| "-".to_string())
        };

        if self.proposal_sent.is_some() {
            // Leader view
            format!(
                "leader proposal=@{} R1_collect={} R2_collect={} total={} votes={}+{}",
                ms(self.proposal_sent),
                d(prepare_delta),
                d(commit_delta),
                d(total),
                self.prepare_vote_count,
                self.commit_vote_count,
            )
        } else {
            // Follower view
            format!(
                "follower proposal=@{} vote_delay={} commit_vote=@{} total=@{}",
                ms(self.proposal_received),
                d(vote_delta),
                ms(self.commit_vote_sent),
                ms(self.commit_qc_formed),
            )
        }
    }
}

/// Maximum number of views ahead a future message can be to be buffered.
/// Messages beyond this window trigger a QC-based view jump attempt.
///
/// Exposed at the crate root so the orchestrator can rate-limit messages
/// that would force the engine into the view-jump path (which performs an
/// expensive BLS aggregate verification per message).
pub const FUTURE_VIEW_WINDOW: u64 = 50;

/// Maximum number of buffered future-view messages.
/// When exceeded, the oldest (lowest view) messages are evicted.
const MAX_FUTURE_MESSAGES: usize = 256;

/// Events fed into the consensus engine.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)] // Message is the dominant hot-path variant; boxing adds unnecessary allocation
pub enum ConsensusEvent {
    /// A consensus message received from the network.
    Message(ConsensusMessage),
    /// A block has been executed and is ready for proposal (leader path).
    /// Tuple: (block_hash, tx_root_hash).
    BlockReady(B256, Option<B256>),
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
        /// Validator changes committed with this block (if any).
        validator_changes: Option<Vec<n42_primitives::consensus::ValidatorChange>>,
    },
    /// A late Proposal recovered validator changes for an already-committed block.
    /// The orchestrator must patch any cached sync metadata for that block.
    CommittedBlockValidatorChangesRecovered {
        view: ViewNumber,
        block_hash: B256,
        validator_changes: Vec<n42_primitives::consensus::ValidatorChange>,
    },
    /// View change occurred; new view started.
    ViewChanged { new_view: ViewNumber },
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

/// Pending proposal state (retained for timeout cleanup; with optimistic voting,
/// R1 votes are sent immediately and this struct is rarely populated).
#[derive(Debug, Clone)]
pub(super) struct PendingProposal {
    pub(super) _view: ViewNumber,
    pub(super) _block_hash: B256,
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
    /// Cached public key derived from `secret_key` to avoid repeated BLS scalar
    /// multiplications in `local_validator_index_for_view` (called on every message).
    pub(super) local_public_key: n42_primitives::BlsPublicKey,
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
    /// Tracks R1 (Vote) per validator per view for equivocation detection.
    pub(super) equivocation_tracker: HashMap<u32, B256>,
    /// Tracks R2 (CommitVote) per validator per view for equivocation detection.
    pub(super) commit_equivocation_tracker: HashMap<u32, B256>,
    /// Buffer for messages arriving for future views (within FUTURE_VIEW_WINDOW).
    pub(super) future_msg_buffer: Vec<(ViewNumber, ConsensusMessage)>,
    /// Per-view timing for commit latency diagnosis.
    pub(super) view_timing: ViewTiming,
    /// Timing from the last committed view (preserved across advance_to_view).
    last_committed_timing: Option<ViewTiming>,
    /// Pending tx_root_hash values for Baby Raptr DA verification.
    /// Maps block_hash -> tx_root_hash for proposals awaiting verification.
    /// Bounded to 64 entries; oldest evicted when full.
    pub(super) pending_tx_roots: BoundedFifoMap<B256, B256>,
    /// `validator_changes_hash` per pending proposal, captured at process_proposal
    /// (or on_block_ready for the leader). Looked up at R2 commit-vote signing
    /// time so the same `changes_hash` enters the BLS message that
    /// `verify_commit_qc` will check on the receiving side.
    pub(super) pending_changes_hashes: BoundedFifoMap<B256, B256>,
    /// Durable last-voted-view sink, fsync'd before every R1 vote (HotStuff-2
    /// safety invariant — see `crate::vote_log`). Defaults to `NoopVoteLog`
    /// for tests / single-validator dev mode; production builds inject a
    /// file-backed implementation from the orchestrator.
    pub(super) vote_log: Arc<dyn VoteLogWriter>,
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
        Self::with_epoch_manager_and_vote_log(
            my_index,
            secret_key,
            epoch_manager,
            base_timeout_ms,
            max_timeout_ms,
            output_tx,
            Arc::new(NoopVoteLog),
        )
    }

    /// Like [`Self::with_epoch_manager`] but with an explicit vote-log writer.
    /// Production builds inject a file-backed `FileVoteLog` from the orchestrator
    /// so every R1 vote is fsync'd before the signature is broadcast.
    pub fn with_epoch_manager_and_vote_log(
        my_index: u32,
        secret_key: BlsSecretKey,
        epoch_manager: EpochManager,
        base_timeout_ms: u64,
        max_timeout_ms: u64,
        output_tx: mpsc::Sender<EngineOutput>,
        vote_log: Arc<dyn VoteLogWriter>,
    ) -> Self {
        let local_public_key = secret_key.public_key();
        Self {
            my_index,
            secret_key,
            local_public_key,
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
            commit_equivocation_tracker: HashMap::new(),
            future_msg_buffer: Vec::new(),
            view_timing: ViewTiming::new(),
            last_committed_timing: None,
            pending_tx_roots: BoundedFifoMap::new(64),
            pending_changes_hashes: BoundedFifoMap::new(64),
            vote_log,
        }
    }

    /// Creates a consensus engine with recovered state from a persisted snapshot.
    ///
    /// Restores safety invariants (locked_qc, last_committed_qc) so the node
    /// resumes with its previous locking commitments intact. Uses a `NoopVoteLog`
    /// — production callers should use [`Self::with_recovered_state_and_vote_log`].
    #[allow(clippy::too_many_arguments)] // recovery needs all safety-critical fields
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
        last_voted_view: ViewNumber,
    ) -> Self {
        Self::with_recovered_state_and_vote_log(
            my_index,
            secret_key,
            epoch_manager,
            base_timeout_ms,
            max_timeout_ms,
            output_tx,
            recovered_view,
            locked_qc,
            last_committed_qc,
            consecutive_timeouts,
            last_voted_view,
            Arc::new(NoopVoteLog),
        )
    }

    /// Like [`Self::with_recovered_state`] but with an explicit vote-log writer.
    #[allow(clippy::too_many_arguments)]
    pub fn with_recovered_state_and_vote_log(
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
        last_voted_view: ViewNumber,
        vote_log: Arc<dyn VoteLogWriter>,
    ) -> Self {
        /// Maximum consecutive timeouts preserved from a snapshot to prevent
        /// absurdly long backoff durations on recovery.
        const MAX_RECOVERED_CONSECUTIVE_TIMEOUTS: u32 = 128;

        let safe_consecutive_timeouts =
            consecutive_timeouts.min(MAX_RECOVERED_CONSECUTIVE_TIMEOUTS);
        if safe_consecutive_timeouts != consecutive_timeouts {
            tracing::warn!(target: "n42::cl::engine",
                original = consecutive_timeouts,
                capped = safe_consecutive_timeouts,
                "recovered consecutive_timeouts exceeded sanity limit, capping"
            );
        }
        let local_public_key = secret_key.public_key();
        Self {
            my_index,
            secret_key,
            local_public_key,
            epoch_manager,
            round_state: RoundState::from_snapshot(
                recovered_view,
                locked_qc,
                last_committed_qc,
                safe_consecutive_timeouts,
                last_voted_view,
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
            commit_equivocation_tracker: HashMap::new(),
            future_msg_buffer: Vec::new(),
            view_timing: ViewTiming::new(),
            last_committed_timing: None,
            pending_tx_roots: BoundedFifoMap::new(64),
            pending_changes_hashes: BoundedFifoMap::new(64),
            vote_log,
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

    /// Returns this node's validator index.
    pub fn my_index(&self) -> u32 {
        self.my_index
    }

    pub fn validator_count(&self) -> u32 {
        self.validator_set().len()
    }

    pub fn quorum_size(&self) -> usize {
        self.validator_set().quorum_size()
    }

    /// Returns the validator index if the message carries a valid single-validator
    /// BLS signature under the validator set for that message's view.
    ///
    /// This is intentionally narrower than full consensus acceptance: it only answers
    /// "does this peer control the validator key for this message's view?" so outer layers can bind a
    /// `PeerId` to a validator identity without reusing weaker transport metadata.
    pub fn authenticated_signer(&self, msg: &ConsensusMessage) -> Option<u32> {
        match msg {
            ConsensusMessage::Proposal(proposal) => {
                let pk = self
                    .validator_set_for_view(proposal.view)
                    .get_public_key(proposal.proposer)
                    .ok()?;
                let sig_msg = crate::protocol::quorum::proposal_signing_message(
                    proposal.view,
                    &proposal.block_hash,
                    &proposal.validator_changes,
                );
                if pk
                    .verify_prevalidated(&sig_msg, &proposal.signature)
                    .is_err()
                {
                    tracing::debug!(target: "n42::consensus", view = proposal.view, proposer = proposal.proposer, "proposal signature verification failed");
                    return None;
                }
                Some(proposal.proposer)
            }
            ConsensusMessage::Vote(vote) => {
                let pk = self
                    .validator_set_for_view(vote.view)
                    .get_public_key(vote.voter)
                    .ok()?;
                let sig_msg = signing_message(vote.view, &vote.block_hash);
                if pk.verify_prevalidated(&sig_msg, &vote.signature).is_err() {
                    tracing::debug!(target: "n42::consensus", view = vote.view, voter = vote.voter, "vote signature verification failed");
                    return None;
                }
                Some(vote.voter)
            }
            ConsensusMessage::CommitVote(commit_vote) => {
                let pk = self
                    .validator_set_for_view(commit_vote.view)
                    .get_public_key(commit_vote.voter)
                    .ok()?;
                // authenticated_signer is invoked from the orchestrator before
                // the engine's main message handler runs, so the proposal may
                // not be cached yet. The canonical verification re-runs in
                // process_commit_vote with the same fallback semantics.
                let changes_hash = self.cached_changes_hash(&commit_vote.block_hash);
                let sig_msg = commit_signing_message(
                    commit_vote.view,
                    &commit_vote.block_hash,
                    &changes_hash,
                );
                if pk
                    .verify_prevalidated(&sig_msg, &commit_vote.signature)
                    .is_err()
                {
                    tracing::debug!(target: "n42::consensus", view = commit_vote.view, voter = commit_vote.voter, "commit_vote signature verification failed");
                    return None;
                }
                Some(commit_vote.voter)
            }
            ConsensusMessage::Timeout(timeout) => {
                let pk = self
                    .validator_set_for_view(timeout.view)
                    .get_public_key(timeout.sender)
                    .ok()?;
                let sig_msg = timeout_signing_message(timeout.view);
                if pk
                    .verify_prevalidated(&sig_msg, &timeout.signature)
                    .is_err()
                {
                    tracing::debug!(target: "n42::consensus", view = timeout.view, sender = timeout.sender, "timeout signature verification failed");
                    return None;
                }
                Some(timeout.sender)
            }
            ConsensusMessage::NewView(new_view) => {
                let pk = self
                    .validator_set_for_view(new_view.view)
                    .get_public_key(new_view.leader)
                    .ok()?;
                let sig_msg = newview_signing_message(new_view.view);
                if pk
                    .verify_prevalidated(&sig_msg, &new_view.signature)
                    .is_err()
                {
                    tracing::debug!(target: "n42::consensus", view = new_view.view, leader = new_view.leader, "new_view signature verification failed");
                    return None;
                }
                Some(new_view.leader)
            }
            ConsensusMessage::PrepareQC(_) | ConsensusMessage::Decide(_) => None,
        }
    }

    pub fn epoch_manager(&self) -> &EpochManager {
        &self.epoch_manager
    }

    pub fn epoch_manager_mut(&mut self) -> &mut EpochManager {
        &mut self.epoch_manager
    }

    /// Re-derives `my_index` from the current validator set after an external
    /// epoch advance (e.g. during block sync).  Returns the new index, or `None`
    /// if this node is not in the current set.
    pub fn sync_local_validator_index(&mut self) -> Option<u32> {
        let new_index = self
            .epoch_manager
            .current_validator_set()
            .index_of_public_key(&self.local_public_key);
        if let Some(idx) = new_index {
            if idx != self.my_index {
                tracing::info!(
                    target: "n42::cl::engine",
                    old_index = self.my_index,
                    new_index = idx,
                    "updated my_index after epoch advance (sync)"
                );
                self.my_index = idx;
            }
        } else {
            tracing::warn!(
                target: "n42::cl::engine",
                "local key not in current validator set (sync)"
            );
        }
        new_index
    }

    /// Returns true iff `self.local_public_key` is a validator in this view's set.
    /// Used to gate consensus broadcasts so observers (or nodes whose `my_index`
    /// has not yet been re-derived after an epoch transition) do not sign messages
    /// under a foreign sender index, which would fail BLS verification on every
    /// receiver and stall the network.
    pub(super) fn is_local_validator_active_for_view(&self, view: ViewNumber) -> bool {
        self.local_validator_index_for_view(view).is_some()
    }

    /// Proposes adding a new validator, to be committed at the next CommitQC.
    ///
    /// See [`EpochManager::propose_add_validator`] for safety constraints.
    pub fn propose_add_validator(
        &mut self,
        info: n42_chainspec::ValidatorInfo,
    ) -> crate::error::ConsensusResult<()> {
        self.epoch_manager.propose_add_validator(info)
    }

    /// Proposes removing a validator, to be committed at the next CommitQC.
    ///
    /// See [`EpochManager::propose_remove_validator`] for safety constraints.
    pub fn propose_remove_validator(
        &mut self,
        addr: alloy_primitives::Address,
    ) -> crate::error::ConsensusResult<()> {
        self.epoch_manager.propose_remove_validator(addr)
    }

    /// Returns the timing from the last committed view.
    pub fn last_committed_view_timing(&self) -> Option<&ViewTiming> {
        self.last_committed_timing.as_ref()
    }

    pub fn is_current_leader(&self) -> bool {
        self.is_leader_for_view(self.round_state.current_view())
    }

    /// Returns the validator index of the current leader.
    pub fn current_leader_index(&self) -> u32 {
        self.leader_index_for_view(self.round_state.current_view())
    }

    /// Checks if this node is the leader for a specific view.
    pub fn is_leader_for_view(&self, view: u64) -> bool {
        self.local_validator_index_for_view(view)
            .is_some_and(|index| index == self.leader_index_for_view(view))
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

    pub fn last_voted_view(&self) -> u64 {
        self.round_state.last_voted_view()
    }

    // ── Event processing ──

    /// Processes a consensus event and generates outputs.
    pub fn process_event(&mut self, event: ConsensusEvent) -> ConsensusResult<()> {
        let kind = match &event {
            ConsensusEvent::Message(_) => "message",
            ConsensusEvent::BlockReady(..) => "block_ready",
            ConsensusEvent::BlockImported(_) => "block_imported",
        };
        let _span = tracing::info_span!(
            target: "n42.cl.consensus.event",
            "process_event",
            view = self.round_state.current_view(),
            kind = kind,
        )
        .entered();
        match event {
            ConsensusEvent::Message(msg) => self.process_message(msg),
            ConsensusEvent::BlockReady(block_hash, tx_root_hash) => {
                self.on_block_ready(block_hash, tx_root_hash)
            }
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
            let is_exempt = matches!(
                msg,
                ConsensusMessage::Decide(_) | ConsensusMessage::NewView(_)
            ) || (matches!(msg, ConsensusMessage::Timeout(_))
                && view <= current_view + FUTURE_VIEW_WINDOW);

            if view > current_view && !is_exempt {
                if view <= current_view + FUTURE_VIEW_WINDOW {
                    if self.future_msg_buffer.len() >= MAX_FUTURE_MESSAGES {
                        // Evict the oldest (lowest view) entry.
                        if let Some(min_idx) = self
                            .future_msg_buffer
                            .iter()
                            .enumerate()
                            .min_by_key(|(_, (v, _))| *v)
                            .map(|(i, _)| i)
                        {
                            self.future_msg_buffer.swap_remove(min_idx);
                        }
                    }
                    tracing::debug!(target: "n42::cl::engine",
                        current_view,
                        msg_view = view,
                        buffered = self.future_msg_buffer.len(),
                        "buffering future-view message"
                    );
                    self.future_msg_buffer.push((view, msg));
                    return Ok(());
                }

                // Beyond FUTURE_VIEW_WINDOW: attempt QC-based view jump.
                if self.try_qc_view_jump(&msg, view)? {
                    let new_current = self.round_state.current_view();
                    if view == new_current {
                        return self.dispatch_message(msg);
                    } else if view > new_current
                        && view <= new_current + FUTURE_VIEW_WINDOW
                        && self.future_msg_buffer.len() < MAX_FUTURE_MESSAGES
                    {
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
        if let Some(view) = msg_view
            && view < current_view
            && !matches!(
                msg,
                ConsensusMessage::Decide(_) | ConsensusMessage::NewView(_)
            )
        {
            if let ConsensusMessage::Proposal(ref proposal) = msg
                && self.recover_late_committed_proposal(proposal)?
            {
                return Ok(());
            }
            tracing::trace!(target: "n42::cl::engine", current_view, msg_view = view, "discarding stale consensus message");
            return Ok(());
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
    fn try_qc_view_jump(
        &mut self,
        msg: &ConsensusMessage,
        msg_view: ViewNumber,
    ) -> ConsensusResult<bool> {
        let current_view = self.round_state.current_view();

        // Genesis QC (view 0) has no real signatures — skip it.
        let qc = match Self::extract_qc_from_message(msg) {
            Some(qc) if qc.view > 0 => qc,
            _ => return Ok(false),
        };

        // Proposal/Timeout/NewView can carry either a prepare QC or a commit QC as
        // their justify/high QC, so the view-jump path must accept both domains.
        // For Decide we know the exact changes_hash from the message; for others
        // we fall back to ZERO since the proposal is not yet cached on this
        // far-behind node (acceptable: the next regular proposal will retry).
        let verify_result = match msg {
            ConsensusMessage::Decide(decide) => super::quorum::verify_commit_qc(
                qc,
                self.validator_set_for_view(qc.view),
                &decide.validator_changes_hash,
            ),
            ConsensusMessage::PrepareQC(_) => {
                super::quorum::verify_qc(qc, self.validator_set_for_view(qc.view))
            }
            ConsensusMessage::Proposal(_)
            | ConsensusMessage::Timeout(_)
            | ConsensusMessage::NewView(_) => {
                let changes_hash = self.cached_changes_hash(&qc.block_hash);
                super::quorum::verify_qc_any_domain(
                    qc,
                    self.validator_set_for_view(qc.view),
                    &changes_hash,
                )
            }
            ConsensusMessage::Vote(_) | ConsensusMessage::CommitVote(_) => return Ok(false),
        };
        if verify_result.is_err() {
            tracing::debug!(target: "n42::cl::engine",
                current_view,
                msg_view,
                qc_view = qc.view,
                "QC view jump failed: invalid QC signature"
            );
            return Ok(false);
        }

        // Jump target: prefer msg_view (authenticated via BLS signature for Timeout/Proposal/
        // NewView messages) but cap the gap between msg_view and qc.view to limit the blast
        // radius of a single Byzantine validator sending a far-future view with an old QC.
        // A gap > MAX_VIEW_JUMP_GAP is implausible in normal operation and indicates either
        // a Byzantine validator or a severely partitioned node; in both cases we clamp.
        const MAX_VIEW_JUMP_GAP: u64 = 10_000;
        let target_view = msg_view.min(qc.view.saturating_add(MAX_VIEW_JUMP_GAP));
        if target_view <= current_view {
            return Ok(false);
        }

        tracing::info!(target: "n42::cl::engine",
            current_view,
            target_view,
            qc_view = qc.view,
            msg_view,
            "QC-based view jump: recovering node catching up to network"
        );

        self.round_state.update_locked_qc(qc);
        self.round_state.reset_consecutive_timeouts();

        self.emit(EngineOutput::SyncRequired {
            local_view: current_view,
            target_view,
        })?;
        self.advance_to_view(target_view)?;

        // Use actual view after advance: buffered-message replay may push view beyond target_view.
        let actual_view = self.round_state.current_view();
        self.emit(EngineOutput::ViewChanged {
            new_view: actual_view,
        })?;

        Ok(true)
    }

    // ── Internal helpers ──

    pub(super) fn advance_to_view(&mut self, new_view: ViewNumber) -> ConsensusResult<()> {
        // Monotonicity guard: never regress the view. This can happen when buffered-message
        // replay triggers a nested advance_to_view (e.g., a replayed Decide).
        if new_view <= self.round_state.current_view() {
            return Ok(());
        }

        // Save current PrepareQC for piggybacking into the next Proposal (chained mode).
        self.previous_prepare_qc = self.prepare_qc.take();

        // Pre-validate local key presence BEFORE advancing epoch (irreversible).
        if self.epoch_manager.epochs_enabled()
            && self.epoch_manager.is_epoch_boundary(new_view)
            && self.epoch_manager.has_staged_next()
        {
            let local_in_next = self
                .epoch_manager
                .peek_next_set()
                .and_then(|set| set.index_of_public_key(&self.local_public_key));

            let advanced = self.epoch_manager.advance_epoch();
            debug_assert!(
                advanced,
                "advance_epoch must succeed when has_staged_next is true"
            );

            let new_epoch = self.epoch_manager.current_epoch();
            let validator_count = self.validator_set().len();

            if let Some(new_index) = local_in_next {
                if new_index != self.my_index {
                    tracing::info!(
                        target: "n42::cl::engine",
                        old_index = self.my_index,
                        new_index,
                        new_epoch,
                        "updated local validator index for new epoch"
                    );
                    self.my_index = new_index;
                }
            } else {
                tracing::error!(
                    target: "n42::cl::engine",
                    new_epoch,
                    validator_count,
                    "local validator NOT in new epoch's validator set — \
                     consensus participation disabled; node continues syncing blocks"
                );
            }

            tracing::info!(
                target: "n42::cl::engine",
                new_epoch,
                validator_count,
                view = new_view,
                "epoch transition at view boundary"
            );
            self.emit(EngineOutput::EpochTransition {
                new_epoch,
                validator_count,
            })?;
        }

        self.round_state.advance_view(new_view);
        self.pacemaker
            .reset_for_view(new_view, self.round_state.consecutive_timeouts());
        self.vote_collector = None;
        self.commit_collector = None;
        self.timeout_collector = None;
        self.prepare_qc = None;
        self.pending_proposal = None;
        self.imported_blocks.clear();
        self.equivocation_tracker.clear();
        self.commit_equivocation_tracker.clear();

        // Defense-in-depth: re-derive my_index from the current set on every view
        // advance. The epoch-boundary branch above only fires for live consensus
        // transitions; nodes that joined via block sync, snapshot recovery, or any
        // path that mutates epoch_manager outside of advance_to_view would otherwise
        // keep their stale `my_index = 0` observer default and fail BLS verification
        // on every signed message they emit.
        let _ = self.sync_local_validator_index();
        // Preserve timing from committed view for external reading.
        if self.view_timing.commit_qc_formed.is_some() {
            self.last_committed_timing = Some(self.view_timing.clone());
        }
        self.view_timing = ViewTiming::new();

        // Replay buffered messages for the new view; re-buffer those still in window.
        let drained: Vec<(ViewNumber, ConsensusMessage)> =
            self.future_msg_buffer.drain(..).collect();
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
            tracing::debug!(target: "n42::cl::engine", view = new_view, replaying = to_replay.len(), "replaying buffered future-view messages");
        }

        for msg in to_replay {
            if let Err(e) = self.dispatch_message(msg) {
                tracing::debug!(target: "n42::cl::engine", view = new_view, error = %e, "buffered message replay failed");
            }
        }

        tracing::debug!(target: "n42::cl::engine", view = new_view, "advanced to new view");
        Ok(())
    }

    pub(super) fn validator_set(&self) -> &ValidatorSet {
        self.epoch_manager.current_validator_set()
    }

    pub(super) fn validator_set_for_view(&self, view: ViewNumber) -> &ValidatorSet {
        self.epoch_manager.validator_set_for_view(view)
    }

    /// Resolves the correct validator set for QC verification.
    ///
    /// `validator_set_for_view` uses a mathematical epoch formula that diverges
    /// from the actual epoch state in the "epoch-drift zone": after a validator-set
    /// transition the formula may map old views to the wrong epoch number, causing
    /// it to fall back to the current (larger) set.  For example, after advancing
    /// from epoch 0 (3 validators) to epoch 1 (4 validators) at view 151, the first
    /// proposal carries a justify_qc from view 150.  `epoch_for_view(150)` = 4,
    /// which is neither the current epoch (1) nor in history (only epoch 0 stored),
    /// so `validator_set_for_view` falls back to the current 4-validator set.
    /// Verifying a 3-bit bitmap against a 4-validator set then fails.
    ///
    /// This method detects the mismatch via bitmap-size comparison and falls back
    /// to `find_validator_set_by_len`, which searches current / next / historical
    /// sets by validator count.  BLS verification in the caller confirms correctness.
    pub(super) fn resolve_qc_validator_set(
        &self,
        qc: &n42_primitives::consensus::QuorumCertificate,
    ) -> &ValidatorSet {
        let primary = self.validator_set_for_view(qc.view);
        if primary.len() as usize == qc.signers.len() {
            return primary;
        }
        // Bitmap size doesn't match the primary set — epoch drift has occurred.
        // Search all known sets by size; BLS in the caller will confirm the match.
        if let Some(vs) = self
            .epoch_manager
            .find_validator_set_by_len(qc.signers.len())
        {
            tracing::debug!(
                target: "n42::cl::engine",
                qc_view = qc.view,
                primary_len = primary.len(),
                bitmap_len = qc.signers.len(),
                "epoch-drift fallback: using historical validator set for QC verification"
            );
            return vs;
        }
        // No set matched the bitmap size; return primary and let BLS fail clearly.
        primary
    }

    pub(super) fn leader_index_for_view(&self, view: ViewNumber) -> u32 {
        LeaderSelector::leader_for_view(view, self.validator_set_for_view(view))
    }

    pub(super) fn local_validator_index_for_view(&self, view: ViewNumber) -> Option<u32> {
        self.validator_set_for_view(view)
            .index_of_public_key(&self.local_public_key)
    }

    /// Returns the cached `validator_changes_hash` for `block_hash`, or
    /// `B256::ZERO` (the canonical "no changes" sentinel from
    /// `EpochManager::hash_changes`) when the proposal isn't in the cache.
    /// Used by every R2 commit-vote signing/verification path.
    pub(super) fn cached_changes_hash(&self, block_hash: &B256) -> B256 {
        self.pending_changes_hashes
            .get(block_hash)
            .copied()
            .unwrap_or_default()
    }

    pub(super) fn emit(&self, output: EngineOutput) -> ConsensusResult<()> {
        const MAX_OUTPUT_SEND_RETRIES: u32 = 3;

        // Treat bounded-channel backpressure as recoverable. Only a closed channel
        // is immediately fatal to consensus.
        let is_block_committed = matches!(output, EngineOutput::BlockCommitted { .. });
        let output_kind = match &output {
            EngineOutput::BroadcastMessage(_) => "BroadcastMessage",
            EngineOutput::SendToValidator(_, _) => "SendToValidator",
            EngineOutput::ExecuteBlock(_) => "ExecuteBlock",
            EngineOutput::BlockCommitted { .. } => "BlockCommitted",
            EngineOutput::CommittedBlockValidatorChangesRecovered { .. } => {
                "CommittedBlockValidatorChangesRecovered"
            }
            EngineOutput::ViewChanged { .. } => "ViewChanged",
            EngineOutput::SyncRequired { .. } => "SyncRequired",
            EngineOutput::EquivocationDetected { .. } => "EquivocationDetected",
            EngineOutput::EpochTransition { .. } => "EpochTransition",
        };

        match self.output_tx.try_send(output) {
            Ok(()) => Ok(()),
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                tracing::error!(
                    target: "n42::cl::engine",
                    output = output_kind,
                    "consensus output channel closed"
                );
                Err(crate::error::ConsensusError::OutputChannelClosed)
            }
            Err(tokio::sync::mpsc::error::TrySendError::Full(output)) => {
                let mut pending = output;

                for attempt in 1..=MAX_OUTPUT_SEND_RETRIES {
                    // Brief sleep to give the channel consumer a chance to drain.
                    // 500µs × 3 retries = 1.5ms max blocking per emit() call.
                    // This is acceptable: channel backpressure is rare (capacity 64-1024),
                    // and 1.5ms is negligible relative to the 8-second slot target.
                    // std::thread::sleep is used deliberately here rather than tokio::time::sleep
                    // because emit() is a sync fn; the short duration minimises worker stall.
                    std::thread::sleep(std::time::Duration::from_micros(500));
                    match self.output_tx.try_send(pending) {
                        Ok(()) => {
                            if is_block_committed {
                                tracing::warn!(
                                    target: "n42::cl::engine",
                                    attempt,
                                    "BlockCommitted delivered after retry"
                                );
                            } else {
                                tracing::warn!(
                                    target: "n42::cl::engine",
                                    output = output_kind,
                                    attempt,
                                    "consensus output delivered after retry"
                                );
                            }
                            return Ok(());
                        }
                        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                            tracing::error!(
                                target: "n42::cl::engine",
                                output = output_kind,
                                attempt,
                                "consensus output channel closed during retry"
                            );
                            return Err(crate::error::ConsensusError::OutputChannelClosed);
                        }
                        Err(tokio::sync::mpsc::error::TrySendError::Full(output)) => {
                            pending = output;
                        }
                    }
                }

                if is_block_committed {
                    tracing::error!(
                        target: "n42::cl::engine",
                        "CRITICAL: BlockCommitted lost after {} retries",
                        MAX_OUTPUT_SEND_RETRIES
                    );
                } else {
                    tracing::error!(
                        target: "n42::cl::engine",
                        output = output_kind,
                        retries = MAX_OUTPUT_SEND_RETRIES,
                        "consensus output channel remained full after retries"
                    );
                }

                Err(crate::error::ConsensusError::OutputChannelClosed)
            }
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
    use n42_primitives::consensus::{CommitVote, Decide, Proposal, ValidatorChange, Vote};

    fn test_key(seed: u8) -> n42_primitives::BlsSecretKey {
        n42_primitives::BlsSecretKey::key_gen(&[seed; 32])
            .expect("deterministic test key should be valid")
    }

    fn make_engine_with_output_capacity(
        n: usize,
        my_index: u32,
        output_capacity: usize,
    ) -> (
        ConsensusEngine,
        Vec<n42_primitives::BlsSecretKey>,
        ValidatorSet,
        mpsc::Receiver<EngineOutput>,
    ) {
        let sks: Vec<_> = (0..n).map(|i| test_key(0x10 + i as u8)).collect();
        let infos: Vec<_> = sks
            .iter()
            .enumerate()
            .map(|(i, sk)| ValidatorInfo {
                address: Address::with_last_byte(i as u8),
                bls_public_key: sk.public_key(),
                p2p_peer_id: None,
            })
            .collect();
        let f = ((n as u32).saturating_sub(1)) / 3;
        let vs = ValidatorSet::new(&infos, f);

        let (output_tx, output_rx) = mpsc::channel(output_capacity);
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

    fn make_engine(
        n: usize,
        my_index: u32,
    ) -> (
        ConsensusEngine,
        Vec<n42_primitives::BlsSecretKey>,
        ValidatorSet,
        mpsc::Receiver<EngineOutput>,
    ) {
        make_engine_with_output_capacity(n, my_index, 1024)
    }

    fn build_test_commit_qc(
        view: ViewNumber,
        block_hash: B256,
        sks: &[n42_primitives::BlsSecretKey],
        vs: &ValidatorSet,
        signers: &[u32],
    ) -> QuorumCertificate {
        build_test_commit_qc_with_changes(view, block_hash, sks, vs, signers, &B256::ZERO)
    }

    fn build_test_commit_qc_with_changes(
        view: ViewNumber,
        block_hash: B256,
        sks: &[n42_primitives::BlsSecretKey],
        vs: &ValidatorSet,
        signers: &[u32],
        changes_hash: &B256,
    ) -> QuorumCertificate {
        use crate::protocol::quorum::{VoteCollector, commit_signing_message};
        let mut collector = VoteCollector::new(view, block_hash, vs.len());
        for &i in signers {
            let msg = commit_signing_message(view, &block_hash, changes_hash);
            collector.add_vote(i, sks[i as usize].sign(&msg)).unwrap();
        }
        collector
            .build_qc_with_message(vs, &commit_signing_message(view, &block_hash, changes_hash))
            .unwrap()
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
    fn test_epoch_transition_updates_local_validator_index() {
        let sks: Vec<_> = (0..4).map(|i| test_key(0x30 + i as u8)).collect();
        let infos_epoch_0: Vec<_> = sks
            .iter()
            .enumerate()
            .map(|(i, sk)| ValidatorInfo {
                address: Address::with_last_byte(i as u8),
                bls_public_key: sk.public_key(),
                p2p_peer_id: None,
            })
            .collect();
        let infos_epoch_1 = vec![
            infos_epoch_0[1].clone(),
            infos_epoch_0[2].clone(),
            infos_epoch_0[0].clone(),
            infos_epoch_0[3].clone(),
        ];

        let mut epoch_manager =
            EpochManager::with_epoch_length(ValidatorSet::new(&infos_epoch_0, 1), 2);
        epoch_manager
            .stage_next_epoch(&infos_epoch_1, 1)
            .expect("next epoch should stage");

        let (output_tx, mut output_rx) = mpsc::channel(16);
        let mut engine = ConsensusEngine::with_epoch_manager(
            0,
            sks[0].clone(),
            epoch_manager,
            60000,
            120000,
            output_tx,
        );

        engine
            .advance_to_view(3)
            .expect("epoch transition should succeed");

        assert_eq!(engine.epoch_manager.current_epoch(), 1);
        assert_eq!(engine.my_index, 2);
        assert!(matches!(
            output_rx.try_recv(),
            Ok(EngineOutput::EpochTransition {
                new_epoch: 1,
                validator_count: 4,
            })
        ));
    }

    /// Tests that `sync_local_validator_index` correctly updates `my_index`
    /// after an external epoch advance (simulating the sync path).
    #[test]
    fn test_sync_local_validator_index_after_epoch_advance() {
        // Create 3 validators for epoch 0.
        let sks: Vec<_> = (0..4).map(|i| test_key(0x50 + i as u8)).collect();
        let infos_epoch_0: Vec<ValidatorInfo> = sks[..3]
            .iter()
            .enumerate()
            .map(|(i, sk)| ValidatorInfo {
                address: Address::with_last_byte(i as u8),
                bls_public_key: sk.public_key(),
                p2p_peer_id: None,
            })
            .collect();

        // New validator #3 will be added in epoch 1.
        let new_validator_info = ValidatorInfo {
            address: Address::with_last_byte(3),
            bls_public_key: sks[3].public_key(),
            p2p_peer_id: None,
        };

        // Build engine for the NEW validator with my_index=0 (observer default,
        // simulating a node that started before being added to the set).
        let epoch_manager =
            EpochManager::with_epoch_length(ValidatorSet::new(&infos_epoch_0, 0), 5);
        let (output_tx, _rx) = mpsc::channel(16);
        let mut engine = ConsensusEngine::with_epoch_manager(
            0,              // wrong index — simulates observer default
            sks[3].clone(), // but uses validator #3's secret key
            epoch_manager,
            60000,
            120000,
            output_tx,
        );

        // Before epoch advance: sync_local_validator_index returns None
        // because validator #3 is not in the 3-validator set.
        assert_eq!(engine.sync_local_validator_index(), None);
        assert_eq!(engine.my_index(), 0); // still observer

        // Simulate sync path: stage next epoch with the new validator added.
        let mut infos_epoch_1 = infos_epoch_0.clone();
        infos_epoch_1.push(new_validator_info);
        engine
            .epoch_manager_mut()
            .stage_next_epoch(&infos_epoch_1, 1)
            .unwrap();
        engine.epoch_manager_mut().advance_epoch();

        // Now sync_local_validator_index should find our key and update my_index.
        let new_idx = engine.sync_local_validator_index();
        assert!(new_idx.is_some(), "validator #3 should be in the new set");
        assert_eq!(
            engine.my_index(),
            new_idx.unwrap(),
            "my_index should be updated to the correct position in the new set"
        );
        // Verify it's not still 0 (unless it actually IS index 0 in the new set).
        let actual_idx = engine
            .epoch_manager()
            .current_validator_set()
            .index_of_public_key(&sks[3].public_key())
            .unwrap();
        assert_eq!(engine.my_index(), actual_idx);
    }

    /// Tests that a newly-added validator's votes are accepted by the leader
    /// after epoch transition (validates the voting.rs fix).
    #[test]
    fn test_new_validator_votes_accepted_after_epoch() {
        // Start with 3 validators, epoch_length=5.
        let sks: Vec<_> = (0..4).map(|i| test_key(0x60 + i as u8)).collect();
        let infos_epoch_0: Vec<ValidatorInfo> = sks[..3]
            .iter()
            .enumerate()
            .map(|(i, sk)| ValidatorInfo {
                address: Address::with_last_byte(i as u8),
                bls_public_key: sk.public_key(),
                p2p_peer_id: None,
            })
            .collect();

        let new_validator_info = ValidatorInfo {
            address: Address::with_last_byte(3),
            bls_public_key: sks[3].public_key(),
            p2p_peer_id: None,
        };

        let mut infos_epoch_1 = infos_epoch_0.clone();
        infos_epoch_1.push(new_validator_info);

        // Create 4 engines: 3 original + 1 new validator.
        let epoch_length = 5u64;
        let mut engines = Vec::new();
        let mut rxs = Vec::new();

        for i in 0..4usize {
            let my_index = if i < 3 { i as u32 } else { 0 }; // new validator starts as observer
            let epoch_manager =
                EpochManager::with_epoch_length(ValidatorSet::new(&infos_epoch_0, 0), epoch_length);
            let (tx, rx) = mpsc::channel(1024);
            let engine = ConsensusEngine::with_epoch_manager(
                my_index,
                sks[i].clone(),
                epoch_manager,
                60000,
                120000,
                tx,
            );
            engines.push(engine);
            rxs.push(rx);
        }

        // Stage next epoch on all engines: add validator #3.
        for engine in &mut engines {
            engine
                .epoch_manager_mut()
                .stage_next_epoch(&infos_epoch_1, 1)
                .unwrap();
        }

        // Advance all engines to view 6 (epoch boundary for epoch_length=5).
        // This triggers advance_epoch on the live consensus path.
        for engine in &mut engines {
            engine.advance_to_view(epoch_length + 1).unwrap();
        }
        // Drain EpochTransition outputs.
        for rx in &mut rxs {
            while rx.try_recv().is_ok() {}
        }

        // After epoch transition, the new validator (engine[3]) should have
        // its my_index updated to its correct position.
        let new_idx = engines[3]
            .epoch_manager()
            .current_validator_set()
            .index_of_public_key(&sks[3].public_key())
            .unwrap();
        assert_eq!(
            engines[3].my_index(),
            new_idx,
            "new validator my_index should be updated after epoch transition"
        );

        // Now test that the new validator can vote and the leader accepts it.
        let view = epoch_length + 1; // view 6
        let vs_epoch_1 = ValidatorSet::new(&infos_epoch_1, 1);
        let leader = LeaderSelector::leader_for_view(view, &vs_epoch_1);

        // Leader proposes a block.
        let block_hash = B256::repeat_byte(0xAB);
        engines[leader as usize]
            .process_event(ConsensusEvent::BlockReady(block_hash, None))
            .expect("leader BlockReady should succeed");

        // Get the proposal.
        let mut proposal = None;
        while let Ok(o) = rxs[leader as usize].try_recv() {
            if let EngineOutput::BroadcastMessage(msg @ ConsensusMessage::Proposal(_)) = o {
                proposal = Some(msg);
            }
        }
        let proposal = proposal.expect("leader should broadcast Proposal");

        // All non-leader validators process the proposal and vote.
        for i in 0..4 {
            if i as u32 != leader {
                engines[i]
                    .process_event(ConsensusEvent::Message(proposal.clone()))
                    .expect("should accept Proposal");
            }
        }

        // Collect votes from all non-leaders (including the new validator #3)
        // and send them to the leader.
        for i in 0..4 {
            if i as u32 != leader {
                while let Ok(o) = rxs[i].try_recv() {
                    if let EngineOutput::SendToValidator(target, msg) = o {
                        if target == leader {
                            engines[leader as usize]
                                .process_event(ConsensusEvent::Message(msg))
                                .expect("leader should accept vote from any validator including new one");
                        }
                    }
                }
            }
        }

        // Leader should have formed a PrepareQC (quorum reached with 4 validators).
        let mut has_prepare_qc = false;
        while let Ok(o) = rxs[leader as usize].try_recv() {
            if matches!(
                o,
                EngineOutput::BroadcastMessage(ConsensusMessage::PrepareQC(_))
            ) {
                has_prepare_qc = true;
            }
        }
        assert!(
            has_prepare_qc,
            "leader should form PrepareQC with votes from all 4 validators (including newly added)"
        );
    }

    /// Builds an observer engine: 3 validators in the active set, plus a 4th secret
    /// key used as the local key (not in the set). `my_index` defaults to 0, but the
    /// local key is at no valid index — exactly the post-join hazard the guard targets.
    fn make_observer_engine(seed_base: u8) -> (ConsensusEngine, mpsc::Receiver<EngineOutput>) {
        let sks: Vec<_> = (0..4).map(|i| test_key(seed_base + i as u8)).collect();
        let infos: Vec<ValidatorInfo> = sks[..3]
            .iter()
            .enumerate()
            .map(|(i, sk)| ValidatorInfo {
                address: Address::with_last_byte(i as u8),
                bls_public_key: sk.public_key(),
                p2p_peer_id: None,
            })
            .collect();

        let (output_tx, output_rx) = mpsc::channel(64);
        let engine = ConsensusEngine::new(
            0,
            sks[3].clone(),
            ValidatorSet::new(&infos, 0),
            60_000,
            120_000,
            output_tx,
        );
        (engine, output_rx)
    }

    /// Regression: observer-mode node (local key not in the set) must not broadcast
    /// a timeout. Otherwise it signs with its own key under sender=0, every receiver
    /// fails BLS verification, and on_timeout's self-process call returns
    /// InvalidSignature, stalling the node.
    #[test]
    fn test_observer_does_not_broadcast_timeout() {
        let (mut engine, mut output_rx) = make_observer_engine(0x70);
        assert!(!engine.is_local_validator_active_for_view(engine.current_view()));

        engine
            .on_timeout()
            .expect("on_timeout must succeed for observer");

        let broadcast_count = std::iter::from_fn(|| output_rx.try_recv().ok())
            .filter(|o| {
                matches!(
                    o,
                    EngineOutput::BroadcastMessage(ConsensusMessage::Timeout(_))
                )
            })
            .count();
        assert_eq!(
            broadcast_count, 0,
            "observer must not broadcast Timeout messages"
        );
    }

    /// Regression: same observer guard for vote broadcast.
    #[test]
    fn test_observer_does_not_send_vote() {
        let (mut engine, mut output_rx) = make_observer_engine(0x80);

        engine
            .send_vote(engine.current_view(), B256::repeat_byte(0xCC))
            .expect("send_vote must succeed for observer (silent skip)");

        let vote_count = std::iter::from_fn(|| output_rx.try_recv().ok())
            .filter(|o| {
                matches!(
                    o,
                    EngineOutput::SendToValidator(_, ConsensusMessage::Vote(_))
                )
            })
            .count();
        assert_eq!(vote_count, 0, "observer must not send Vote messages");
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
    fn test_is_leader_for_view_uses_staged_next_epoch_index() {
        let sks: Vec<_> = (0..4).map(|i| test_key(0x40 + i as u8)).collect();
        let infos_epoch_0: Vec<_> = sks
            .iter()
            .enumerate()
            .map(|(i, sk)| ValidatorInfo {
                address: Address::with_last_byte(i as u8),
                bls_public_key: sk.public_key(),
                p2p_peer_id: None,
            })
            .collect();
        let infos_epoch_1 = vec![
            infos_epoch_0[1].clone(),
            infos_epoch_0[2].clone(),
            infos_epoch_0[0].clone(),
            infos_epoch_0[3].clone(),
        ];

        let mut epoch_manager =
            EpochManager::with_epoch_length(ValidatorSet::new(&infos_epoch_0, 1), 1);
        epoch_manager
            .stage_next_epoch(&infos_epoch_1, 1)
            .expect("next epoch should stage");

        let (output_tx, _output_rx) = mpsc::channel(16);
        let engine = ConsensusEngine::with_epoch_manager(
            0,
            sks[0].clone(),
            epoch_manager,
            60000,
            120000,
            output_tx,
        );

        assert!(
            !engine.is_current_leader(),
            "validator 0 should not be leader in current epoch view 1"
        );
        assert!(
            engine.is_leader_for_view(2),
            "staged next epoch should make validator 0 leader for view 2 after re-indexing"
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
    fn test_emit_retries_non_block_committed_output_when_channel_is_full() {
        let (engine, _, _, mut rx) = make_engine_with_output_capacity(1, 0, 1);

        engine
            .emit(EngineOutput::ExecuteBlock(B256::repeat_byte(0xAB)))
            .expect("first output should fill the channel");

        let drain = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(1));

            let first = rx.blocking_recv().expect("expected first output");
            let second = rx.blocking_recv().expect("expected retried output");
            (first, second)
        });

        engine
            .emit(EngineOutput::ViewChanged { new_view: 2 })
            .expect("backpressure on non-critical outputs should be retried");

        let (first, second) = drain.join().expect("drain thread should complete");
        assert!(
            matches!(first, EngineOutput::ExecuteBlock(hash) if hash == B256::repeat_byte(0xAB))
        );
        assert!(matches!(second, EngineOutput::ViewChanged { new_view: 2 }));
    }

    #[test]
    fn test_try_form_tc_and_advance_uses_next_epoch_local_index_for_new_view() {
        use crate::protocol::quorum::TimeoutCollector;
        use crate::protocol::quorum::timeout_signing_message;

        let sks: Vec<_> = (0..4).map(|i| test_key(0x50 + i as u8)).collect();
        let infos_epoch_0: Vec<_> = sks
            .iter()
            .enumerate()
            .map(|(i, sk)| ValidatorInfo {
                address: Address::with_last_byte(i as u8),
                bls_public_key: sk.public_key(),
                p2p_peer_id: None,
            })
            .collect();
        let infos_epoch_1 = vec![
            infos_epoch_0[1].clone(),
            infos_epoch_0[2].clone(),
            infos_epoch_0[0].clone(),
            infos_epoch_0[3].clone(),
        ];

        let mut epoch_manager =
            EpochManager::with_epoch_length(ValidatorSet::new(&infos_epoch_0, 1), 1);
        epoch_manager
            .stage_next_epoch(&infos_epoch_1, 1)
            .expect("next epoch should stage");

        let (output_tx, mut output_rx) = mpsc::channel(16);
        let mut engine = ConsensusEngine::with_epoch_manager(
            0,
            sks[0].clone(),
            epoch_manager,
            60000,
            120000,
            output_tx,
        );

        let mut collector = TimeoutCollector::new(1, 4);
        let msg = timeout_signing_message(1);
        for signer in [0u32, 1, 2] {
            collector
                .add_verified_timeout(
                    signer,
                    sks[signer as usize].sign(&msg),
                    QuorumCertificate::genesis(),
                )
                .expect("timeout should add");
        }
        engine.timeout_collector = Some(collector);

        engine
            .try_form_tc_and_advance(1, 2)
            .expect("TC should form and advance");

        let outputs: Vec<_> = std::iter::from_fn(|| output_rx.try_recv().ok()).collect();
        let emitted_new_view = outputs.iter().find_map(|output| match output {
            EngineOutput::BroadcastMessage(ConsensusMessage::NewView(nv)) => Some(nv.clone()),
            _ => None,
        });

        assert_eq!(engine.epoch_manager.current_epoch(), 1);
        assert_eq!(engine.my_index, 2);
        assert!(matches!(
            emitted_new_view,
            Some(nv) if nv.view == 2 && nv.leader == 2
        ));
    }

    #[test]
    fn test_engine_block_ready_non_leader() {
        let (mut engine, _, _, _rx) = make_engine(4, 0);
        engine
            .process_event(ConsensusEvent::BlockReady(B256::repeat_byte(0xAA), None))
            .expect("non-leader block ready should succeed");
        assert_eq!(engine.current_phase(), Phase::WaitingForProposal);
    }

    #[test]
    fn test_engine_block_ready_as_leader() {
        let (mut engine, _, _, mut rx) = make_engine(1, 0);
        let block_hash = B256::repeat_byte(0xBB);

        engine
            .process_event(ConsensusEvent::BlockReady(block_hash, None))
            .expect("leader block ready should succeed");

        // Single-validator: consensus completes in one call, advancing to view 2.
        assert_eq!(engine.current_view(), 2);
        assert_eq!(engine.current_phase(), Phase::WaitingForProposal);

        let mut outputs = vec![];
        while let Ok(output) = rx.try_recv() {
            outputs.push(output);
        }

        assert!(outputs.iter().any(|o| matches!(
            o,
            EngineOutput::BroadcastMessage(ConsensusMessage::Proposal(_))
        )));
        assert!(outputs.iter().any(|o| matches!(
            o,
            EngineOutput::BroadcastMessage(ConsensusMessage::PrepareQC(_))
        )));
        assert!(outputs.iter().any(|o| matches!(
            o,
            EngineOutput::BroadcastMessage(ConsensusMessage::Decide(_))
        )));
        assert!(outputs.iter().any(
            |o| matches!(o, EngineOutput::BlockCommitted { block_hash: h, .. } if *h == block_hash)
        ));
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
            tx_root_hash: None,
            validator_changes: None,
        };

        // Stale proposals are silently discarded by the pre-filter.
        let result = engine.process_event(ConsensusEvent::Message(ConsensusMessage::Proposal(
            proposal,
        )));
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
            tx_root_hash: None,
            validator_changes: None,
        };

        let result = engine.process_event(ConsensusEvent::Message(ConsensusMessage::Proposal(
            proposal,
        )));
        assert!(result.is_err());
        match result.unwrap_err() {
            crate::error::ConsensusError::InvalidProposer {
                expected, actual, ..
            } => {
                assert_eq!(expected, 1);
                assert_eq!(actual, 0);
            }
            other => panic!("expected InvalidProposer, got: {:?}", other),
        }
    }

    #[test]
    fn test_engine_valid_proposal_votes_immediately() {
        let (mut engine, sks, _, mut rx) = make_engine(4, 0);

        let block_hash = B256::repeat_byte(0xEE);
        let msg = crate::protocol::quorum::proposal_signing_message(1, &block_hash, &None);
        let proposal = n42_primitives::consensus::Proposal {
            view: 1,
            block_hash,
            justify_qc: QuorumCertificate::genesis(),
            proposer: 1,
            signature: sks[1].sign(&msg),
            prepare_qc: None,
            tx_root_hash: None,
            validator_changes: None,
        };

        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Proposal(
                proposal,
            )))
            .expect("valid proposal should be accepted");

        assert_eq!(engine.current_phase(), Phase::Voting);

        let mut outputs = vec![];
        while let Ok(output) = rx.try_recv() {
            outputs.push(output);
        }

        // Optimistic Voting: vote is sent immediately with the Proposal, no BlockData wait.
        assert!(
            outputs
                .iter()
                .any(|o| matches!(o, EngineOutput::ExecuteBlock(h) if *h == block_hash))
        );
        assert!(outputs.iter().any(|o| matches!(
            o,
            EngineOutput::SendToValidator(_, ConsensusMessage::Vote(_))
        )));
    }

    #[test]
    fn test_engine_block_imported_before_proposal() {
        let (mut engine, sks, _, mut rx) = make_engine(4, 0);
        let block_hash = B256::repeat_byte(0xEE);

        engine
            .process_event(ConsensusEvent::BlockImported(block_hash))
            .expect("BlockImported before proposal should succeed");

        assert!(engine.imported_blocks.contains(&block_hash));

        let msg = crate::protocol::quorum::proposal_signing_message(1, &block_hash, &None);
        let proposal = n42_primitives::consensus::Proposal {
            view: 1,
            block_hash,
            justify_qc: QuorumCertificate::genesis(),
            proposer: 1,
            signature: sks[1].sign(&msg),
            prepare_qc: None,
            tx_root_hash: None,
            validator_changes: None,
        };

        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Proposal(
                proposal,
            )))
            .expect("valid proposal should be accepted");

        let mut outputs = vec![];
        while let Ok(output) = rx.try_recv() {
            outputs.push(output);
        }

        // Optimistic Voting: vote sent immediately regardless of BlockData status.
        assert!(outputs.iter().any(|o| matches!(
            o,
            EngineOutput::SendToValidator(_, ConsensusMessage::Vote(_))
        )));
    }

    #[test]
    fn test_engine_timeout_clears_state() {
        let (mut engine, sks, _, mut rx) = make_engine(4, 0);
        let block_hash = B256::repeat_byte(0xEE);

        let msg = crate::protocol::quorum::proposal_signing_message(1, &block_hash, &None);
        let proposal = n42_primitives::consensus::Proposal {
            view: 1,
            block_hash,
            justify_qc: QuorumCertificate::genesis(),
            proposer: 1,
            signature: sks[1].sign(&msg),
            prepare_qc: None,
            tx_root_hash: None,
            validator_changes: None,
        };
        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Proposal(
                proposal,
            )))
            .expect("proposal should succeed");
        while rx.try_recv().is_ok() {}

        // With optimistic voting, vote was already sent. Timeout still clears state.
        engine.on_timeout().expect("timeout should succeed");
        assert!(engine.pending_proposal.is_none());
        assert!(engine.imported_blocks.is_empty());
    }

    #[test]
    fn test_engine_timeout() {
        let (mut engine, _, _, mut rx) = make_engine(4, 0);

        engine.on_timeout().expect("timeout should succeed");

        assert_eq!(engine.current_phase(), Phase::TimedOut);
        let output = rx.try_recv().expect("should have timeout output");
        assert!(matches!(
            output,
            EngineOutput::BroadcastMessage(ConsensusMessage::Timeout(_))
        ));
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
            .process_event(ConsensusEvent::BlockReady(block_hash, None))
            .expect("block ready should succeed");

        assert_eq!(engine.current_phase(), Phase::Voting);
        while rx.try_recv().is_ok() {}

        // Two more validators vote (leader already has 1 self-vote).
        for i in [0u32, 2] {
            let msg = signing_message(view, &block_hash);
            let vote = Vote {
                view,
                block_hash,
                voter: i,
                signature: sks[i as usize].sign(&msg),
            };
            engine
                .process_event(ConsensusEvent::Message(ConsensusMessage::Vote(vote)))
                .expect("vote should succeed");
        }

        let mut outputs = vec![];
        while let Ok(o) = rx.try_recv() {
            outputs.push(o);
        }
        assert!(outputs.iter().any(|o| matches!(
            o,
            EngineOutput::BroadcastMessage(ConsensusMessage::PrepareQC(_))
        )));

        // Two commit votes (leader already self-voted for Round 2).
        for i in [0u32, 2] {
            let msg = commit_signing_message(view, &block_hash, &alloy_primitives::B256::ZERO);
            let cv = CommitVote {
                view,
                block_hash,
                voter: i,
                signature: sks[i as usize].sign(&msg),
            };
            engine
                .process_event(ConsensusEvent::Message(ConsensusMessage::CommitVote(cv)))
                .expect("commit vote should succeed");
        }

        assert_eq!(engine.current_view(), 2);
        assert_eq!(engine.current_phase(), Phase::WaitingForProposal);

        outputs.clear();
        while let Ok(o) = rx.try_recv() {
            outputs.push(o);
        }
        assert!(outputs.iter().any(|o| matches!(
            o,
            EngineOutput::BroadcastMessage(ConsensusMessage::Decide(_))
        )));
        assert!(outputs.iter().any(|o| matches!(o, EngineOutput::BlockCommitted { block_hash: h, view: v, .. } if *h == block_hash && *v == view)));
    }

    #[test]
    fn test_duplicate_commit_vote_ignored() {
        use crate::protocol::quorum::{commit_signing_message, signing_message};

        let (mut engine, sks, _vs, mut rx) = make_engine(4, 1);
        let block_hash = B256::repeat_byte(0xF4);
        let view = 1u64;

        engine
            .process_event(ConsensusEvent::BlockReady(block_hash, None))
            .expect("block ready should succeed");
        while rx.try_recv().is_ok() {}

        for i in [0u32, 2] {
            let msg = signing_message(view, &block_hash);
            let vote = Vote {
                view,
                block_hash,
                voter: i,
                signature: sks[i as usize].sign(&msg),
            };
            engine
                .process_event(ConsensusEvent::Message(ConsensusMessage::Vote(vote)))
                .expect("vote should succeed");
        }
        while rx.try_recv().is_ok() {}

        let msg = commit_signing_message(view, &block_hash, &alloy_primitives::B256::ZERO);
        let dup_sig = sks[0].sign(&msg);
        let dup_commit_vote = CommitVote {
            view,
            block_hash,
            voter: 0,
            signature: dup_sig.clone(),
        };
        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::CommitVote(
                dup_commit_vote.clone(),
            )))
            .expect("first commit vote should succeed");
        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::CommitVote(
                dup_commit_vote,
            )))
            .expect("duplicate commit vote delivery should be ignored");

        let cv = CommitVote {
            view,
            block_hash,
            voter: 2,
            signature: sks[2].sign(&msg),
        };
        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::CommitVote(cv)))
            .expect("distinct commit vote should succeed");

        assert_eq!(engine.current_view(), 2);
        assert_eq!(engine.current_phase(), Phase::WaitingForProposal);
    }

    #[test]
    fn test_validator_receives_prepare_qc() {
        use crate::protocol::quorum::{VoteCollector, signing_message};

        let (mut engine, sks, vs, mut rx) = make_engine(4, 0);
        let block_hash = B256::repeat_byte(0xF2);
        let view = 1u64;

        let prop_msg = crate::protocol::quorum::proposal_signing_message(view, &block_hash, &None);
        let proposal = n42_primitives::consensus::Proposal {
            view,
            block_hash,
            justify_qc: QuorumCertificate::genesis(),
            proposer: 1,
            signature: sks[1].sign(&prop_msg),
            prepare_qc: None,
            tx_root_hash: None,
            validator_changes: None,
        };
        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Proposal(
                proposal,
            )))
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

        let prepare_qc = n42_primitives::consensus::PrepareQC {
            view,
            block_hash,
            qc,
        };
        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::PrepareQC(
                prepare_qc,
            )))
            .expect("PrepareQC should succeed");

        assert_eq!(engine.current_phase(), Phase::PreCommit);

        let mut outputs = vec![];
        while let Ok(o) = rx.try_recv() {
            outputs.push(o);
        }
        let has_commit_vote = outputs.iter().any(|o| {
            matches!(
                o,
                EngineOutput::SendToValidator(1, ConsensusMessage::CommitVote(cv))
                if cv.view == view && cv.block_hash == block_hash
            )
        });
        assert!(
            has_commit_vote,
            "validator should send CommitVote to leader"
        );
    }

    #[test]
    fn test_prepare_qc_rejects_block_hash_mismatch_between_wrapper_and_qc() {
        use crate::protocol::quorum::{VoteCollector, signing_message};

        let (mut engine, sks, vs, mut rx) = make_engine(4, 0);
        let block_hash = B256::repeat_byte(0xF2);
        let wrong_hash = B256::repeat_byte(0xF3);
        let view = 1u64;

        let prop_msg = crate::protocol::quorum::proposal_signing_message(view, &block_hash, &None);
        let proposal = n42_primitives::consensus::Proposal {
            view,
            block_hash,
            justify_qc: QuorumCertificate::genesis(),
            proposer: 1,
            signature: sks[1].sign(&prop_msg),
            prepare_qc: None,
            tx_root_hash: None,
            validator_changes: None,
        };
        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Proposal(
                proposal,
            )))
            .expect("proposal should succeed");
        while rx.try_recv().is_ok() {}

        let mut collector = VoteCollector::new(view, block_hash, vs.len());
        for i in 0..3u32 {
            let msg = signing_message(view, &block_hash);
            collector.add_vote(i, sks[i as usize].sign(&msg)).unwrap();
        }
        let qc = collector.build_qc(&vs).unwrap();

        let prepare_qc = n42_primitives::consensus::PrepareQC {
            view,
            block_hash: wrong_hash,
            qc,
        };
        let result = engine.process_event(ConsensusEvent::Message(ConsensusMessage::PrepareQC(
            prepare_qc,
        )));

        assert!(matches!(
            result,
            Err(crate::error::ConsensusError::BlockHashMismatch {
                expected,
                got,
            }) if expected == wrong_hash && got == block_hash
        ));
        assert_eq!(engine.current_phase(), Phase::Voting);
        assert!(
            !matches!(
                rx.try_recv(),
                Ok(EngineOutput::SendToValidator(
                    _,
                    ConsensusMessage::CommitVote(_)
                ))
            ),
            "invalid PrepareQC must not trigger a CommitVote"
        );
    }

    #[test]
    fn test_prepare_qc_rejects_view_mismatch_between_wrapper_and_qc() {
        let (mut engine, sks, vs, mut rx) = make_engine(4, 0);
        let block_hash = B256::repeat_byte(0xF4);
        let view = 1u64;

        let prop_msg = crate::protocol::quorum::proposal_signing_message(view, &block_hash, &None);
        let proposal = n42_primitives::consensus::Proposal {
            view,
            block_hash,
            justify_qc: QuorumCertificate::genesis(),
            proposer: 1,
            signature: sks[1].sign(&prop_msg),
            prepare_qc: None,
            tx_root_hash: None,
            validator_changes: None,
        };
        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Proposal(
                proposal,
            )))
            .expect("proposal should succeed");
        while rx.try_recv().is_ok() {}

        let qc = build_test_prepare_qc(2, block_hash, &sks, &vs, &[0, 1, 2]);
        let prepare_qc = n42_primitives::consensus::PrepareQC {
            view,
            block_hash,
            qc,
        };
        let result = engine.process_event(ConsensusEvent::Message(ConsensusMessage::PrepareQC(
            prepare_qc,
        )));

        assert!(matches!(
            result,
            Err(crate::error::ConsensusError::ViewMismatch {
                current,
                received,
            }) if current == view && received == 2
        ));
        assert_eq!(engine.current_phase(), Phase::Voting);
    }

    #[test]
    fn test_process_vote_rejects_invalid_signature() {
        use crate::protocol::quorum::signing_message;

        let (mut engine, sks, _, mut rx) = make_engine(4, 1);
        let block_hash = B256::repeat_byte(0xA1);
        let view = 1u64;

        engine
            .process_event(ConsensusEvent::BlockReady(block_hash, None))
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
            crate::error::ConsensusError::InvalidSignature {
                view: v,
                validator_index,
            } => {
                assert_eq!(v, view);
                assert_eq!(validator_index, 0);
            }
            other => panic!("expected InvalidSignature, got: {:?}", other),
        }
    }

    #[test]
    fn test_authenticated_signer_accepts_valid_vote_signature() {
        use crate::protocol::quorum::signing_message;

        let (engine, sks, _, _) = make_engine(4, 0);
        let block_hash = B256::repeat_byte(0xC1);
        let vote = Vote {
            view: 7,
            block_hash,
            voter: 2,
            signature: sks[2].sign(&signing_message(7, &block_hash)),
        };

        assert_eq!(
            engine.authenticated_signer(&ConsensusMessage::Vote(vote)),
            Some(2)
        );
    }

    #[test]
    fn test_authenticated_signer_rejects_invalid_vote_signature() {
        use crate::protocol::quorum::signing_message;

        let (engine, sks, _, _) = make_engine(4, 0);
        let block_hash = B256::repeat_byte(0xC2);
        let vote = Vote {
            view: 7,
            block_hash,
            voter: 2,
            signature: sks[1].sign(&signing_message(7, &block_hash)),
        };

        assert_eq!(
            engine.authenticated_signer(&ConsensusMessage::Vote(vote)),
            None
        );
    }

    #[test]
    fn test_process_vote_ignores_wrong_block() {
        use crate::protocol::quorum::signing_message;

        let (mut engine, sks, _, mut rx) = make_engine(4, 1);
        let block_hash = B256::repeat_byte(0xA2);
        let wrong_block = B256::repeat_byte(0xFF);
        let view = 1u64;

        engine
            .process_event(ConsensusEvent::BlockReady(block_hash, None))
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
            .process_event(ConsensusEvent::BlockReady(block_hash, None))
            .expect("block ready");
        while rx.try_recv().is_ok() {}

        for i in [0u32, 2] {
            let msg = signing_message(view, &block_hash);
            let vote = Vote {
                view,
                block_hash,
                voter: i,
                signature: sks[i as usize].sign(&msg),
            };
            engine
                .process_event(ConsensusEvent::Message(ConsensusMessage::Vote(vote)))
                .expect("vote should succeed");
        }
        while rx.try_recv().is_ok() {}

        let wrong_msg = commit_signing_message(99, &block_hash, &alloy_primitives::B256::ZERO);
        let cv = CommitVote {
            view,
            block_hash,
            voter: 0,
            signature: sks[0].sign(&wrong_msg),
        };

        let result =
            engine.process_event(ConsensusEvent::Message(ConsensusMessage::CommitVote(cv)));
        assert!(result.is_err());
        match result.unwrap_err() {
            crate::error::ConsensusError::InvalidSignature {
                view: v,
                validator_index,
            } => {
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

        let result =
            engine.process_event(ConsensusEvent::Message(ConsensusMessage::Timeout(timeout)));
        assert!(result.is_err());
        match result.unwrap_err() {
            crate::error::ConsensusError::InvalidSignature {
                view: v,
                validator_index,
            } => {
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
            .process_event(ConsensusEvent::BlockReady(block_hash, None))
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
        while let Ok(o) = rx.try_recv() {
            outputs.push(o);
        }
        assert!(outputs.iter().any(|o| matches!(
            o,
            EngineOutput::BroadcastMessage(ConsensusMessage::PrepareQC(_))
        )));

        for i in [0u32, 2] {
            let msg = commit_signing_message(view, &block_hash, &alloy_primitives::B256::ZERO);
            let cv = CommitVote {
                view,
                block_hash,
                voter: i,
                signature: sks[i as usize].sign(&msg),
            };
            engine
                .process_event(ConsensusEvent::Message(ConsensusMessage::CommitVote(cv)))
                .expect("commit vote should succeed");
        }

        assert_eq!(engine.current_view(), 2);
        let mut outputs = vec![];
        while let Ok(o) = rx.try_recv() {
            outputs.push(o);
        }
        assert!(outputs.iter().any(|o| matches!(
            o,
            EngineOutput::BroadcastMessage(ConsensusMessage::Decide(_))
        )));
        assert!(
            outputs
                .iter()
                .any(|o| matches!(o, EngineOutput::BlockCommitted { .. }))
        );
    }

    #[test]
    fn test_decide_advances_follower() {
        let (mut engine, sks, vs, mut rx) = make_engine(4, 0);
        let block_hash = B256::repeat_byte(0xD1);
        let view = 1u64;

        let commit_qc = build_test_commit_qc(view, block_hash, &sks, &vs, &[0, 1, 2]);
        let decide = Decide {
            view,
            block_hash,
            commit_qc,
            validator_changes_hash: alloy_primitives::B256::ZERO,
        };

        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Decide(decide)))
            .expect("valid Decide should succeed");

        assert_eq!(engine.current_view(), 2);
        assert_eq!(engine.current_phase(), Phase::WaitingForProposal);

        let mut outputs = vec![];
        while let Ok(o) = rx.try_recv() {
            outputs.push(o);
        }
        let has_committed = outputs.iter().any(|o| {
            matches!(
                o,
                EngineOutput::BlockCommitted { block_hash: h, view: v, .. }
                if *h == block_hash && *v == view
            )
        });
        assert!(has_committed);
    }

    #[test]
    fn test_decide_missing_validator_changes_requests_resync_for_committed_view() {
        let (mut engine, sks, vs, mut rx) = make_engine(4, 0);
        let block_hash = B256::repeat_byte(0xD9);
        let view = 2u64;

        // The commit_qc must be signed under the same changes_hash that the
        // Decide carries (Plan #2: R2 signing message binds to changes_hash).
        let changes_hash = B256::repeat_byte(0x42);
        let commit_qc = build_test_commit_qc_with_changes(
            view,
            block_hash,
            &sks,
            &vs,
            &[0, 1, 2],
            &changes_hash,
        );
        let decide = Decide {
            view,
            block_hash,
            commit_qc,
            validator_changes_hash: changes_hash,
        };

        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Decide(decide)))
            .expect("decide with missed validator changes should still commit");

        let outputs: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(outputs.iter().any(|o| matches!(
            o,
            EngineOutput::SyncRequired {
                local_view: 1,
                target_view: 2,
            }
        )));
    }

    #[test]
    fn test_late_committed_proposal_recovers_validator_changes_before_boundary() {
        use crate::protocol::quorum::proposal_signing_message;
        use crate::validator::LeaderSelector;

        let sks: Vec<_> = (0..4).map(|i| test_key(0x30 + i as u8)).collect();
        let infos_epoch_0: Vec<_> = sks
            .iter()
            .take(3)
            .enumerate()
            .map(|(i, sk)| ValidatorInfo {
                address: Address::with_last_byte(i as u8),
                bls_public_key: sk.public_key(),
                p2p_peer_id: None,
            })
            .collect();
        let vs_epoch_0 = ValidatorSet::new(&infos_epoch_0, 0);
        let epoch_manager = EpochManager::with_epoch_length(vs_epoch_0.clone(), 2);
        let (output_tx, mut output_rx) = mpsc::channel(128);
        let mut engine = ConsensusEngine::with_epoch_manager(
            1,
            sks[1].clone(),
            epoch_manager,
            60000,
            120000,
            output_tx,
        );

        let add_change = ValidatorChange::Add {
            address: Address::with_last_byte(3),
            bls_public_key: sks[3].public_key(),
            p2p_peer_id: None,
        };
        let changes = vec![add_change];
        let changes_hash = EpochManager::hash_changes(&changes);

        let block_hash_1 = B256::repeat_byte(0xE1);
        let decide_1 = Decide {
            view: 1,
            block_hash: block_hash_1,
            commit_qc: build_test_commit_qc_with_changes(
                1,
                block_hash_1,
                &sks,
                &vs_epoch_0,
                &[0],
                &changes_hash,
            ),
            validator_changes_hash: changes_hash,
        };
        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Decide(decide_1)))
            .expect("decide should succeed even when proposal changes were missed");

        assert_eq!(engine.current_view(), 2);
        assert!(!engine.epoch_manager().has_staged_next());

        let proposer = LeaderSelector::leader_for_view(1, &vs_epoch_0);
        let proposal = Proposal {
            view: 1,
            block_hash: block_hash_1,
            justify_qc: QuorumCertificate::genesis(),
            proposer,
            signature: sks[proposer as usize].sign(&proposal_signing_message(
                1,
                &block_hash_1,
                &Some(changes.clone()),
            )),
            prepare_qc: None,
            tx_root_hash: None,
            validator_changes: Some(changes),
        };
        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Proposal(
                proposal,
            )))
            .expect("late committed proposal should recover validator changes");

        assert!(engine.epoch_manager().has_staged_next());

        let block_hash_2 = B256::repeat_byte(0xE2);
        let decide_2 = Decide {
            view: 2,
            block_hash: block_hash_2,
            commit_qc: build_test_commit_qc(2, block_hash_2, &sks, &vs_epoch_0, &[0]),
            validator_changes_hash: B256::ZERO,
        };
        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Decide(decide_2)))
            .expect("next decide should cross the epoch boundary");

        assert_eq!(engine.epoch_manager().current_epoch(), 1);
        assert_eq!(engine.epoch_manager().current_validator_set().len(), 4);

        let outputs: Vec<_> = std::iter::from_fn(|| output_rx.try_recv().ok()).collect();
        assert!(outputs.iter().any(|o| matches!(
            o,
            EngineOutput::CommittedBlockValidatorChangesRecovered {
                view: 1,
                block_hash,
                validator_changes,
            } if *block_hash == block_hash_1 && validator_changes.len() == 1
        )));
        assert!(outputs.iter().any(|o| matches!(
            o,
            EngineOutput::EpochTransition {
                new_epoch: 1,
                validator_count: 4,
            }
        )));
    }

    #[test]
    fn test_decide_stale_ignored() {
        let (mut engine, sks, vs, mut rx) = make_engine(4, 0);
        let block_hash = B256::repeat_byte(0xD2);

        for v in 1u64..=4 {
            let bh = B256::repeat_byte(v as u8);
            let cqc = build_test_commit_qc(v, bh, &sks, &vs, &[0, 1, 2]);
            let _ =
                engine.process_event(ConsensusEvent::Message(ConsensusMessage::Decide(Decide {
                    view: v,
                    block_hash: bh,
                    commit_qc: cqc,
                    validator_changes_hash: alloy_primitives::B256::ZERO,
                })));
            while rx.try_recv().is_ok() {}
        }
        assert_eq!(engine.current_view(), 5);

        let commit_qc = build_test_commit_qc(3, block_hash, &sks, &vs, &[0, 1, 2]);
        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Decide(Decide {
                view: 3,
                block_hash,
                commit_qc,
                validator_changes_hash: alloy_primitives::B256::ZERO,
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

        let result =
            engine.process_event(ConsensusEvent::Message(ConsensusMessage::Decide(Decide {
                view,
                block_hash,
                commit_qc: weak_qc,
                validator_changes_hash: alloy_primitives::B256::ZERO,
            })));
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
        use crate::protocol::quorum::signing_message;
        use bitvec::prelude::*;

        let (mut engine, sks, _vs, _rx) = make_engine(4, 0);
        let block_hash = B256::repeat_byte(0xD4);
        let view = 1u64;

        let wrong_msg = signing_message(view, &block_hash);
        let sigs: Vec<_> = (0..3u32)
            .map(|i| sks[i as usize].sign(&wrong_msg))
            .collect();
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
        let result =
            engine.process_event(ConsensusEvent::Message(ConsensusMessage::Decide(Decide {
                view,
                block_hash,
                commit_qc: forged_qc,
                validator_changes_hash: alloy_primitives::B256::ZERO,
            })));
        assert!(result.is_err());
        match result.unwrap_err() {
            crate::error::ConsensusError::InvalidQC { view: v, reason } => {
                assert_eq!(v, view);
                assert!(reason.contains("commit QC"), "got: {reason}");
            }
            other => panic!("expected InvalidQC, got: {:?}", other),
        }
    }

    /// Plan #2 regression: a Byzantine leader cannot attach a fake
    /// `validator_changes_hash` to a Decide whose CommitQC was signed under
    /// a different changes_hash. The R2 commit-vote signing message now
    /// binds the hash, so the BLS aggregate verify catches the mismatch.
    #[test]
    fn test_decide_rejects_forged_changes_hash() {
        let (mut engine, sks, vs, _rx) = make_engine(4, 0);
        let block_hash = B256::repeat_byte(0xCB);
        let view = 1u64;

        // Honest commit_qc — signed under ZERO (no validator changes).
        let honest_commit_qc = build_test_commit_qc(view, block_hash, &sks, &vs, &[0, 1, 2]);
        // Byzantine leader keeps the same commit_qc but swaps the
        // changes_hash to something nonzero, hoping followers will commit
        // their local pending changes.
        let forged_decide = Decide {
            view,
            block_hash,
            commit_qc: honest_commit_qc,
            validator_changes_hash: B256::repeat_byte(0xEE),
        };
        let err = engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Decide(
                forged_decide,
            )))
            .expect_err("forged changes_hash must fail commit_qc verification");
        match err {
            crate::error::ConsensusError::InvalidQC { reason, .. } => {
                assert!(reason.contains("commit QC"), "got: {reason}");
            }
            other => panic!("expected InvalidQC, got: {other:?}"),
        }
    }

    #[test]
    fn test_block_imported_caches_hash() {
        let (mut engine, _sks, _, _rx) = make_engine(4, 0);
        let hash = B256::repeat_byte(0xBB);

        engine
            .process_event(ConsensusEvent::BlockImported(hash))
            .expect("BlockImported should succeed");

        // With optimistic voting, on_block_imported simply tracks the hash.
        assert!(engine.imported_blocks.contains(&hash));
    }

    #[test]
    fn test_imported_blocks_capacity_bound() {
        let (mut engine, _sks, _, _rx) = make_engine(4, 0);

        for i in 0..64u8 {
            engine
                .process_event(ConsensusEvent::BlockImported(B256::repeat_byte(i)))
                .expect("BlockImported should succeed");
        }
        assert_eq!(engine.imported_blocks.len(), 64);

        engine
            .process_event(ConsensusEvent::BlockImported(B256::repeat_byte(0xFF)))
            .expect("BlockImported at capacity should succeed");
        assert_eq!(engine.imported_blocks.len(), 64);
        assert!(!engine.imported_blocks.contains(&B256::repeat_byte(0xFF)));
    }

    #[test]
    fn test_far_future_proposal_triggers_view_jump() {
        let (mut engine, sks, vs, mut rx) = make_engine(4, 0);
        assert_eq!(engine.current_view(), 1);

        let far_view = 100u64;
        let block_hash = B256::repeat_byte(0xF0);
        let justify_qc = build_test_prepare_qc(99, B256::repeat_byte(0xEF), &sks, &vs, &[0, 1, 2]);

        let msg = crate::protocol::quorum::proposal_signing_message(far_view, &block_hash, &None);
        let proposal = n42_primitives::consensus::Proposal {
            view: far_view,
            block_hash,
            justify_qc,
            proposer: 0,
            signature: sks[0].sign(&msg),
            prepare_qc: None,
            tx_root_hash: None,
            validator_changes: None,
        };

        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Proposal(
                proposal,
            )))
            .expect("far-future proposal with valid QC should trigger view jump");

        assert_eq!(engine.current_view(), far_view);

        let mut outputs = vec![];
        while let Ok(o) = rx.try_recv() {
            outputs.push(o);
        }
        assert!(outputs.iter().any(|o| matches!(
            o,
            EngineOutput::SyncRequired {
                local_view: 1,
                target_view: 100
            }
        )));
        assert!(
            outputs
                .iter()
                .any(|o| matches!(o, EngineOutput::ViewChanged { new_view: 100 }))
        );
    }

    #[test]
    fn test_far_future_proposal_with_commit_qc_triggers_view_jump() {
        let (mut engine, sks, vs, mut rx) = make_engine(4, 0);
        assert_eq!(engine.current_view(), 1);

        let far_view = 100u64;
        let block_hash = B256::repeat_byte(0xF5);
        let justify_qc = build_test_commit_qc(99, B256::repeat_byte(0xF4), &sks, &vs, &[0, 1, 2]);

        let msg = crate::protocol::quorum::proposal_signing_message(far_view, &block_hash, &None);
        let proposal = n42_primitives::consensus::Proposal {
            view: far_view,
            block_hash,
            justify_qc,
            proposer: 0,
            signature: sks[0].sign(&msg),
            prepare_qc: None,
            tx_root_hash: None,
            validator_changes: None,
        };

        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Proposal(
                proposal,
            )))
            .expect("far-future proposal with commit QC should trigger view jump");

        assert_eq!(engine.current_view(), far_view);

        let outputs: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(outputs.iter().any(|o| matches!(
            o,
            EngineOutput::SyncRequired {
                local_view: 1,
                target_view: 100
            }
        )));
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
    fn test_far_future_timeout_with_commit_qc_triggers_view_jump() {
        use crate::protocol::quorum::timeout_signing_message;
        use n42_primitives::consensus::TimeoutMessage;

        let (mut engine, sks, vs, mut rx) = make_engine(4, 0);
        assert_eq!(engine.current_view(), 1);

        let far_view = 200u64;
        let high_qc = build_test_commit_qc(190, B256::repeat_byte(0xBD), &sks, &vs, &[0, 1, 2]);

        let msg = timeout_signing_message(far_view);
        let timeout = TimeoutMessage {
            view: far_view,
            high_qc,
            sender: 2,
            signature: sks[2].sign(&msg),
        };

        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Timeout(timeout)))
            .expect("far-future timeout with commit QC should trigger view jump");

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
        let vote = Vote {
            view: far_view,
            block_hash,
            voter: 2,
            signature: sks[2].sign(&msg),
        };

        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Vote(vote)))
            .expect("far-future vote should be silently dropped");

        assert_eq!(engine.current_view(), 1);
    }

    #[test]
    fn test_far_future_invalid_qc_is_dropped() {
        use crate::protocol::quorum::signing_message;
        use bitvec::prelude::*;

        let (mut engine, sks, _, _rx) = make_engine(4, 0);
        assert_eq!(engine.current_view(), 1);

        let far_view = 100u64;
        let block_hash = B256::repeat_byte(0xBB);

        let wrong_msg = signing_message(99, &B256::repeat_byte(0xFF));
        let sigs: Vec<_> = (0..3u32)
            .map(|i| sks[i as usize].sign(&wrong_msg))
            .collect();
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

        let msg = crate::protocol::quorum::proposal_signing_message(far_view, &block_hash, &None);
        let proposal = n42_primitives::consensus::Proposal {
            view: far_view,
            block_hash,
            justify_qc: forged_qc,
            proposer: 0,
            signature: sks[0].sign(&msg),
            prepare_qc: None,
            tx_root_hash: None,
            validator_changes: None,
        };

        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Proposal(
                proposal,
            )))
            .expect("far-future proposal with invalid QC should be silently dropped");

        assert_eq!(engine.current_view(), 1);
    }

    #[test]
    fn test_far_future_genesis_qc_is_dropped() {
        let (mut engine, sks, _, _rx) = make_engine(4, 0);
        let far_view = 100u64;
        let block_hash = B256::repeat_byte(0xCC);

        let msg = crate::protocol::quorum::proposal_signing_message(far_view, &block_hash, &None);
        let proposal = n42_primitives::consensus::Proposal {
            view: far_view,
            block_hash,
            justify_qc: QuorumCertificate::genesis(),
            proposer: 0,
            signature: sks[0].sign(&msg),
            prepare_qc: None,
            tx_root_hash: None,
            validator_changes: None,
        };

        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Proposal(
                proposal,
            )))
            .expect("far-future proposal with genesis QC should be silently dropped");

        assert_eq!(engine.current_view(), 1);
    }

    #[test]
    fn test_view_jump_resets_consecutive_timeouts() {
        let (mut engine, sks, vs, mut rx) = make_engine(4, 0);

        engine.on_timeout().expect("timeout");
        while rx.try_recv().is_ok() {}
        assert!(engine.consecutive_timeouts() >= 1);

        let far_view = 100u64;
        let block_hash = B256::repeat_byte(0xDD);
        let justify_qc = build_test_prepare_qc(99, B256::repeat_byte(0xDE), &sks, &vs, &[0, 1, 2]);

        let msg = crate::protocol::quorum::proposal_signing_message(far_view, &block_hash, &None);
        let proposal = n42_primitives::consensus::Proposal {
            view: far_view,
            block_hash,
            justify_qc,
            proposer: 0,
            signature: sks[0].sign(&msg),
            prepare_qc: None,
            tx_root_hash: None,
            validator_changes: None,
        };

        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Proposal(
                proposal,
            )))
            .expect("should trigger view jump");

        assert_eq!(engine.current_view(), 100);
        assert_eq!(engine.consecutive_timeouts(), 0);
    }

    #[test]
    fn test_view_jump_updates_locked_qc() {
        let (mut engine, sks, vs, mut rx) = make_engine(4, 0);
        assert_eq!(engine.locked_qc().view, 0);

        let far_view = 100u64;
        let block_hash = B256::repeat_byte(0xEE);
        let justify_qc = build_test_prepare_qc(95, B256::repeat_byte(0x95), &sks, &vs, &[0, 1, 2]);

        let msg = crate::protocol::quorum::proposal_signing_message(far_view, &block_hash, &None);
        let proposal = n42_primitives::consensus::Proposal {
            view: far_view,
            block_hash,
            justify_qc,
            proposer: 0,
            signature: sks[0].sign(&msg),
            prepare_qc: None,
            tx_root_hash: None,
            validator_changes: None,
        };

        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Proposal(
                proposal,
            )))
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
            view: 1,
            block_hash: B256::repeat_byte(0x01),
            commit_qc,
            validator_changes_hash: alloy_primitives::B256::ZERO,
        })));
        while rx.try_recv().is_ok() {}

        let commit_qc2 = build_test_commit_qc(2, B256::repeat_byte(0x02), &sks, &vs, &[0, 1, 2]);
        let _ = engine.process_event(ConsensusEvent::Message(ConsensusMessage::Decide(Decide {
            view: 2,
            block_hash: B256::repeat_byte(0x02),
            commit_qc: commit_qc2,
            validator_changes_hash: alloy_primitives::B256::ZERO,
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
        assert_eq!(
            engine.locked_qc().view,
            50,
            "locked_qc should NOT downgrade from 50 to 30"
        );
    }

    #[test]
    fn test_increased_future_view_window_buffers_50() {
        use crate::protocol::quorum::signing_message;

        let (mut engine, sks, _, _rx) = make_engine(4, 0);
        assert_eq!(engine.current_view(), 1);

        let view = 51u64;
        let block_hash = B256::repeat_byte(0xFF);
        let msg = signing_message(view, &block_hash);
        let vote = Vote {
            view,
            block_hash,
            voter: 2,
            signature: sks[2].sign(&msg),
        };

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
        let decide = Decide {
            view: far_view,
            block_hash,
            commit_qc,
            validator_changes_hash: alloy_primitives::B256::ZERO,
        };

        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Decide(decide)))
            .expect("Decide should use direct bypass path");

        assert_eq!(engine.current_view(), 101);

        let mut outputs = vec![];
        while let Ok(o) = rx.try_recv() {
            outputs.push(o);
        }
        assert!(
            outputs
                .iter()
                .any(|o| matches!(o, EngineOutput::SyncRequired { .. }))
        );
        assert!(
            outputs
                .iter()
                .any(|o| matches!(o, EngineOutput::BlockCommitted { .. }))
        );
    }

    #[test]
    fn test_on_timeout_repeat_rebroadcasts() {
        // Repeat on_timeout() calls intentionally re-broadcast the Timeout message so
        // validators who missed the first message (network jitter, late join) can still
        // collect our vote for TC formation. The consecutive_timeouts counter must NOT
        // increment on repeats (pacemaker backoff should not compound).
        let (mut engine, _, _, mut rx) = make_engine(4, 0);

        engine.on_timeout().expect("first timeout");
        let t1 = engine.consecutive_timeouts();
        let mut outputs = vec![];
        while let Ok(o) = rx.try_recv() {
            outputs.push(o);
        }
        let broadcast_count_1 = outputs
            .iter()
            .filter(|o| {
                matches!(
                    o,
                    EngineOutput::BroadcastMessage(ConsensusMessage::Timeout(_))
                )
            })
            .count();
        assert_eq!(broadcast_count_1, 1);

        engine.on_timeout().expect("repeat timeout");
        // consecutive_timeouts must not increment on repeat (no double backoff).
        assert_eq!(engine.consecutive_timeouts(), t1);
        let mut outputs = vec![];
        while let Ok(o) = rx.try_recv() {
            outputs.push(o);
        }
        let broadcast_count_2 = outputs
            .iter()
            .filter(|o| {
                matches!(
                    o,
                    EngineOutput::BroadcastMessage(ConsensusMessage::Timeout(_))
                )
            })
            .count();
        // Repeat timeout re-broadcasts to help late/slow validators collect our vote.
        assert_eq!(broadcast_count_2, 1);
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
        let outputs: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(
            !outputs
                .iter()
                .any(|o| matches!(o, EngineOutput::SyncRequired { .. })),
            "genesis high_qc should not trigger sync"
        );
    }

    #[test]
    fn test_observer_future_timeout_triggers_sync_without_invalid_rebroadcast() {
        use crate::protocol::quorum::timeout_signing_message;
        use n42_primitives::consensus::TimeoutMessage;

        let sks: Vec<_> = (0..4).map(|i| test_key(0x90 + i as u8)).collect();
        let infos: Vec<ValidatorInfo> = sks[..3]
            .iter()
            .enumerate()
            .map(|(i, sk)| ValidatorInfo {
                address: Address::with_last_byte(i as u8),
                bls_public_key: sk.public_key(),
                p2p_peer_id: None,
            })
            .collect();
        let vs = ValidatorSet::new(&infos, 0);
        let (output_tx, mut rx) = mpsc::channel(64);
        let mut engine =
            ConsensusEngine::new(3, sks[3].clone(), vs.clone(), 60_000, 120_000, output_tx);

        let timeout_view = 31u64;
        let high_qc = build_test_prepare_qc(29, B256::repeat_byte(0xAC), &sks, &vs, &[0, 1, 2]);
        let timeout = TimeoutMessage {
            view: timeout_view,
            high_qc,
            sender: 1,
            signature: sks[1].sign(&timeout_signing_message(timeout_view)),
        };

        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Timeout(timeout)))
            .expect("observer should accept future timeout for sync");

        assert_eq!(engine.current_view(), timeout_view);

        let outputs: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(outputs.iter().any(|o| matches!(
            o,
            EngineOutput::SyncRequired {
                local_view: 0,
                target_view: 29
            }
        )));
        assert!(
            !outputs.iter().any(|o| matches!(
                o,
                EngineOutput::BroadcastMessage(ConsensusMessage::Timeout(_))
            )),
            "observer must not rebroadcast timeout with a stale/provisional index"
        );
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
        while let Ok(o) = rx.try_recv() {
            outputs.push(o);
        }
        assert!(outputs.iter().any(|o| matches!(
            o,
            EngineOutput::BroadcastMessage(ConsensusMessage::NewView(_))
        )));
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
        while let Ok(o) = rx.try_recv() {
            outputs.push(o);
        }
        assert!(!outputs.iter().any(|o| matches!(
            o,
            EngineOutput::BroadcastMessage(ConsensusMessage::NewView(_))
        )));
        assert_eq!(engine.current_view(), 1);
    }

    #[test]
    fn test_advance_to_view_replays_buffered() {
        use crate::protocol::quorum::signing_message;

        let (mut engine, sks, vs, mut rx) = make_engine(4, 0);
        assert_eq!(engine.current_view(), 1);

        let view5_hash = B256::repeat_byte(0x55);
        let msg = signing_message(5, &view5_hash);
        let vote = Vote {
            view: 5,
            block_hash: view5_hash,
            voter: 2,
            signature: sks[2].sign(&msg),
        };
        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Vote(vote)))
            .expect("future Vote should be buffered");

        assert_eq!(engine.current_view(), 1);
        assert_eq!(engine.future_msg_buffer.len(), 1);
        assert_eq!(engine.future_msg_buffer[0].0, 5);

        for v in 1u64..=4 {
            let bh = B256::repeat_byte(v as u8);
            let cqc = build_test_commit_qc(v, bh, &sks, &vs, &[0, 1, 2]);
            let _ =
                engine.process_event(ConsensusEvent::Message(ConsensusMessage::Decide(Decide {
                    view: v,
                    block_hash: bh,
                    commit_qc: cqc,
                    validator_changes_hash: alloy_primitives::B256::ZERO,
                })));
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
            let _ =
                engine.process_event(ConsensusEvent::Message(ConsensusMessage::Decide(Decide {
                    view: v,
                    block_hash: bh,
                    commit_qc: cqc,
                    validator_changes_hash: alloy_primitives::B256::ZERO,
                })));
        }
        while rx.try_recv().is_ok() {}
        assert_eq!(engine.current_view(), 10);

        let _ = engine.advance_to_view(5);
        assert_eq!(engine.current_view(), 10);

        let _ = engine.advance_to_view(10);
        assert_eq!(engine.current_view(), 10);

        let _ = engine.advance_to_view(15);
        assert_eq!(engine.current_view(), 15);
    }

    #[test]
    fn test_buffer_eviction_removes_lowest_view() {
        use crate::protocol::quorum::signing_message;

        let (mut engine, sks, _, _rx) = make_engine(4, 0);
        assert_eq!(engine.current_view(), 1);

        // Directly fill the buffer beyond MAX_FUTURE_MESSAGES to test eviction logic.
        // Using process_event is limited by FUTURE_VIEW_WINDOW, so populate directly.
        for v in 2u64..=(MAX_FUTURE_MESSAGES as u64 + 5) {
            let block_hash = B256::repeat_byte((v % 256) as u8);
            let msg = signing_message(v, &block_hash);
            let vote = Vote {
                view: v,
                block_hash,
                voter: 1,
                signature: sks[1].sign(&msg),
            };

            // Simulate the eviction logic from process_message
            if engine.future_msg_buffer.len() >= MAX_FUTURE_MESSAGES {
                if let Some(min_idx) = engine
                    .future_msg_buffer
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, (view, _))| *view)
                    .map(|(i, _)| i)
                {
                    engine.future_msg_buffer.swap_remove(min_idx);
                }
            }
            engine
                .future_msg_buffer
                .push((v, ConsensusMessage::Vote(vote)));
        }

        assert_eq!(engine.future_msg_buffer.len(), MAX_FUTURE_MESSAGES);

        let min_view = engine
            .future_msg_buffer
            .iter()
            .map(|(v, _)| *v)
            .min()
            .unwrap_or(0);
        assert!(
            min_view >= 3,
            "lowest view in buffer should be >= 3 after eviction, got {min_view}"
        );
    }

    #[test]
    fn test_qc_jump_emits_sync_required() {
        let (mut engine, sks, vs, mut rx) = make_engine(4, 0);
        assert_eq!(engine.current_view(), 1);

        let far_view = 80u64;
        let block_hash = B256::repeat_byte(0xAB);
        let justify_qc = build_test_prepare_qc(79, B256::repeat_byte(0x79), &sks, &vs, &[0, 1, 2]);

        let msg = crate::protocol::quorum::proposal_signing_message(far_view, &block_hash, &None);
        let proposal = n42_primitives::consensus::Proposal {
            view: far_view,
            block_hash,
            justify_qc,
            proposer: 0,
            signature: sks[0].sign(&msg),
            prepare_qc: None,
            tx_root_hash: None,
            validator_changes: None,
        };

        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Proposal(
                proposal,
            )))
            .expect("far-future proposal should trigger QC jump");

        let mut outputs = vec![];
        while let Ok(o) = rx.try_recv() {
            outputs.push(o);
        }

        let sync = outputs.iter().find_map(|o| match o {
            EngineOutput::SyncRequired {
                local_view,
                target_view,
            } => Some((*local_view, *target_view)),
            _ => None,
        });
        assert!(sync.is_some());
        let (local, target) = sync.unwrap();
        assert_eq!(local, 1);
        assert_eq!(target, 80);
    }

    #[test]
    fn test_stale_view_changed_prevented() {
        let (mut engine, sks, vs, mut rx) = make_engine(4, 0);
        assert_eq!(engine.current_view(), 1);

        let justify_qc = build_test_prepare_qc(79, B256::repeat_byte(0x79), &sks, &vs, &[0, 1, 2]);
        let msg =
            crate::protocol::quorum::proposal_signing_message(80, &B256::repeat_byte(0xAB), &None);
        let proposal = n42_primitives::consensus::Proposal {
            view: 80,
            block_hash: B256::repeat_byte(0xAB),
            justify_qc,
            proposer: 0,
            signature: sks[0].sign(&msg),
            prepare_qc: None,
            tx_root_hash: None,
            validator_changes: None,
        };

        engine
            .process_event(ConsensusEvent::Message(ConsensusMessage::Proposal(
                proposal,
            )))
            .expect("should trigger QC jump to view 80");

        let actual_view = engine.current_view();
        assert!(actual_view >= 80);

        let mut outputs = vec![];
        while let Ok(o) = rx.try_recv() {
            outputs.push(o);
        }

        let view_changed_values: Vec<ViewNumber> = outputs
            .iter()
            .filter_map(|o| match o {
                EngineOutput::ViewChanged { new_view } => Some(*new_view),
                _ => None,
            })
            .collect();

        assert!(!view_changed_values.is_empty());

        let last_vc = *view_changed_values.last().unwrap();
        assert_eq!(
            last_vc, actual_view,
            "ViewChanged should report actual current_view ({}), got {}",
            actual_view, last_vc
        );
    }
}
