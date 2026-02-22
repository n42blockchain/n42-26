use alloy_primitives::B256;
use n42_primitives::consensus::{ConsensusMessage, PrepareQC, Proposal, ViewNumber};

use crate::error::{ConsensusError, ConsensusResult};
use crate::validator::LeaderSelector;
use super::quorum::{commit_signing_message, signing_message};
use super::round::Phase;
use super::state_machine::{ConsensusEngine, EngineOutput, PendingProposal};

impl ConsensusEngine {
    /// Called when this node (as leader) has a block ready to propose.
    pub(super) fn on_block_ready(&mut self, block_hash: B256) -> ConsensusResult<()> {
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

        self.vote_collector = Some(crate::protocol::quorum::VoteCollector::new(
            view, block_hash, self.validator_set().len(),
        ));
        self.commit_collector = Some(crate::protocol::quorum::VoteCollector::new(
            view, block_hash, self.validator_set().len(),
        ));
        self.round_state.enter_voting();

        // GossipSub does not deliver messages back to the sender, so the leader
        // must add its own vote directly to the collector.
        let leader_vote_msg = signing_message(view, &block_hash);
        let leader_vote_sig = self.secret_key.sign(&leader_vote_msg);
        if let Some(ref mut collector) = self.vote_collector {
            let _ = collector.add_vote(self.my_index, leader_vote_sig);
        }

        self.emit(EngineOutput::BroadcastMessage(ConsensusMessage::Proposal(proposal)));

        // Check if quorum already reached (single-validator scenario).
        self.try_form_prepare_qc()
    }

    /// Processes a proposal from the leader.
    pub(super) fn process_proposal(&mut self, proposal: Proposal) -> ConsensusResult<()> {
        let view = self.round_state.current_view();

        if proposal.view != view {
            return Err(ConsensusError::ViewMismatch {
                current: view,
                received: proposal.view,
            });
        }

        let expected_leader = LeaderSelector::leader_for_view(view, self.validator_set());
        if proposal.proposer != expected_leader {
            return Err(ConsensusError::InvalidProposer {
                view,
                expected: expected_leader,
                actual: proposal.proposer,
            });
        }

        let pk = self.validator_set().get_public_key(proposal.proposer)?;
        let msg = signing_message(view, &proposal.block_hash);
        pk.verify(&msg, &proposal.signature).map_err(|_| ConsensusError::InvalidSignature {
            view,
            validator_index: proposal.proposer,
        })?;

        if !self.round_state.is_safe_to_vote(&proposal.justify_qc) {
            return Err(ConsensusError::SafetyViolation {
                qc_view: proposal.justify_qc.view,
                locked_view: self.round_state.locked_qc().view,
            });
        }

        self.round_state.update_locked_qc(&proposal.justify_qc);

        // Chained mode: process piggybacked PrepareQC if present.
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
                    // Invalid piggybacked QC is not fatal — the proposal itself is valid.
                    tracing::warn!(view, error = %e, "rejected invalid piggybacked PrepareQC, ignoring");
                }
            }
        }

        self.round_state.enter_voting();
        self.emit(EngineOutput::ExecuteBlock(proposal.block_hash));

        // If the block was already imported (BlockData arrived before Proposal), vote immediately.
        if self.imported_blocks.remove(&proposal.block_hash) {
            tracing::debug!(view, block_hash = %proposal.block_hash, "block already imported, voting immediately");
            self.send_vote(view, proposal.block_hash)?;
        } else {
            tracing::debug!(view, block_hash = %proposal.block_hash, "deferring vote until block data imported");
            self.pending_proposal = Some(PendingProposal {
                view: proposal.view,
                block_hash: proposal.block_hash,
            });
        }

        Ok(())
    }

    /// Processes a PrepareQC from the leader: validates the QC and sends a CommitVote.
    pub(super) fn process_prepare_qc(&mut self, pqc: PrepareQC) -> ConsensusResult<()> {
        let view = self.round_state.current_view();

        if pqc.view != view {
            return Err(ConsensusError::ViewMismatch {
                current: view,
                received: pqc.view,
            });
        }

        super::quorum::verify_qc(&pqc.qc, self.validator_set())?;
        self.round_state.update_locked_qc(&pqc.qc);
        self.round_state.enter_pre_commit();

        tracing::debug!(view, block_hash = %pqc.block_hash, "received valid PrepareQC, sending commit vote");

        let commit_msg = commit_signing_message(view, &pqc.block_hash);
        let commit_sig = self.secret_key.sign(&commit_msg);
        let leader = LeaderSelector::leader_for_view(view, self.validator_set());

        let commit_vote = n42_primitives::consensus::CommitVote {
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

    /// Sends a Round 1 vote for the given view and block hash.
    pub(super) fn send_vote(&mut self, view: ViewNumber, block_hash: B256) -> ConsensusResult<()> {
        let leader = LeaderSelector::leader_for_view(view, self.validator_set());
        let vote_msg = signing_message(view, &block_hash);
        let vote_sig = self.secret_key.sign(&vote_msg);

        let vote = n42_primitives::consensus::Vote {
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
    /// Supports two arrival orders (reference: Aptos Baby Raptr):
    /// 1. Proposal first, BlockData later — pending_proposal is set — vote now.
    /// 2. BlockData first, Proposal later — cache in imported_blocks — vote when Proposal arrives.
    pub(super) fn on_block_imported(&mut self, block_hash: B256) -> ConsensusResult<()> {
        if let Some(pending) = self.pending_proposal.take() {
            if pending.block_hash == block_hash {
                tracing::debug!(view = pending.view, %block_hash, "block imported, sending deferred vote");
                self.send_vote(pending.view, pending.block_hash)?;
            } else {
                if self.imported_blocks.len() < 32 {
                    self.imported_blocks.insert(block_hash);
                }
                self.pending_proposal = Some(pending);
            }
        } else if self.imported_blocks.len() < 32 {
            self.imported_blocks.insert(block_hash);
        }
        Ok(())
    }
}
