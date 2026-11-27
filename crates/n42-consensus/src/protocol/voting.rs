use alloy_primitives::B256;
use n42_primitives::consensus::{CommitVote, ConsensusMessage, PrepareQC, Vote};
use std::collections::HashMap;

use super::state_machine::{
    ConsensusEngine, EngineOutput, EquivocationKind, EquivocationProof, SignedMessage,
};
use crate::error::{ConsensusError, ConsensusResult};

/// A conflict found by [`check_equivocation`]: both block hashes and both
/// signed messages, first-seen first.
pub(super) type Conflict = (B256, B256, SignedMessage, SignedMessage);

/// Checks an equivocation tracker for a validator voting for two different
/// blocks. The first message a validator signs in the view is kept as
/// evidence; a later one for a different block returns the conflict.
pub(super) fn check_equivocation(
    tracker: &mut HashMap<u32, (B256, SignedMessage)>,
    voter: u32,
    block_hash: B256,
    signed: SignedMessage,
) -> Option<Conflict> {
    match tracker.get(&voter) {
        Some((prev, first)) if *prev != block_hash => {
            Some((*prev, block_hash, first.clone(), signed))
        }
        Some(_) => None,
        None => {
            tracker.insert(voter, (block_hash, signed));
            None
        }
    }
}

impl ConsensusEngine {
    /// Emits [`EngineOutput::EquivocationDetected`] with the signed pair and
    /// the validator's key from the view's set.
    pub(super) fn emit_equivocation(
        &mut self,
        kind: EquivocationKind,
        view: u64,
        validator: u32,
        (hash1, hash2, first, second): Conflict,
    ) -> ConsensusResult<()> {
        let public_key = self
            .validator_set_for_view(view)
            .get_public_key(validator)?
            .to_bytes();
        self.emit(EngineOutput::EquivocationDetected {
            view,
            validator,
            hash1,
            hash2,
            proof: Box::new(EquivocationProof {
                kind,
                public_key,
                first,
                second,
            }),
        })
    }

    /// Processes a Round 1 (Prepare) vote from a validator.
    pub(super) fn process_vote(&mut self, vote: Vote) -> ConsensusResult<()> {
        self.process_vote_inner(vote, false)
    }

    /// Processes a vote whose exact message was authenticated by the engine's
    /// randomized batch verifier.
    pub(super) fn process_verified_vote(&mut self, vote: Vote) -> ConsensusResult<()> {
        self.process_vote_inner(vote, true)
    }

    fn process_vote_inner(&mut self, vote: Vote, signature_verified: bool) -> ConsensusResult<()> {
        let view = self.round_state.current_view();

        if vote.view != view {
            return Err(ConsensusError::ViewMismatch {
                current: view,
                received: vote.view,
            });
        }

        // Votes are addressed to the leader (SendToValidator). Followers that receive
        // multi-path duplicates (or anything a Byzantine peer relays directly) drop
        // here, mirroring the R2 design. Aside from saving work, this prevents an
        // unauthenticated peer from poisoning equivocation_tracker with forged
        // (voter, hash) pairs and starving the real voter's later vote.
        if !self.is_current_leader() {
            return Ok(());
        }

        let msg = self.signing_profile.vote_message(view, vote.block_hash);
        if !signature_verified {
            let view_set = self.validator_set_for_view(view);
            let pk = view_set.get_public_key(vote.voter)?;
            if !self
                .signing_profile
                .verify_single(pk, &msg, &vote.signature)
            {
                return Err(ConsensusError::InvalidSignature {
                    view,
                    validator_index: vote.voter,
                });
            }
        }

        // Equivocation check is gated on a verified signature so that an unauthenticated
        // peer cannot poison the tracker against a real voter.
        let signed = SignedMessage {
            signed: msg,
            signature: vote.signature.to_bytes(),
        };
        if let Some(conflict) = check_equivocation(
            &mut self.equivocation_tracker,
            vote.voter,
            vote.block_hash,
            signed,
        ) {
            tracing::warn!(target: "n42::cl::voting", view, validator = vote.voter,
                hash1 = %conflict.0, hash2 = %conflict.1, "vote equivocation detected");
            self.emit_equivocation(EquivocationKind::Vote, view, vote.voter, conflict)?;
            return Ok(());
        }

        // Compare with the collector only after signature verification and
        // equivocation tracking. Otherwise a Byzantine validator's vote for a
        // sibling hash is discarded before it can become evidence, making
        // detection depend on which vote arrived first.
        let expected_hash = match self.vote_collector.as_ref() {
            Some(c) => c.block_hash(),
            None => return Ok(()),
        };
        if vote.block_hash != expected_hash {
            tracing::debug!(target: "n42::cl::voting", view, voter = vote.voter,
                "ignoring verified vote for non-proposed block after equivocation tracking");
            return Ok(());
        }

        let collector = match self.vote_collector.as_mut() {
            Some(c) => c,
            None => {
                tracing::warn!(target: "n42::cl::voting", view, "vote_collector not initialized, ignoring vote");
                return Ok(());
            }
        };
        match collector.add_verified_vote(vote.voter, vote.signature) {
            Ok(()) => {}
            Err(ConsensusError::DuplicateVote { .. }) => {
                tracing::debug!(
                    target: "n42::cl::voting",
                    view,
                    voter = vote.voter,
                    "ignoring duplicate vote (dual-path delivery)"
                );
                return Ok(());
            }
            Err(e) => return Err(e),
        }

        tracing::debug!(target: "n42::cl::voting",
            view,
            voter = vote.voter,
            count = collector.vote_count(),
            "received vote"
        );

        self.try_form_prepare_qc()
    }

    /// Attempts to form a PrepareQC from collected Round 1 votes.
    ///
    /// When quorum is reached, broadcasts the QC, transitions to PreCommit,
    /// and self-votes for CommitVote (Round 2).
    pub(super) fn try_form_prepare_qc(&mut self) -> ConsensusResult<()> {
        let view = self.round_state.current_view();
        let view_set = self.validator_set_for_view(view);

        let has_quorum = self
            .vote_collector
            .as_ref()
            .is_some_and(|c| c.has_quorum(view_set.quorum_size()));

        if !has_quorum || self.prepare_qc.is_some() {
            return Ok(());
        }

        let collector = match self.vote_collector.as_ref() {
            Some(c) => c,
            None => {
                tracing::warn!(target: "n42::cl::voting", view, "vote_collector not initialized in try_form_prepare_qc");
                return Ok(());
            }
        };
        let vote_message = self
            .signing_profile
            .vote_message(view, collector.block_hash());
        let qc = collector.build_qc_with_profile_message(
            view_set,
            &vote_message,
            self.signing_profile,
        )?;

        tracing::debug!(target: "n42::cl::voting", view, signers = qc.signer_count(), "QC formed, entering pre-commit");

        self.view_timing.prepare_qc_formed = Some(std::time::Instant::now());
        self.view_timing.prepare_vote_count = collector.vote_count() as u32;
        self.prepare_qc = Some(qc.clone());
        self.round_state.enter_pre_commit();
        self.round_state.update_locked_qc(&qc);

        let block_hash = qc.block_hash;
        let prepare_qc_msg = PrepareQC {
            view,
            block_hash,
            qc,
        };
        self.emit(EngineOutput::BroadcastMessage(ConsensusMessage::PrepareQC(
            prepare_qc_msg,
        )))?;

        self.cast_leader_commit_self_vote(view, block_hash)
    }

    /// The leader's Round-2 self-vote for `block_hash` in `view`, after its
    /// PrepareQC formed. Bound to the same changes_hash `on_block_ready`
    /// cached so it signs the same domain as every follower's commit vote.
    /// AM35: gated on the block's durability like every other R2 vote; held
    /// (and retried on `BlockDataStaged`) while the block data is not here.
    pub(super) fn cast_leader_commit_self_vote(
        &mut self,
        view: u64,
        block_hash: B256,
    ) -> ConsensusResult<()> {
        if view != self.round_state.current_view() || self.prepare_qc.is_none() {
            return Ok(());
        }
        if !self.round_state.may_commit_vote_in(view) {
            tracing::warn!(target: "n42::cl::voting", view,
                last_commit_voted = self.round_state.last_commit_voted_view(),
                "leader already cast a commit vote in this view; suppressing R2 self-vote");
            return Ok(());
        }
        if let Some(missing) = self.persist_before_commit_vote(view, block_hash, "leader")? {
            tracing::info!(target: "n42::cl::voting", view, %block_hash, %missing, "holding the leader commit self-vote until the block can be made durable");
            self.held_self_commit_vote = Some((view, block_hash));
            return Ok(());
        }
        let changes_hash = self.cached_changes_hash(&block_hash);
        let commit_msg = self
            .signing_profile
            .commit_message(view, block_hash, changes_hash);
        let commit_sig = self.signing_profile.sign(&self.secret_key, &commit_msg);
        if let Some(ref mut collector) = self.commit_collector {
            collector.add_verified_vote(self.my_index, commit_sig)?;
        }

        self.try_form_commit_qc()
    }

    /// Processes a Round 2 (Commit) vote from a validator.
    pub(super) fn process_commit_vote(&mut self, cv: CommitVote) -> ConsensusResult<()> {
        self.process_commit_vote_inner(cv, false)
    }

    /// Processes a commit vote whose exact message was authenticated by the
    /// engine's randomized batch verifier.
    pub(super) fn process_verified_commit_vote(&mut self, cv: CommitVote) -> ConsensusResult<()> {
        self.process_commit_vote_inner(cv, true)
    }

    fn process_commit_vote_inner(
        &mut self,
        cv: CommitVote,
        signature_verified: bool,
    ) -> ConsensusResult<()> {
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

        let changes_hash = self.cached_changes_hash(&cv.block_hash);
        let msg = self
            .signing_profile
            .commit_message(view, cv.block_hash, changes_hash);
        if !signature_verified {
            let view_set = self.validator_set_for_view(view);
            let pk = view_set.get_public_key(cv.voter)?;
            if !self.signing_profile.verify_single(pk, &msg, &cv.signature) {
                return Err(ConsensusError::InvalidSignature {
                    view,
                    validator_index: cv.voter,
                });
            }
        }

        // R2 equivocation check — gated on verified signature, leader-only by design
        // (CommitVotes are sent directly to the leader, not broadcast).
        let signed = SignedMessage {
            signed: msg,
            signature: cv.signature.to_bytes(),
        };
        if let Some(conflict) = check_equivocation(
            &mut self.commit_equivocation_tracker,
            cv.voter,
            cv.block_hash,
            signed,
        ) {
            tracing::warn!(target: "n42::cl::voting", view, validator = cv.voter,
                hash1 = %conflict.0, hash2 = %conflict.1, "commit-vote equivocation detected");
            self.emit_equivocation(EquivocationKind::CommitVote, view, cv.voter, conflict)?;
            return Ok(());
        }

        let expected_hash = match self.commit_collector.as_ref() {
            Some(c) => c.block_hash(),
            None => return Ok(()),
        };
        if cv.block_hash != expected_hash {
            tracing::debug!(target: "n42::cl::voting", view, voter = cv.voter,
                "ignoring verified commit vote for non-proposed block after equivocation tracking");
            return Ok(());
        }

        let collector = match self.commit_collector.as_mut() {
            Some(c) => c,
            None => {
                tracing::warn!(target: "n42::cl::voting", view, "commit_collector not initialized, ignoring commit vote");
                return Ok(());
            }
        };
        match collector.add_verified_vote(cv.voter, cv.signature) {
            Ok(()) => {}
            Err(ConsensusError::DuplicateVote { .. }) => {
                tracing::debug!(
                    target: "n42::cl::voting",
                    view,
                    voter = cv.voter,
                    "ignoring duplicate commit vote (dual-path delivery)"
                );
                return Ok(());
            }
            Err(e) => return Err(e),
        }

        tracing::debug!(target: "n42::cl::voting",
            view,
            voter = cv.voter,
            count = collector.vote_count(),
            "received commit vote"
        );

        self.try_form_commit_qc()
    }

    /// Attempts to form a Commit QC from collected Round 2 votes.
    ///
    /// When quorum is reached, commits the block, broadcasts Decide,
    /// and advances to the next view.
    pub(super) fn try_form_commit_qc(&mut self) -> ConsensusResult<()> {
        let view = self.round_state.current_view();
        let view_set = self.validator_set_for_view(view);

        let has_quorum = self
            .commit_collector
            .as_ref()
            .is_some_and(|c| c.has_quorum(view_set.quorum_size()));

        if !has_quorum {
            return Ok(());
        }
        let collector = match self.commit_collector.as_ref() {
            Some(c) => c,
            None => return Ok(()),
        };
        let block_hash = collector.block_hash();
        let changes_hash = self.cached_changes_hash(&block_hash);
        let commit_msg = self
            .signing_profile
            .commit_message(view, block_hash, changes_hash);
        let commit_qc =
            collector.build_qc_with_profile_message(view_set, &commit_msg, self.signing_profile)?;

        self.view_timing.commit_qc_formed = Some(std::time::Instant::now());
        self.view_timing.commit_vote_count = collector.vote_count() as u32;

        tracing::info!(target: "n42::cl::voting", view, %block_hash,
            consensus_timing = %self.view_timing.summary(),
            "block committed!");

        self.round_state.commit(commit_qc.clone());

        // The Decide must carry the hash the commit votes signed. Changes queued
        // after the proposal stay pending for a later block; staging them here
        // would commit a change no block carried and break every follower's
        // CommitQC check.
        let changes_hash = self.cached_changes_hash(&block_hash);

        // Capture changes BEFORE commit clears pending (needed for Decide + BlockCommitted).
        // Commit-then-Activate: if validator changes were proposed, stage them now.
        let committed_changes = if changes_hash != alloy_primitives::B256::ZERO
            && self.epoch_manager.pending_changes_hash() == changes_hash
        {
            let changes = self.epoch_manager.pending_changes_for_proposal();
            self.epoch_manager.commit_pending_changes()?;
            changes
        } else {
            None
        };

        let decide = n42_primitives::consensus::Decide {
            view,
            block_hash,
            commit_qc: commit_qc.clone(),
            validator_changes_hash: changes_hash,
        };
        self.emit(EngineOutput::BroadcastMessage(ConsensusMessage::Decide(
            decide,
        )))?;
        // Clean up pending tx_root_hash for the committed block.
        self.pending_tx_roots.remove(&block_hash);

        self.emit(EngineOutput::BlockCommitted {
            view,
            block_hash,
            commit_qc,
            validator_changes: committed_changes,
        })?;

        let next_view = view.saturating_add(1);
        self.advance_to_view(next_view)?;

        // Notify the orchestrator that the view has advanced so the next leader
        // can start building immediately instead of waiting for a pacemaker timeout.
        let actual_view = self.round_state.current_view();
        self.emit(EngineOutput::ViewChanged {
            new_view: actual_view,
        })
    }
}
