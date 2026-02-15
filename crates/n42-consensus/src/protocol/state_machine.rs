use alloy_primitives::B256;
use n42_primitives::{
    BlsSecretKey,
    consensus::{
        CommitVote, ConsensusMessage, NewView, Proposal, QuorumCertificate,
        TimeoutMessage, ViewNumber, Vote,
    },
};
use tokio::sync::mpsc;

use crate::error::{ConsensusError, ConsensusResult};
use crate::validator::{LeaderSelector, ValidatorSet};
use super::pacemaker::Pacemaker;
use super::quorum::{
    VoteCollector, TimeoutCollector,
    signing_message, timeout_signing_message,
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
        }
    }

    /// Handles a view timeout.
    pub fn on_timeout(&mut self) -> ConsensusResult<()> {
        let view = self.round_state.current_view();
        tracing::warn!(view, "view timed out");

        self.round_state.timeout();

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
            ConsensusMessage::Timeout(t) => self.process_timeout(t),
            ConsensusMessage::NewView(nv) => self.process_new_view(nv),
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

        self.emit(EngineOutput::BroadcastMessage(
            ConsensusMessage::Proposal(proposal),
        ));

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

        tracing::info!(view, block_hash = %proposal.block_hash, "received valid proposal, voting");

        // Request block execution
        self.emit(EngineOutput::ExecuteBlock(proposal.block_hash));

        // Send Round 1 vote to leader
        let vote_msg = signing_message(view, &proposal.block_hash);
        let vote_sig = self.secret_key.sign(&vote_msg);

        let vote = Vote {
            view,
            block_hash: proposal.block_hash,
            voter: self.my_index,
            signature: vote_sig,
        };

        self.emit(EngineOutput::SendToValidator(
            expected_leader,
            ConsensusMessage::Vote(vote),
        ));

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

        collector.add_vote(vote.voter, vote.signature)?;

        tracing::debug!(
            view,
            voter = vote.voter,
            count = collector.vote_count(),
            "received vote"
        );

        // Check if we have a quorum
        if collector.has_quorum(self.validator_set.quorum_size()) {
            let qc = collector.build_qc(&self.validator_set)?;
            tracing::info!(view, signers = qc.signer_count(), "QC formed, entering pre-commit");

            self.prepare_qc = Some(qc.clone());
            self.round_state.enter_pre_commit();
            self.round_state.update_locked_qc(&qc);

            // Broadcast QC to all validators (they'll send commit votes)
            // We piggyback the QC as a Proposal-like message;
            // for simplicity, we broadcast it as a CommitVote trigger
            // by having validators see the QC and respond with commit votes.
            // In practice, the leader sends the QC, and validators respond.

            // For now, we assume validators send commit votes upon seeing the QC.
            // The QC is embedded in the protocol state; we don't have a separate
            // "QC broadcast" message. Instead, we transition and wait for commit votes.
        }

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

        collector.add_vote(cv.voter, cv.signature)?;

        tracing::debug!(
            view,
            voter = cv.voter,
            count = collector.vote_count(),
            "received commit vote"
        );

        // Check if we have a quorum for commit
        if collector.has_quorum(self.validator_set.quorum_size()) {
            let commit_qc = collector.build_qc(&self.validator_set)?;
            let block_hash = commit_qc.block_hash;

            tracing::info!(view, %block_hash, "block committed!");

            self.round_state.commit(commit_qc.clone());

            self.emit(EngineOutput::BlockCommitted {
                view,
                block_hash,
                commit_qc,
            });

            // Advance to next view
            let next_view = view + 1;
            self.advance_to_view(next_view);
        }

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
        let next_view = view + 1;
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
