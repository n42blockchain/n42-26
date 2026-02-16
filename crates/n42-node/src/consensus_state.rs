use arc_swap::ArcSwap;
use n42_consensus::ValidatorSet;
use n42_primitives::QuorumCertificate;
use std::sync::Arc;

/// Shared consensus state between the Orchestrator and PayloadBuilder.
///
/// The Orchestrator writes to this state (low frequency, on block commit),
/// while the PayloadBuilder reads from it (on every payload build attempt).
/// `ArcSwap` provides lock-free reads for the hot path.
#[derive(Debug)]
pub struct SharedConsensusState {
    /// The latest committed QC, updated by the Orchestrator on each BlockCommitted event.
    /// `None` at genesis before any block is committed.
    pub latest_committed_qc: ArcSwap<Option<QuorumCertificate>>,
    /// The current validator set, initialized from chainspec on startup.
    pub validator_set: Arc<ValidatorSet>,
}

impl SharedConsensusState {
    /// Creates a new shared state with the given validator set and no initial QC.
    pub fn new(validator_set: ValidatorSet) -> Self {
        Self {
            latest_committed_qc: ArcSwap::from_pointee(None),
            validator_set: Arc::new(validator_set),
        }
    }

    /// Updates the latest committed QC (called by the Orchestrator).
    pub fn update_committed_qc(&self, qc: QuorumCertificate) {
        self.latest_committed_qc.store(Arc::new(Some(qc)));
    }

    /// Reads the latest committed QC (called by the PayloadBuilder).
    pub fn load_committed_qc(&self) -> Arc<Option<QuorumCertificate>> {
        self.latest_committed_qc.load_full()
    }
}
