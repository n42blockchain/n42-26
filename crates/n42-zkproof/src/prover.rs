use alloy_primitives::B256;
use serde::{Deserialize, Serialize};

use crate::error::ZkProofError;

/// Block execution input passed to the ZK prover.
///
/// Contains all data needed to re-execute a block inside a zkVM and verify
/// the resulting state root and receipts root.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockExecutionInput {
    pub block_hash: B256,
    pub block_number: u64,
    pub parent_hash: B256,
    /// RLP-encoded block header.
    pub header_rlp: Vec<u8>,
    /// RLP-encoded transactions.
    pub transactions_rlp: Vec<Vec<u8>>,
    /// Serialized CompactBlockExecution (serde_json bytes).
    pub bundle_state_json: Vec<u8>,
    pub parent_state_root: B256,
}

/// Type of ZK proof generated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProofType {
    /// Fast proof for development/testing (mock).
    Mock,
    /// SP1 core proof (large, fast to generate).
    Core,
    /// SP1 compressed proof (smaller, slower to generate).
    Compressed,
    /// Groth16 wrapper (on-chain verifiable, slowest).
    Groth16,
}

impl std::fmt::Display for ProofType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Mock => write!(f, "mock"),
            Self::Core => write!(f, "core"),
            Self::Compressed => write!(f, "compressed"),
            Self::Groth16 => write!(f, "groth16"),
        }
    }
}

/// Result of a successful ZK proof generation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZkProofResult {
    pub block_hash: B256,
    pub block_number: u64,
    /// Serialized proof bytes (format depends on backend).
    pub proof_bytes: Vec<u8>,
    /// Public values committed by the guest program
    /// (block_hash, state_root, receipts_root).
    pub public_values: Vec<u8>,
    pub proof_type: ProofType,
    /// Which prover backend generated this proof.
    pub prover_backend: String,
    /// Time taken to generate the proof in milliseconds.
    pub generation_ms: u64,
    /// Whether the proof was self-verified after generation.
    pub verified: bool,
}

/// Backend-agnostic ZK prover trait.
///
/// Implementations include `MockProver` (testing), and future `Sp1Prover` / `ZisKProver`.
pub trait ZkProver: Send + Sync {
    /// Human-readable backend name.
    fn name(&self) -> &str;

    /// Generate a ZK proof for the given block execution input.
    fn prove(&self, input: &BlockExecutionInput) -> Result<ZkProofResult, ZkProofError>;

    /// Verify a previously generated proof.
    fn verify(&self, result: &ZkProofResult) -> Result<bool, ZkProofError>;
}

/// Mock prover for testing: generates deterministic fake proofs instantly.
pub struct MockProver;

impl MockProver {
    pub fn new() -> Self {
        Self
    }
}

impl Default for MockProver {
    fn default() -> Self {
        Self::new()
    }
}

impl ZkProver for MockProver {
    fn name(&self) -> &str {
        "mock"
    }

    fn prove(&self, input: &BlockExecutionInput) -> Result<ZkProofResult, ZkProofError> {
        // Deterministic mock proof: hash of (block_hash || block_number).
        let mut proof_data = Vec::with_capacity(64);
        proof_data.extend_from_slice(input.block_hash.as_slice());
        proof_data.extend_from_slice(&input.block_number.to_le_bytes());

        // Public values: block_hash (32) + parent_state_root (32) as placeholder.
        let mut public_values = Vec::with_capacity(64);
        public_values.extend_from_slice(input.block_hash.as_slice());
        public_values.extend_from_slice(input.parent_state_root.as_slice());

        Ok(ZkProofResult {
            block_hash: input.block_hash,
            block_number: input.block_number,
            proof_bytes: proof_data,
            public_values,
            proof_type: ProofType::Mock,
            prover_backend: "mock".to_string(),
            generation_ms: 0,
            verified: true,
        })
    }

    fn verify(&self, result: &ZkProofResult) -> Result<bool, ZkProofError> {
        // Mock verification: check that proof_bytes starts with block_hash.
        Ok(result.proof_bytes.len() >= 32
            && result.proof_bytes[..32] == *result.block_hash.as_slice())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mock_prove_verify_roundtrip() {
        let prover = MockProver::new();
        let input = BlockExecutionInput {
            block_hash: B256::repeat_byte(0xAA),
            block_number: 42,
            parent_hash: B256::repeat_byte(0xBB),
            header_rlp: vec![1, 2, 3],
            transactions_rlp: vec![vec![4, 5, 6]],
            bundle_state_json: b"{}".to_vec(),
            parent_state_root: B256::repeat_byte(0xCC),
        };

        let result = prover.prove(&input).unwrap();
        assert_eq!(result.block_hash, input.block_hash);
        assert_eq!(result.block_number, 42);
        assert_eq!(result.proof_type, ProofType::Mock);
        assert_eq!(result.prover_backend, "mock");
        assert!(result.verified);

        let valid = prover.verify(&result).unwrap();
        assert!(valid);
    }

    #[test]
    fn test_mock_verify_tampered_proof() {
        let prover = MockProver::new();
        let input = BlockExecutionInput {
            block_hash: B256::repeat_byte(0xAA),
            block_number: 1,
            parent_hash: B256::ZERO,
            header_rlp: vec![],
            transactions_rlp: vec![],
            bundle_state_json: vec![],
            parent_state_root: B256::ZERO,
        };

        let mut result = prover.prove(&input).unwrap();
        // Tamper with block_hash but keep proof_bytes unchanged.
        result.block_hash = B256::repeat_byte(0xFF);
        let valid = prover.verify(&result).unwrap();
        assert!(!valid, "tampered proof should fail verification");
    }

    #[test]
    fn test_input_serialization_roundtrip() {
        let input = BlockExecutionInput {
            block_hash: B256::repeat_byte(0x11),
            block_number: 100,
            parent_hash: B256::repeat_byte(0x22),
            header_rlp: vec![0xDE, 0xAD],
            transactions_rlp: vec![vec![0xBE, 0xEF]],
            bundle_state_json: b"{\"test\":true}".to_vec(),
            parent_state_root: B256::repeat_byte(0x33),
        };

        let encoded = bincode::serialize(&input).unwrap();
        let decoded: BlockExecutionInput = bincode::deserialize(&encoded).unwrap();
        assert_eq!(decoded.block_hash, input.block_hash);
        assert_eq!(decoded.block_number, input.block_number);
        assert_eq!(decoded.transactions_rlp, input.transactions_rlp);
    }

    #[test]
    fn test_proof_result_serialization_roundtrip() {
        let result = ZkProofResult {
            block_hash: B256::repeat_byte(0xDD),
            block_number: 999,
            proof_bytes: vec![1, 2, 3, 4],
            public_values: vec![5, 6, 7, 8],
            proof_type: ProofType::Compressed,
            prover_backend: "sp1".to_string(),
            generation_ms: 12345,
            verified: true,
        };

        let json = serde_json::to_string(&result).unwrap();
        let decoded: ZkProofResult = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.block_number, 999);
        assert_eq!(decoded.proof_type, ProofType::Compressed);
        assert_eq!(decoded.prover_backend, "sp1");
    }
}
