use alloy_primitives::B256;
use n42_primitives::{
    BlsSecretKey,
    consensus::{
        CommitVote, ConsensusMessage, Decide, NewView, PrepareQC, Proposal, QuorumCertificate,
        TimeoutMessage, ViewNumber, Vote,
    },
};
use std::collections::HashSet;
use tokio::sync::mpsc;

use crate::error::{ConsensusError, ConsensusResult};
use crate::validator::{LeaderSelector, ValidatorSet};
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
}

/// Pending proposal awaiting block data import before voting (follower path).
#[derive(Debug, Clone)]
struct PendingProposal {
    view: ViewNumber,
    block_hash: B256,
}

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
    /// The active validator set.
    validator_set: ValidatorSet,
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
    /// Output channel for engine actions.
    output_tx: mpsc::UnboundedSender<EngineOutput>,
    /// Pending proposal awaiting block data import before voting.
    /// Set when process_proposal() defers the vote (follower path).
    pending_proposal: Option<PendingProposal>,
    /// Block hashes that have been imported but no matching proposal yet.
    /// Handles the case where BlockData arrives before Proposal.
    imported_blocks: HashSet<B256>,
}

impl ConsensusEngine {
    /// Creates a new consensus engine.
    pub fn new(
        my_index: u32,
        secret_key: BlsSecretKey,
        validator_set: ValidatorSet,
        base_timeout_ms: u64,
        max_timeout_ms: u64,
        output_tx: mpsc::UnboundedSender<EngineOutput>,
    ) -> Self {
        let pacemaker = Pacemaker::new(base_timeout_ms, max_timeout_ms);

        Self {
            my_index,
            secret_key,
            validator_set,
            round_state: RoundState::new(),
            pacemaker,
            vote_collector: None,
            commit_collector: None,
            timeout_collector: None,
            prepare_qc: None,
            output_tx,
            pending_proposal: None,
            imported_blocks: HashSet::new(),
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

    /// Checks if this node is the leader for the current view.
    pub fn is_current_leader(&self) -> bool {
        LeaderSelector::is_leader(
            self.my_index,
            self.round_state.current_view(),
            &self.validator_set,
        )
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
        tracing::warn!(view, "view timed out");

        self.round_state.timeout();

        // Clear pending block data state: block data may never arrive.
        // Similar to Tendermint's "prevote nil" — timeout implies giving up on this view.
        self.pending_proposal = None;
        self.imported_blocks.clear();

        // Initialize timeout collector
        self.timeout_collector =
            Some(TimeoutCollector::new(view, self.validator_set.len()));

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

        let proposal = Proposal {
            view,
            block_hash,
            justify_qc,
            proposer: self.my_index,
            signature,
        };

        tracing::info!(view, %block_hash, "proposing block");

        // Initialize vote collectors for this view
        self.vote_collector =
            Some(VoteCollector::new(view, block_hash, self.validator_set.len()));
        self.commit_collector =
            Some(VoteCollector::new(view, block_hash, self.validator_set.len()));

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
            LeaderSelector::leader_for_view(view, &self.validator_set);
        if proposal.proposer != expected_leader {
            return Err(ConsensusError::InvalidProposer {
                view,
                expected: expected_leader,
                actual: proposal.proposer,
            });
        }

        // Verify proposer's signature
        let pk = self.validator_set.get_public_key(proposal.proposer)?;
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
        self.round_state.enter_voting();

        // Request block execution (orchestrator will import block data)
        self.emit(EngineOutput::ExecuteBlock(proposal.block_hash));

        // Check if the block has already been imported (BlockData arrived before Proposal).
        // Reference: Aptos Baby Raptr — "if local data exists, vote immediately".
        if self.imported_blocks.remove(&proposal.block_hash) {
            tracing::info!(view, block_hash = %proposal.block_hash, "received valid proposal, block already imported, voting immediately");
            self.send_vote(view, proposal.block_hash)?;
        } else {
            // Defer the vote until BlockImported arrives.
            tracing::info!(view, block_hash = %proposal.block_hash, "received valid proposal, deferring vote until block data imported");
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

        let collector = match self.vote_collector.as_mut() {
            Some(c) => c,
            None => return Ok(()),
        };

        // Verify the vote is for the correct block
        if vote.block_hash != collector.block_hash() {
            tracing::debug!(view, voter = vote.voter, "ignoring vote for different block");
            return Ok(());
        }

        // Verify BLS signature before adding to collector
        let pk = self.validator_set.get_public_key(vote.voter)?;
        let msg = signing_message(view, &vote.block_hash);
        pk.verify(&msg, &vote.signature).map_err(|_| {
            ConsensusError::InvalidSignature {
                view,
                validator_index: vote.voter,
            }
        })?;

        collector.add_vote(vote.voter, vote.signature)?;

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
            .is_some_and(|c| c.has_quorum(self.validator_set.quorum_size()));

        if !has_quorum {
            return Ok(());
        }

        // Already formed QC for this view
        if self.prepare_qc.is_some() {
            return Ok(());
        }

        let qc = self
            .vote_collector
            .as_ref()
            .unwrap()
            .build_qc(&self.validator_set)?;

        tracing::info!(view, signers = qc.signer_count(), "QC formed, entering pre-commit");

        self.prepare_qc = Some(qc.clone());
        self.round_state.enter_pre_commit();
        self.round_state.update_locked_qc(&qc);

        // Broadcast PrepareQC so validators can send CommitVotes
        let prepare_qc_msg = PrepareQC {
            view,
            block_hash: qc.block_hash,
            qc,
        };
        self.emit(EngineOutput::BroadcastMessage(
            ConsensusMessage::PrepareQC(prepare_qc_msg),
        ));

        // Leader self-vote for CommitVote (Round 2)
        let block_hash = self.prepare_qc.as_ref().unwrap().block_hash;
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
        super::quorum::verify_qc(&pqc.qc, &self.validator_set)?;

        // Update locked QC
        self.round_state.update_locked_qc(&pqc.qc);
        self.round_state.enter_pre_commit();

        tracing::info!(view, block_hash = %pqc.block_hash, "received valid PrepareQC, sending commit vote");

        // Send CommitVote (Round 2) to leader
        let commit_msg = commit_signing_message(view, &pqc.block_hash);
        let commit_sig = self.secret_key.sign(&commit_msg);

        let leader = LeaderSelector::leader_for_view(view, &self.validator_set);

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

        let collector = match self.commit_collector.as_mut() {
            Some(c) => c,
            None => return Ok(()),
        };

        // Verify the commit vote is for the correct block
        if cv.block_hash != collector.block_hash() {
            tracing::debug!(view, voter = cv.voter, "ignoring commit vote for different block");
            return Ok(());
        }

        // Verify BLS signature before adding to collector
        let pk = self.validator_set.get_public_key(cv.voter)?;
        let msg = commit_signing_message(view, &cv.block_hash);
        pk.verify(&msg, &cv.signature).map_err(|_| {
            ConsensusError::InvalidSignature {
                view,
                validator_index: cv.voter,
            }
        })?;

        collector.add_vote(cv.voter, cv.signature)?;

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
            .is_some_and(|c| c.has_quorum(self.validator_set.quorum_size()));

        if !has_quorum {
            return Ok(());
        }

        // CommitVotes use "commit" prefix in their signing message
        let block_hash = self
            .commit_collector
            .as_ref()
            .map(|c| c.block_hash())
            .unwrap_or_default();
        let commit_msg = commit_signing_message(view, &block_hash);

        let commit_qc = self
            .commit_collector
            .as_ref()
            .unwrap()
            .build_qc_with_message(&self.validator_set, &commit_msg)?;

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
        let quorum_size = self.validator_set.quorum_size();
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

        if timeout.view != view {
            // Accept timeouts for current view only
            if timeout.view > view {
                // We're behind, catch up
                self.advance_to_view(timeout.view);
            }
            return Ok(());
        }

        // Verify BLS signature on timeout message before adding to collector
        let pk = self.validator_set.get_public_key(timeout.sender)?;
        let msg = timeout_signing_message(view);
        pk.verify(&msg, &timeout.signature).map_err(|_| {
            ConsensusError::InvalidSignature {
                view,
                validator_index: timeout.sender,
            }
        })?;

        let collector = self
            .timeout_collector
            .get_or_insert_with(|| TimeoutCollector::new(view, self.validator_set.len()));

        collector.add_timeout(
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
        let next_view = view.saturating_add(1);
        let next_leader =
            LeaderSelector::leader_for_view(next_view, &self.validator_set);

        if collector.has_quorum(self.validator_set.quorum_size())
            && next_leader == self.my_index
        {
            let tc = collector.build_tc(&self.validator_set)?;

            tracing::info!(view, "TC formed, I am the new leader for view {}", next_view);

            // Update locked QC from TC's high_qc if higher
            self.round_state.update_locked_qc(&tc.high_qc);

            // Send NewView to all
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
            LeaderSelector::leader_for_view(nv.view, &self.validator_set);
        if nv.leader != expected_leader {
            return Err(ConsensusError::InvalidProposer {
                view: nv.view,
                expected: expected_leader,
                actual: nv.leader,
            });
        }

        // Update locked QC from TC's high_qc if higher
        self.round_state.update_locked_qc(&nv.timeout_cert.high_qc);

        tracing::info!(
            old_view = view,
            new_view = nv.view,
            "received NewView, advancing"
        );

        self.advance_to_view(nv.view);
        self.emit(EngineOutput::ViewChanged { new_view: nv.view });

        Ok(())
    }

    // ── Block data import handling ──

    /// Sends a Round 1 vote for the given view and block hash.
    /// Extracted from process_proposal() to support both immediate and deferred voting.
    fn send_vote(&mut self, view: ViewNumber, block_hash: B256) -> ConsensusResult<()> {
        let leader = LeaderSelector::leader_for_view(view, &self.validator_set);
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
                tracing::info!(
                    view = pending.view, %block_hash,
                    "block imported, sending deferred vote"
                );
                self.send_vote(pending.view, pending.block_hash)?;
            } else {
                // Hash mismatch: cache this import, restore the pending proposal
                self.imported_blocks.insert(block_hash);
                self.pending_proposal = Some(pending);
            }
        } else {
            // Case 2: BlockData arrived first, no Proposal yet → cache for later
            self.imported_blocks.insert(block_hash);
        }
        Ok(())
    }

    // ── Internal helpers ──

    fn advance_to_view(&mut self, new_view: ViewNumber) {
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

        tracing::debug!(view = new_view, "advanced to new view");
    }

    fn emit(&self, output: EngineOutput) {
        if self.output_tx.send(output).is_err() {
            tracing::error!("consensus output channel closed");
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
        mpsc::UnboundedReceiver<EngineOutput>,
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

        let (output_tx, output_rx) = mpsc::unbounded_channel();
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

        // Create a proposal with wrong view
        let msg = signing_message(99, &B256::repeat_byte(0xCC));
        let sig = sks[1].sign(&msg);

        let proposal = Proposal {
            view: 99,
            block_hash: B256::repeat_byte(0xCC),
            justify_qc: QuorumCertificate::genesis(),
            proposer: 1,
            signature: sig,
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

        let (mut engine, sks, vs, _rx) = make_engine(4, 0);
        let block_hash = B256::repeat_byte(0xD2);

        // Advance engine to view 5 by processing a future timeout
        let dummy = n42_primitives::consensus::TimeoutMessage {
            view: 5,
            high_qc: QuorumCertificate::genesis(),
            sender: 0,
            signature: sks[0].sign(b"advance"),
        };
        let _ = engine.process_event(ConsensusEvent::Message(
            ConsensusMessage::Timeout(dummy),
        ));
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
}
