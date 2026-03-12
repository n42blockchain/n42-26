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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display() {
        let e = ZkProofError::Serialization("bad data".to_string());
        assert_eq!(e.to_string(), "serialization error: bad data");

        let e = ZkProofError::Prover("timeout".to_string());
        assert_eq!(e.to_string(), "prover error: timeout");

        let e = ZkProofError::Verification("invalid proof".to_string());
        assert_eq!(e.to_string(), "verification error: invalid proof");

        let e = ZkProofError::BackendNotAvailable("sp1 not found".to_string());
        assert_eq!(e.to_string(), "backend not available: sp1 not found");
    }

    #[test]
    fn test_error_is_std_error() {
        let e: Box<dyn std::error::Error> =
            Box::new(ZkProofError::Prover("test".to_string()));
        assert!(e.to_string().contains("test"));
    }

    #[test]
    fn test_error_debug() {
        let e = ZkProofError::Serialization("foo".to_string());
        let debug = format!("{:?}", e);
        assert!(debug.contains("Serialization"));
        assert!(debug.contains("foo"));
    }
}
