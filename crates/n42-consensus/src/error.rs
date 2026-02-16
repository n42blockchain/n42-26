use n42_primitives::consensus::ViewNumber;
use thiserror::Error;

/// Errors specific to the N42 HotStuff-2 consensus protocol.
#[derive(Debug, Error)]
pub enum ConsensusError {
    /// BLS signature verification failed.
    #[error("invalid BLS signature from validator {validator_index} in view {view}")]
    InvalidSignature {
        view: ViewNumber,
        validator_index: u32,
    },

    /// The message sender is not the expected leader for this view.
    #[error("invalid proposer: expected validator {expected}, got {actual} in view {view}")]
    InvalidProposer {
        view: ViewNumber,
        expected: u32,
        actual: u32,
    },

    /// Not enough votes to form a quorum certificate.
    #[error("insufficient votes: have {have}, need {need} in view {view}")]
    InsufficientVotes {
        view: ViewNumber,
        have: usize,
        need: usize,
    },

    /// Message view does not match the current view.
    #[error("view mismatch: current view is {current}, message view is {received}")]
    ViewMismatch {
        current: ViewNumber,
        received: ViewNumber,
    },

    /// Duplicate vote from the same validator in the same view.
    #[error("duplicate vote from validator {validator_index} in view {view}")]
    DuplicateVote {
        view: ViewNumber,
        validator_index: u32,
    },

    /// The quorum certificate is invalid (bad signature or insufficient signers).
    #[error("invalid quorum certificate for view {view}: {reason}")]
    InvalidQC {
        view: ViewNumber,
        reason: String,
    },

    /// The timeout certificate is invalid.
    #[error("invalid timeout certificate for view {view}: {reason}")]
    InvalidTC {
        view: ViewNumber,
        reason: String,
    },

    /// The validator index is out of bounds.
    #[error("unknown validator index {index}, set size is {set_size}")]
    UnknownValidator {
        index: u32,
        set_size: u32,
    },

    /// The proposal's justify_qc does not extend the locked QC.
    #[error("safety violation: proposal QC view {qc_view} does not extend locked QC view {locked_view}")]
    SafetyViolation {
        qc_view: ViewNumber,
        locked_view: ViewNumber,
    },

    /// Block hash in QC does not match expected block hash (e.g., in Decide message).
    #[error("block hash mismatch: expected {expected}, got {got}")]
    BlockHashMismatch {
        expected: alloy_primitives::B256,
        got: alloy_primitives::B256,
    },

    /// Internal BLS cryptographic error.
    #[error("BLS error: {0}")]
    Bls(#[from] n42_primitives::bls::BlsError),
}

/// Result type for consensus operations.
pub type ConsensusResult<T> = Result<T, ConsensusError>;
