use n42_primitives::consensus::{CommitVote, ConsensusMessage, PrepareQC, Vote};

use crate::error::{ConsensusError, ConsensusResult};
use super::quorum::{commit_signing_message, signing_message};
use super::state_machine::{ConsensusEngine, EngineOutput};

impl ConsensusEngine {
    /// Processes a Round 1 (Prepare) vote from a validator.
    pub(super) fn process_vote(&mut self, vote: Vote) -> ConsensusResult<()> {
        let view = self.round_state.current_view();

        if vote.view != view {
            return Err(ConsensusError::ViewMismatch {
                current: view,
                received: vote.view,
            });
        }

        if !self.is_current_leader() {
            return Ok(());
        }

        // Equivocation detection: a validator voting for two different blocks in the same view
        // is Byzantine behavior.
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

        // Check block hash before verification (read-only borrow of collector).
        let expected_hash = match self.vote_collector.as_ref() {
            Some(c) => c.block_hash(),
            None => return Ok(()),
        };

        if vote.block_hash != expected_hash {
            tracing::debug!(view, voter = vote.voter, "ignoring vote for different block");
            return Ok(());
        }

        // Verify BLS signature before adding to collector.
        let pk = self.validator_set().get_public_key(vote.voter)?;
        let msg = signing_message(view, &vote.block_hash);
        pk.verify(&msg, &vote.signature).map_err(|_| ConsensusError::InvalidSignature {
            view,
            validator_index: vote.voter,
        })?;

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
    ///
    /// When quorum is reached, broadcasts the QC, transitions to PreCommit,
    /// and self-votes for CommitVote (Round 2).
    pub(super) fn try_form_prepare_qc(&mut self) -> ConsensusResult<()> {
        let view = self.round_state.current_view();

        let has_quorum = self
            .vote_collector
            .as_ref()
            .is_some_and(|c| c.has_quorum(self.validator_set().quorum_size()));

        if !has_quorum || self.prepare_qc.is_some() {
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

        let block_hash = qc.block_hash;
        let prepare_qc_msg = PrepareQC { view, block_hash, qc };
        self.emit(EngineOutput::BroadcastMessage(ConsensusMessage::PrepareQC(prepare_qc_msg)));

        // Leader self-vote for CommitVote (Round 2).
        let commit_msg = commit_signing_message(view, &block_hash);
        let commit_sig = self.secret_key.sign(&commit_msg);
        if let Some(ref mut collector) = self.commit_collector {
            let _ = collector.add_vote(self.my_index, commit_sig);
        }

        self.try_form_commit_qc()
    }

    /// Processes a Round 2 (Commit) vote from a validator.
    pub(super) fn process_commit_vote(&mut self, cv: CommitVote) -> ConsensusResult<()> {
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

        let expected_hash = match self.commit_collector.as_ref() {
            Some(c) => c.block_hash(),
            None => return Ok(()),
        };

        if cv.block_hash != expected_hash {
            tracing::debug!(view, voter = cv.voter, "ignoring commit vote for different block");
            return Ok(());
        }

        let pk = self.validator_set().get_public_key(cv.voter)?;
        let msg = commit_signing_message(view, &cv.block_hash);
        pk.verify(&msg, &cv.signature).map_err(|_| ConsensusError::InvalidSignature {
            view,
            validator_index: cv.voter,
        })?;

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
    ///
    /// When quorum is reached, commits the block, broadcasts Decide,
    /// and advances to the next view.
    pub(super) fn try_form_commit_qc(&mut self) -> ConsensusResult<()> {
        let view = self.round_state.current_view();

        let has_quorum = self
            .commit_collector
            .as_ref()
            .is_some_and(|c| c.has_quorum(self.validator_set().quorum_size()));

        if !has_quorum {
            return Ok(());
        }

        let collector = match self.commit_collector.as_ref() {
            Some(c) => c,
            None => return Ok(()),
        };
        let block_hash = collector.block_hash();
        let commit_msg = commit_signing_message(view, &block_hash);
        let commit_qc = collector.build_qc_with_message(self.validator_set(), &commit_msg)?;

        tracing::info!(view, %block_hash, "block committed!");

        self.round_state.commit(commit_qc.clone());

        let decide = n42_primitives::consensus::Decide {
            view,
            block_hash,
            commit_qc: commit_qc.clone(),
        };
        self.emit(EngineOutput::BroadcastMessage(ConsensusMessage::Decide(decide)));
        self.emit(EngineOutput::BlockCommitted { view, block_hash, commit_qc });

        let next_view = view.saturating_add(1);
        self.advance_to_view(next_view);

        Ok(())
    }
}
