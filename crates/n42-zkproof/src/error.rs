use thiserror::Error;

/// Errors from the ZK proof subsystem.
#[derive(Debug, Error)]
pub enum ZkProofError {
    #[error("serialization error: {0}")]
    Serialization(String),

    #[error("prover error: {0}")]
    Prover(String),

    #[error("verification error: {0}")]
    Verification(String),

    #[error("backend not available: {0}")]
    BackendNotAvailable(String),
}
