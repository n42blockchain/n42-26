use n42_primitives::consensus::Decide;

use crate::error::{ConsensusError, ConsensusResult};
use super::state_machine::{ConsensusEngine, EngineOutput};

impl ConsensusEngine {
    /// Processes a Decide message from the leader.
    ///
    /// Verifies the CommitQC and advances the follower to the next view.
    /// If the Decide's view is more than 3 views ahead, emits `SyncRequired`
    /// so the orchestrator can initiate block sync.
    pub(super) fn process_decide(&mut self, decide: Decide) -> ConsensusResult<()> {
        let current_view = self.round_state.current_view();

        if decide.view < current_view {
            tracing::debug!(decide_view = decide.view, current_view, "ignoring stale Decide");
            return Ok(());
        }

        let quorum_size = self.validator_set().quorum_size();
        if decide.commit_qc.signer_count() < quorum_size {
            return Err(ConsensusError::InsufficientVotes {
                view: decide.view,
                have: decide.commit_qc.signer_count(),
                need: quorum_size,
            });
        }

        if decide.commit_qc.view != decide.view {
            return Err(ConsensusError::ViewMismatch {
                current: decide.view,
                received: decide.commit_qc.view,
            });
        }

        if decide.commit_qc.block_hash != decide.block_hash {
            return Err(ConsensusError::BlockHashMismatch {
                expected: decide.block_hash,
                got: decide.commit_qc.block_hash,
            });
        }

        // Verify CommitQC aggregate BLS signature. Without this, a Byzantine leader
        // could forge a Decide with a valid signer bitmap but invalid signature.
        super::quorum::verify_commit_qc(&decide.commit_qc, self.validator_set())?;

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

        tracing::info!(view = decide.view, %decide.block_hash, "received Decide, committing block");

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
}
