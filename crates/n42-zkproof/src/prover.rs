use std::time::{SystemTime, UNIX_EPOCH};

use alloy_primitives::B256;
use serde::{Deserialize, Serialize};

use crate::error::ZkProofError;

/// Block execution input passed to the ZK prover.
///
/// Contains all data needed to re-execute a block inside a zkVM and verify
/// the resulting state root and receipts root.
///
/// **IMPORTANT**: The guest program (`crates/n42-zkproof-guest/src/main.rs`)
/// duplicates this struct. Any field changes here MUST be mirrored there,
/// otherwise bincode deserialization will fail at runtime.
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

/// Number of fields in `BlockExecutionInput`. Used as a compile-time anchor
/// to catch accidental field additions. If you add a field above, bump this
/// constant and update the guest's mirror struct accordingly.
pub const BLOCK_EXECUTION_INPUT_FIELD_COUNT: usize = 7;

/// Type of ZK proof generated, ordered by security level (Mock < Core < Compressed < Groth16).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
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
    /// Unix timestamp (seconds) when the proof was created.
    #[serde(default)]
    pub created_at: u64,
}

/// Number of fields in `ZkProofResult`. Compile-time anchor to catch
/// accidental field additions. Bump when adding fields and update all
/// manual constructors (tests, sp1_prover, etc.) accordingly.
pub const ZK_PROOF_RESULT_FIELD_COUNT: usize = 9;

/// Returns current Unix timestamp in seconds.
pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl ZkProofResult {
    /// Returns true if this is a mock proof (not cryptographically secure).
    pub fn is_mock(&self) -> bool {
        self.proof_type == ProofType::Mock
    }
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
        let mut proof_data = Vec::with_capacity(40);
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
            created_at: unix_now(),
        })
    }

    fn verify(&self, result: &ZkProofResult) -> Result<bool, ZkProofError> {
        // Mock verification: proof_bytes must be exactly block_hash (32) || block_number (8).
        if result.proof_bytes.len() != 40 {
            return Ok(false);
        }
        let hash_ok = result.proof_bytes[..32] == *result.block_hash.as_slice();
        let num_ok = result.proof_bytes[32..40] == result.block_number.to_le_bytes();
        Ok(hash_ok && num_ok)
    }
}

// Compile-time field count assertions.
// If you add a field to BlockExecutionInput or ZkProofResult, bump the
// corresponding FIELD_COUNT constant and update all constructors.
const _: () = {
    // Verify BlockExecutionInput has exactly BLOCK_EXECUTION_INPUT_FIELD_COUNT fields.
    // This works by constructing the struct — compilation fails if field count mismatches.
    #[allow(dead_code)]
    fn assert_input_fields() {
        let _input = BlockExecutionInput {
            block_hash: alloy_primitives::B256::ZERO,
            block_number: 0,
            parent_hash: alloy_primitives::B256::ZERO,
            header_rlp: Vec::new(),
            transactions_rlp: Vec::new(),
            bundle_state_json: Vec::new(),
            parent_state_root: alloy_primitives::B256::ZERO,
        };
        // If this doesn't match the constant, the struct changed without updating.
        const { assert!(BLOCK_EXECUTION_INPUT_FIELD_COUNT == 7) };
    }

    #[allow(dead_code)]
    fn assert_result_fields() {
        let _result = ZkProofResult {
            block_hash: alloy_primitives::B256::ZERO,
            block_number: 0,
            proof_bytes: Vec::new(),
            public_values: Vec::new(),
            proof_type: ProofType::Mock,
            prover_backend: String::new(),
            generation_ms: 0,
            verified: false,
            created_at: 0,
        };
        const { assert!(ZK_PROOF_RESULT_FIELD_COUNT == 9) };
    }
};

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to create a `BlockExecutionInput` with configurable fields.
    fn make_input(block_number: u64) -> BlockExecutionInput {
        let mut hash_bytes = [0u8; 32];
        hash_bytes[..8].copy_from_slice(&block_number.to_le_bytes());
        BlockExecutionInput {
            block_hash: B256::from(hash_bytes),
            block_number,
            parent_hash: B256::repeat_byte(0xBB),
            header_rlp: vec![1, 2, 3],
            transactions_rlp: vec![vec![4, 5, 6]],
            bundle_state_json: b"{\"state\":true}".to_vec(),
            parent_state_root: B256::repeat_byte(0xCC),
        }
    }

    // --- MockProver basic ---

    #[test]
    fn test_mock_prover_name() {
        let prover = MockProver::new();
        assert_eq!(prover.name(), "mock");
    }

    #[test]
    fn test_mock_prover_default() {
        let prover: MockProver = Default::default();
        assert_eq!(prover.name(), "mock");
    }

    #[test]
    fn test_mock_prove_verify_roundtrip() {
        let prover = MockProver::new();
        let input = make_input(42);

        let result = prover.prove(&input).unwrap();
        assert_eq!(result.block_hash, input.block_hash);
        assert_eq!(result.block_number, 42);
        assert_eq!(result.proof_type, ProofType::Mock);
        assert_eq!(result.prover_backend, "mock");
        assert!(result.verified);
        assert!(result.is_mock());

        let valid = prover.verify(&result).unwrap();
        assert!(valid);
    }

    #[test]
    fn test_mock_prove_deterministic() {
        let prover = MockProver::new();
        let input = make_input(100);

        let r1 = prover.prove(&input).unwrap();
        let r2 = prover.prove(&input).unwrap();

        assert_eq!(r1.proof_bytes, r2.proof_bytes);
        assert_eq!(r1.public_values, r2.public_values);
        assert_eq!(r1.block_hash, r2.block_hash);
    }

    #[test]
    fn test_mock_prove_different_blocks_produce_different_proofs() {
        let prover = MockProver::new();
        let r1 = prover.prove(&make_input(1)).unwrap();
        let r2 = prover.prove(&make_input(2)).unwrap();

        assert_ne!(r1.proof_bytes, r2.proof_bytes);
        assert_ne!(r1.block_hash, r2.block_hash);
    }

    // --- Tamper detection ---

    #[test]
    fn test_mock_verify_tampered_block_hash() {
        let prover = MockProver::new();
        let mut result = prover.prove(&make_input(1)).unwrap();
        result.block_hash = B256::repeat_byte(0xFF);
        assert!(!prover.verify(&result).unwrap());
    }

    #[test]
    fn test_mock_verify_tampered_proof_bytes() {
        let prover = MockProver::new();
        let mut result = prover.prove(&make_input(1)).unwrap();
        // Corrupt first byte of proof_bytes.
        result.proof_bytes[0] ^= 0xFF;
        assert!(!prover.verify(&result).unwrap());
    }

    #[test]
    fn test_mock_verify_empty_proof_bytes() {
        let prover = MockProver::new();
        let mut result = prover.prove(&make_input(1)).unwrap();
        result.proof_bytes = vec![];
        assert!(!prover.verify(&result).unwrap());
    }

    #[test]
    fn test_mock_verify_truncated_proof_bytes() {
        let prover = MockProver::new();
        let mut result = prover.prove(&make_input(1)).unwrap();
        result.proof_bytes.truncate(16); // Less than 32 bytes.
        assert!(!prover.verify(&result).unwrap());
    }

    // --- Large input handling ---

    #[test]
    fn test_mock_prove_large_input() {
        let prover = MockProver::new();
        let input = BlockExecutionInput {
            block_hash: B256::repeat_byte(0xAA),
            block_number: 1_000_000,
            parent_hash: B256::repeat_byte(0xBB),
            header_rlp: vec![0xDE; 1024],
            transactions_rlp: (0..1000).map(|i| vec![i as u8; 256]).collect(),
            bundle_state_json: vec![0x7B; 1_000_000], // 1MB of data
            parent_state_root: B256::repeat_byte(0xCC),
        };

        let result = prover.prove(&input).unwrap();
        assert!(prover.verify(&result).unwrap());
    }

    #[test]
    fn test_mock_prove_empty_transactions() {
        let prover = MockProver::new();
        let input = BlockExecutionInput {
            block_hash: B256::repeat_byte(0x11),
            block_number: 0,
            parent_hash: B256::ZERO,
            header_rlp: vec![],
            transactions_rlp: vec![],
            bundle_state_json: vec![],
            parent_state_root: B256::ZERO,
        };

        let result = prover.prove(&input).unwrap();
        assert!(prover.verify(&result).unwrap());
    }

    // --- ProofType ---

    #[test]
    fn test_proof_type_display() {
        assert_eq!(ProofType::Mock.to_string(), "mock");
        assert_eq!(ProofType::Core.to_string(), "core");
        assert_eq!(ProofType::Compressed.to_string(), "compressed");
        assert_eq!(ProofType::Groth16.to_string(), "groth16");
    }

    #[test]
    fn test_proof_type_ordering() {
        assert!(ProofType::Mock < ProofType::Core);
        assert!(ProofType::Core < ProofType::Compressed);
        assert!(ProofType::Compressed < ProofType::Groth16);
    }

    #[test]
    fn test_proof_type_serde_roundtrip() {
        for pt in [
            ProofType::Mock,
            ProofType::Core,
            ProofType::Compressed,
            ProofType::Groth16,
        ] {
            let json = serde_json::to_string(&pt).unwrap();
            let decoded: ProofType = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, pt);
        }
    }

    // --- ZkProofResult ---

    #[test]
    fn test_proof_result_is_mock() {
        let mut result = ZkProofResult {
            block_hash: B256::ZERO,
            block_number: 0,
            proof_bytes: vec![],
            public_values: vec![],
            proof_type: ProofType::Mock,
            prover_backend: "mock".to_string(),
            generation_ms: 0,
            verified: true,
            created_at: 0,
        };
        assert!(result.is_mock());

        result.proof_type = ProofType::Compressed;
        assert!(!result.is_mock());
    }

    #[test]
    fn test_proof_result_created_at() {
        let prover = MockProver::new();
        let result = prover.prove(&make_input(1)).unwrap();
        assert!(result.created_at > 0, "created_at should be set");
        // Should be within last 10 seconds.
        let now = unix_now();
        assert!(now - result.created_at < 10);
    }

    #[test]
    fn test_mock_verify_tampered_block_number() {
        let prover = MockProver::new();
        let mut result = prover.prove(&make_input(1)).unwrap();
        result.block_number = 999;
        assert!(!prover.verify(&result).unwrap());
    }

    // --- Serialization roundtrips ---

    #[test]
    fn test_input_bincode_roundtrip() {
        let input = make_input(100);
        let encoded = bincode::serialize(&input).unwrap();
        let decoded: BlockExecutionInput = bincode::deserialize(&encoded).unwrap();
        assert_eq!(decoded.block_hash, input.block_hash);
        assert_eq!(decoded.block_number, input.block_number);
        assert_eq!(decoded.parent_hash, input.parent_hash);
        assert_eq!(decoded.header_rlp, input.header_rlp);
        assert_eq!(decoded.transactions_rlp, input.transactions_rlp);
        assert_eq!(decoded.bundle_state_json, input.bundle_state_json);
        assert_eq!(decoded.parent_state_root, input.parent_state_root);
    }

    #[test]
    fn test_input_json_roundtrip() {
        let input = make_input(200);
        let json = serde_json::to_string(&input).unwrap();
        let decoded: BlockExecutionInput = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.block_hash, input.block_hash);
        assert_eq!(decoded.block_number, input.block_number);
    }

    #[test]
    fn test_proof_result_json_roundtrip() {
        let result = ZkProofResult {
            block_hash: B256::repeat_byte(0xDD),
            block_number: 999,
            proof_bytes: vec![1, 2, 3, 4],
            public_values: vec![5, 6, 7, 8],
            proof_type: ProofType::Compressed,
            prover_backend: "sp1".to_string(),
            generation_ms: 12345,
            verified: true,
            created_at: 1700000000,
        };

        let json = serde_json::to_string(&result).unwrap();
        let decoded: ZkProofResult = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.block_number, 999);
        assert_eq!(decoded.proof_type, ProofType::Compressed);
        assert_eq!(decoded.prover_backend, "sp1");
        assert_eq!(decoded.generation_ms, 12345);
        assert!(decoded.verified);
    }

    #[test]
    fn test_proof_result_bincode_roundtrip() {
        let result = ZkProofResult {
            block_hash: B256::repeat_byte(0xEE),
            block_number: 500,
            proof_bytes: vec![0xAA; 128],
            public_values: vec![0xBB; 64],
            proof_type: ProofType::Core,
            prover_backend: "test".to_string(),
            generation_ms: 42,
            verified: false,
            created_at: 1700000000,
        };

        let encoded = bincode::serialize(&result).unwrap();
        let decoded: ZkProofResult = bincode::deserialize(&encoded).unwrap();
        assert_eq!(decoded.block_hash, result.block_hash);
        assert_eq!(decoded.proof_bytes, result.proof_bytes);
        assert_eq!(decoded.public_values, result.public_values);
        assert!(!decoded.verified);
    }
}
