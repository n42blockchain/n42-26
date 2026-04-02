use alloy_primitives::B256;
use n42_primitives::consensus::{ConsensusMessage, PrepareQC, Proposal, ViewNumber};

use super::quorum::{commit_signing_message, signing_message};
use super::round::Phase;
use super::state_machine::{ConsensusEngine, EngineOutput};
use crate::error::{ConsensusError, ConsensusResult};
use crate::validator::LeaderSelector;

const MAX_IMPORTED_BLOCKS: usize = 64;

impl ConsensusEngine {
    /// Called when this node (as leader) has a block ready to propose.
    pub(super) fn on_block_ready(&mut self, block_hash: B256, tx_root_hash: Option<B256>) -> ConsensusResult<()> {
        let view = self.round_state.current_view();

        if !self.is_current_leader() {
            tracing::debug!(target: "n42::cl::proposal", view, "not leader, ignoring block ready");
            return Ok(());
        }

        if self.round_state.phase() != Phase::WaitingForProposal {
            tracing::debug!(target: "n42::cl::proposal", view, phase = ?self.round_state.phase(), "not in proposal phase");
            return Ok(());
        }

        let justify_qc = self.round_state.locked_qc().clone();
        let piggybacked_qc = self.previous_prepare_qc.take();
        let chained = piggybacked_qc.is_some();

        // Include any pending validator changes so all nodes apply the same
        // changes at CommitQC time (consensus-safe commit-then-activate).
        let validator_changes = self.epoch_manager.pending_changes_for_proposal();

        // Signature covers changes_hash to prevent Byzantine relay from swapping changes.
        let prop_msg = super::quorum::proposal_signing_message(view, &block_hash, &validator_changes);
        let signature = self.secret_key.sign(&prop_msg);
        let vote_msg = signing_message(view, &block_hash);

        let proposal = Proposal {
            view,
            block_hash,
            justify_qc,
            proposer: self.my_index,
            signature,
            prepare_qc: piggybacked_qc,
            tx_root_hash,
            validator_changes,
        };

        if proposal.validator_changes.is_some() {
            tracing::info!(target: "n42::cl::proposal",
                view, %block_hash,
                changes = ?proposal.validator_changes,
                "proposing block with validator changes"
            );
        }

        tracing::debug!(target: "n42::cl::proposal",
            view, %block_hash,
            chained,
            "proposing block"
        );

        self.vote_collector = Some(crate::protocol::quorum::VoteCollector::new(
            view,
            block_hash,
            self.validator_set().len(),
        ));
        self.commit_collector = Some(crate::protocol::quorum::VoteCollector::new(
            view,
            block_hash,
            self.validator_set().len(),
        ));
        self.round_state.enter_voting();

        // GossipSub does not deliver messages back to the sender, so the leader
        // must add its own vote directly to the collector.
        // Reuse the vote_msg computed above (same view + block_hash).
        let leader_vote_sig = self.secret_key.sign(&vote_msg);
        if let Some(ref mut collector) = self.vote_collector {
            collector.add_verified_vote(self.my_index, leader_vote_sig)?;
        }

        self.view_timing.proposal_sent = Some(std::time::Instant::now());
        self.emit(EngineOutput::BroadcastMessage(ConsensusMessage::Proposal(
            proposal,
        )))?;

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

        let view_set = self.validator_set_for_view(view);
        let expected_leader = LeaderSelector::leader_for_view(view, view_set);
        if proposal.proposer != expected_leader {
            return Err(ConsensusError::InvalidProposer {
                view,
                expected: expected_leader,
                actual: proposal.proposer,
            });
        }

        let pk = view_set.get_public_key(proposal.proposer)?;
        // Verify proposal signature covers (view, block_hash, changes_hash).
        let prop_msg = super::quorum::proposal_signing_message(
            view,
            &proposal.block_hash,
            &proposal.validator_changes,
        );
        pk.verify(&prop_msg, &proposal.signature)
            .map_err(|_| ConsensusError::InvalidSignature {
                view,
                validator_index: proposal.proposer,
            })?;

        // Verify the justify_qc's aggregate BLS signature to prevent a Byzantine leader
        // from injecting a forged QC that manipulates honest nodes' locked_qc.
        // Genesis QC (view 0) is exempt — it has no real aggregate signatures.
        // Uses verify_qc_any_domain because justify_qc may be either a prepare QC or commit QC.
        if proposal.justify_qc.view > 0 {
            super::quorum::verify_qc_any_domain(
                &proposal.justify_qc,
                self.validator_set_for_view(proposal.justify_qc.view),
            )
            .map_err(|e| {
                tracing::warn!(target: "n42::cl::proposal",
                    view, proposer = proposal.proposer, qc_view = proposal.justify_qc.view,
                    "rejecting proposal with invalid justify_qc: {e}"
                );
                e
            })?;
        }

        if !self.round_state.is_safe_to_vote(&proposal.justify_qc) {
            return Err(ConsensusError::SafetyViolation {
                qc_view: proposal.justify_qc.view,
                locked_view: self.round_state.locked_qc().view,
            });
        }

        self.round_state.update_locked_qc(&proposal.justify_qc);

        // Chained mode: process piggybacked PrepareQC if present.
        if let Some(ref piggybacked_qc) = proposal.prepare_qc {
            match super::quorum::verify_qc(
                piggybacked_qc,
                self.validator_set_for_view(piggybacked_qc.view),
            ) {
                Ok(()) => {
                    tracing::debug!(target: "n42::cl::proposal",
                        view,
                        qc_view = piggybacked_qc.view,
                        "accepted piggybacked PrepareQC from proposal"
                    );
                    self.round_state.update_locked_qc(piggybacked_qc);
                }
                Err(e) => {
                    // Invalid piggybacked QC is not fatal — the proposal itself is valid.
                    tracing::warn!(target: "n42::cl::proposal", view, error = %e, "rejected invalid piggybacked PrepareQC, ignoring");
                }
            }
        }

        // Apply validator changes from the leader's Proposal.
        // `Some(changes)` → leader has changes, replace local pending.
        // `None` → leader has no changes. Do NOT clear local pending —
        // this node may have pending changes from RPC that haven't been
        // included in a Proposal yet (the node wasn't leader). They will
        // be included when this node next becomes leader.
        const MAX_CHANGES_PER_PROPOSAL: usize = 4;
        if let Some(ref changes) = proposal.validator_changes {
            if changes.len() > MAX_CHANGES_PER_PROPOSAL {
                return Err(ConsensusError::TooManyValidatorChanges {
                    count: changes.len(),
                    max: MAX_CHANGES_PER_PROPOSAL,
                });
            }
            self.epoch_manager.replace_pending_from_proposal(changes);
        }

        // Store pending tx_root_hash for future DA verification if present.
        // Bounded to 64 entries to prevent OOM from unfinalized proposals.
        if let Some(tx_root) = proposal.tx_root_hash {
            if self.pending_tx_roots.len() >= 64
                && let Some(&oldest) = self.pending_tx_roots.keys().next()
            {
                self.pending_tx_roots.remove(&oldest);
            }
            self.pending_tx_roots.insert(proposal.block_hash, tx_root);
        }

        self.round_state.enter_voting();
        self.view_timing.proposal_received = Some(std::time::Instant::now());

        // Trigger eager import in background (needed for finalization).
        self.emit(EngineOutput::ExecuteBlock(proposal.block_hash))?;

        // Optimistic Voting: vote immediately after Proposal validation.
        //
        // R1 vote signs (view, block_hash) — it does NOT commit to block validity.
        // The Proposal has already been fully verified: leader identity, BLS signature,
        // justify_qc aggregate signature, and HotStuff-2 safety rules.
        //
        // Block validity (EVM execution) is verified during finalization:
        //   - new_payload validates the block in reth's engine tree
        //   - FCU commits it to the canonical chain
        //   - If invalid: FCU fails → view change recovers
        //
        // This eliminates the ~300-500ms vote_delay caused by waiting for BlockData
        // arrival and new_payload completion before voting.
        tracing::info!(target: "n42::cl::proposal", view, block_hash = %proposal.block_hash, "optimistic vote: voting immediately after proposal validation");
        self.send_vote(view, proposal.block_hash)?;

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

        if pqc.qc.view != pqc.view {
            return Err(ConsensusError::ViewMismatch {
                current: pqc.view,
                received: pqc.qc.view,
            });
        }

        if pqc.qc.block_hash != pqc.block_hash {
            return Err(ConsensusError::BlockHashMismatch {
                expected: pqc.block_hash,
                got: pqc.qc.block_hash,
            });
        }

        super::quorum::verify_qc(&pqc.qc, self.validator_set_for_view(pqc.qc.view))?;
        self.round_state.update_locked_qc(&pqc.qc);
        self.round_state.enter_pre_commit();

        tracing::debug!(target: "n42::cl::proposal", view, block_hash = %pqc.block_hash, "received valid PrepareQC, sending commit vote");

        let commit_msg = commit_signing_message(view, &pqc.block_hash);
        let commit_sig = self.secret_key.sign(&commit_msg);
        let leader = self.leader_index_for_view(view);

        let commit_vote = n42_primitives::consensus::CommitVote {
            view,
            block_hash: pqc.block_hash,
            voter: self.my_index,
            signature: commit_sig,
        };

        self.view_timing.commit_vote_sent = Some(std::time::Instant::now());
        self.emit(EngineOutput::SendToValidator(
            leader,
            ConsensusMessage::CommitVote(commit_vote),
        ))
    }

    /// Sends a Round 1 vote for the given view and block hash.
    ///
    /// Checks `last_voted_view` to prevent double-voting after crash recovery
    /// (fundamental BFT safety invariant).
    pub(super) fn send_vote(&mut self, view: ViewNumber, block_hash: B256) -> ConsensusResult<()> {
        if !self.round_state.may_vote_in(view) {
            tracing::warn!(
                target: "n42::cl::proposal",
                view,
                last_voted = self.round_state.last_voted_view(),
                "suppressed duplicate vote (already voted in this view)"
            );
            return Ok(());
        }
        // Record BEFORE signing/sending so a crash between record and send is safe
        // (we err on the side of not voting rather than double-voting).
        self.round_state.record_vote(view);

        let leader = self.leader_index_for_view(view);
        let vote_msg = signing_message(view, &block_hash);
        let vote_sig = self.secret_key.sign(&vote_msg);

        let vote = n42_primitives::consensus::Vote {
            view,
            block_hash,
            voter: self.my_index,
            signature: vote_sig,
        };

        tracing::info!(target: "n42::cl::proposal", view, %block_hash, voter = self.my_index, target_leader = leader, "sending vote to leader");
        self.view_timing.vote_sent = Some(std::time::Instant::now());
        self.emit(EngineOutput::SendToValidator(
            leader,
            ConsensusMessage::Vote(vote),
        ))
    }

    /// Handles the BlockImported event from the orchestrator.
    ///
    /// With Optimistic Voting, R1 votes are sent immediately upon Proposal validation
    /// (no waiting for BlockData). This handler only tracks imported blocks for
    /// diagnostics and eager import coordination.
    pub(super) fn on_block_imported(&mut self, block_hash: B256) -> ConsensusResult<()> {
        if self.imported_blocks.len() < MAX_IMPORTED_BLOCKS {
            self.imported_blocks.insert(block_hash);
        } else {
            tracing::warn!(
                target: "n42::cl::proposal",
                view = self.round_state.current_view(),
                limit = MAX_IMPORTED_BLOCKS,
                %block_hash,
                "imported_blocks at capacity, discarding entry (diagnostic tracking only)"
            );
        }
        Ok(())
    }
}
