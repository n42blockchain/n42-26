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

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::B256;

    #[test]
    fn test_error_display_formats() {
        let cases: Vec<(ConsensusError, &[&str])> = vec![
            (
                ConsensusError::InvalidSignature { view: 5, validator_index: 3 },
                &["invalid BLS signature", "3", "5"],
            ),
            (
                ConsensusError::InvalidProposer { view: 10, expected: 1, actual: 2 },
                &["invalid proposer", "1", "2", "10"],
            ),
            (
                ConsensusError::InsufficientVotes { view: 1, have: 2, need: 3 },
                &["insufficient votes", "2", "3"],
            ),
            (
                ConsensusError::ViewMismatch { current: 5, received: 10 },
                &["view mismatch", "5", "10"],
            ),
            (
                ConsensusError::DuplicateVote { view: 1, validator_index: 0 },
                &["duplicate vote", "0"],
            ),
            (
                ConsensusError::InvalidQC { view: 3, reason: "bad sig".into() },
                &["invalid quorum certificate", "bad sig", "3"],
            ),
            (
                ConsensusError::InvalidTC { view: 7, reason: "too few".into() },
                &["invalid timeout certificate", "too few", "7"],
            ),
            (
                ConsensusError::UnknownValidator { index: 99, set_size: 4 },
                &["unknown validator", "99", "4"],
            ),
            (
                ConsensusError::SafetyViolation { qc_view: 1, locked_view: 5 },
                &["safety violation", "1", "5"],
            ),
            (
                ConsensusError::BlockHashMismatch {
                    expected: B256::ZERO,
                    got: B256::repeat_byte(0xFF),
                },
                &["block hash mismatch"],
            ),
        ];

        for (err, keywords) in &cases {
            let s = err.to_string();
            for kw in *keywords {
                assert!(s.contains(kw), "Display for {:?} should contain '{}'", err, kw);
            }
        }
    }

    #[test]
    fn test_bls_error_from_conversion() {
        let bls_err = n42_primitives::bls::BlsError::SigningFailed;
        let consensus_err: ConsensusError = bls_err.into();
        let display = consensus_err.to_string();
        assert!(display.contains("BLS"));
    }

    #[test]
    fn test_consensus_result_ok_and_err() {
        let ok: ConsensusResult<u32> = Ok(42);
        assert_eq!(ok.unwrap(), 42);

        let err: ConsensusResult<u32> = Err(ConsensusError::ViewMismatch {
            current: 1,
            received: 2,
        });
        assert!(err.is_err());
    }
}
