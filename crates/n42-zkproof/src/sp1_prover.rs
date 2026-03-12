//! SP1 zkVM prover backend.
//!
//! Requires the `sp1` feature flag to compile. The guest program ELF must be
//! pre-built via `cd crates/n42-zkproof-guest && cargo prove build` and its
//! path provided via `N42_ZK_GUEST_ELF` environment variable.
//!
//! Modes:
//! - **mock**: Simulates proof generation without cryptographic work (fast, for testing)
//! - **cpu**: Real proof generation on CPU (slow, minutes per block)

use std::path::PathBuf;
use std::time::Instant;

use sp1_sdk::cpu::CpuProver;
use sp1_sdk::{Prover, SP1ProvingKey, SP1Stdin, SP1VerifyingKey};
use tracing::{info, warn};

use crate::error::ZkProofError;
use crate::prover::{unix_now, BlockExecutionInput, ProofType, ZkProofResult, ZkProver};

/// SP1 prover mode.
#[derive(Debug, Clone, Copy)]
pub enum Sp1Mode {
    /// Simulated execution, no real proof (fast, for development).
    Mock,
    /// Real proof on CPU (slow but correct).
    Cpu,
}

impl Sp1Mode {
    /// Parse from string (env var value).
    pub fn from_str_lossy(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "cpu" => Self::Cpu,
            _ => Self::Mock,
        }
    }
}

/// SP1 zkVM prover backend using `CpuProver`.
///
/// Wraps `sp1_sdk::cpu::CpuProver` and a pre-compiled guest ELF to generate
/// and verify ZK proofs of block execution. Supports mock and real CPU modes.
/// CUDA support requires a different prover type and can be added later.
pub struct Sp1Prover {
    prover: CpuProver,
    pk: SP1ProvingKey,
    vk: SP1VerifyingKey,
    mode: Sp1Mode,
}

impl Sp1Prover {
    /// Create a new SP1 prover.
    ///
    /// `elf` is the compiled guest program binary (RISC-V ELF).
    /// Build it with: `cd crates/n42-zkproof-guest && cargo prove build`
    pub fn new(elf: &[u8], mode: Sp1Mode) -> Result<Self, ZkProofError> {
        let prover = match mode {
            Sp1Mode::Mock => CpuProver::mock(),
            Sp1Mode::Cpu => CpuProver::new(),
        };

        let (pk, vk) = prover.setup(elf);

        info!(
            target: "n42::zk::sp1",
            mode = ?mode,
            "SP1 prover initialized"
        );

        Ok(Self { prover, pk, vk, mode })
    }

    /// Load guest ELF from a file path.
    ///
    /// Reads `N42_ZK_GUEST_ELF` env var, or falls back to the default path
    /// relative to the project root.
    pub fn from_env(mode: Sp1Mode) -> Result<Self, ZkProofError> {
        let elf_path = std::env::var("N42_ZK_GUEST_ELF")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                PathBuf::from("crates/n42-zkproof-guest/elf/riscv32im-succinct-zkvm-elf")
            });

        let elf = std::fs::read(&elf_path).map_err(|e| {
            ZkProofError::BackendNotAvailable(format!(
                "failed to read guest ELF from {}: {}. \
                 Build it with: cd crates/n42-zkproof-guest && cargo prove build",
                elf_path.display(),
                e
            ))
        })?;

        Self::new(&elf, mode)
    }
}

impl ZkProver for Sp1Prover {
    fn name(&self) -> &str {
        "sp1"
    }

    fn prove(&self, input: &BlockExecutionInput) -> Result<ZkProofResult, ZkProofError> {
        let start = Instant::now();

        // Serialize input for the guest.
        let input_bytes = bincode::serialize(input).map_err(|e| {
            ZkProofError::Serialization(format!("failed to serialize input: {e}"))
        })?;

        // Write input to SP1 stdin.
        let mut stdin = SP1Stdin::new();
        stdin.write_vec(input_bytes);

        // Generate compressed proof via CpuProver builder API.
        let proof = self
            .prover
            .prove(&self.pk, &stdin)
            .compressed()
            .run()
            .map_err(|e| ZkProofError::Prover(format!("SP1 prove failed: {e}")))?;

        let generation_ms = start.elapsed().as_millis() as u64;

        // Self-verify using the Prover trait's verify method.
        let verified = Prover::verify(&self.prover, &proof, &self.vk).is_ok();

        if !verified {
            warn!(
                target: "n42::zk::sp1",
                block_number = input.block_number,
                "SP1 proof self-verification failed"
            );
        }

        // Serialize proof for storage.
        let proof_bytes = bincode::serialize(&proof).map_err(|e| {
            ZkProofError::Serialization(format!("failed to serialize proof: {e}"))
        })?;

        let proof_type = match self.mode {
            Sp1Mode::Mock => ProofType::Mock,
            _ => ProofType::Compressed,
        };

        Ok(ZkProofResult {
            block_hash: input.block_hash,
            block_number: input.block_number,
            proof_bytes,
            public_values: proof.public_values.to_vec(),
            proof_type,
            prover_backend: "sp1".to_string(),
            generation_ms,
            verified,
            created_at: unix_now(),
        })
    }

    fn verify(&self, result: &ZkProofResult) -> Result<bool, ZkProofError> {
        let proof: sp1_sdk::SP1ProofWithPublicValues =
            bincode::deserialize(&result.proof_bytes).map_err(|e| {
                ZkProofError::Verification(format!("failed to deserialize proof: {e}"))
            })?;

        Ok(Prover::verify(&self.prover, &proof, &self.vk).is_ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Sp1Mode parsing ---

    #[test]
    fn test_sp1_mode_from_str_lossy_cpu() {
        assert!(matches!(Sp1Mode::from_str_lossy("cpu"), Sp1Mode::Cpu));
        assert!(matches!(Sp1Mode::from_str_lossy("CPU"), Sp1Mode::Cpu));
        assert!(matches!(Sp1Mode::from_str_lossy("Cpu"), Sp1Mode::Cpu));
    }

    #[test]
    fn test_sp1_mode_from_str_lossy_mock() {
        assert!(matches!(Sp1Mode::from_str_lossy("mock"), Sp1Mode::Mock));
        assert!(matches!(Sp1Mode::from_str_lossy("MOCK"), Sp1Mode::Mock));
        assert!(matches!(Sp1Mode::from_str_lossy("Mock"), Sp1Mode::Mock));
    }

    #[test]
    fn test_sp1_mode_from_str_lossy_unknown_defaults_to_mock() {
        assert!(matches!(Sp1Mode::from_str_lossy(""), Sp1Mode::Mock));
        assert!(matches!(Sp1Mode::from_str_lossy("unknown"), Sp1Mode::Mock));
        assert!(matches!(Sp1Mode::from_str_lossy("gpu"), Sp1Mode::Mock));
        assert!(matches!(Sp1Mode::from_str_lossy("cuda"), Sp1Mode::Mock));
    }

    #[test]
    fn test_sp1_mode_debug() {
        assert_eq!(format!("{:?}", Sp1Mode::Mock), "Mock");
        assert_eq!(format!("{:?}", Sp1Mode::Cpu), "Cpu");
    }

    #[test]
    fn test_sp1_mode_clone_copy() {
        let mode = Sp1Mode::Cpu;
        let cloned = mode;
        assert!(matches!(cloned, Sp1Mode::Cpu));
    }

    // --- from_env with missing ELF ---

    /// Tests that `from_env` returns `BackendNotAvailable` when the ELF file
    /// cannot be read. Uses a unique env var value to avoid test interference.
    #[test]
    fn test_from_env_missing_elf_returns_error() {
        // Temporarily override env to point to a guaranteed-nonexistent path.
        // Safety: this is test-only and uses a very specific path.
        let key = "N42_ZK_GUEST_ELF";
        let prev = std::env::var(key).ok();
        unsafe { std::env::set_var(key, "/tmp/__n42_zkproof_test_nonexistent_elf__"); }

        let result = Sp1Prover::from_env(Sp1Mode::Mock);

        // Restore previous value.
        match prev {
            Some(v) => unsafe { std::env::set_var(key, v); },
            None => unsafe { std::env::remove_var(key); },
        }

        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("should fail when ELF file is missing"),
        };
        assert!(
            err.to_string().contains("failed to read guest ELF"),
            "expected ELF read error, got: {err}"
        );
    }

    // --- SP1 mock mode end-to-end tests ---
    // These require the guest ELF to be built first:
    //   cd crates/n42-zkproof-guest && cargo prove build

    /// Helper: locate the guest ELF file relative to the crate root.
    fn find_guest_elf() -> Option<Vec<u8>> {
        // Try paths relative to the workspace root (cargo test runs from crate dir).
        let candidates = [
            "../../crates/n42-zkproof-guest/elf/riscv32im-succinct-zkvm-elf",
            "crates/n42-zkproof-guest/elf/riscv32im-succinct-zkvm-elf",
            "../n42-zkproof-guest/elf/riscv32im-succinct-zkvm-elf",
        ];
        for path in candidates {
            if let Ok(elf) = std::fs::read(path) {
                return Some(elf);
            }
        }
        None
    }

    #[test]
    fn test_sp1_mock_prove_verify_roundtrip() {
        let elf = match find_guest_elf() {
            Some(elf) => elf,
            None => {
                eprintln!("SKIP: guest ELF not found, run: cd crates/n42-zkproof-guest && cargo prove build");
                return;
            }
        };

        let prover = Sp1Prover::new(&elf, Sp1Mode::Mock).expect("SP1 mock prover init failed");
        assert_eq!(prover.name(), "sp1");

        let input = crate::prover::BlockExecutionInput {
            block_hash: alloy_primitives::B256::repeat_byte(0xAA),
            block_number: 42,
            parent_hash: alloy_primitives::B256::repeat_byte(0xBB),
            header_rlp: vec![0xDE, 0xAD],
            transactions_rlp: vec![vec![0xBE, 0xEF], vec![0xCA, 0xFE]],
            bundle_state_json: b"{\"accounts\":{}}".to_vec(),
            parent_state_root: alloy_primitives::B256::repeat_byte(0xCC),
        };

        let result = prover.prove(&input).expect("SP1 mock prove failed");
        assert_eq!(result.block_hash, input.block_hash);
        assert_eq!(result.block_number, 42);
        assert_eq!(result.prover_backend, "sp1");
        assert_eq!(result.proof_type, crate::prover::ProofType::Mock);
        assert!(result.verified, "SP1 mock proof should self-verify");
        assert!(!result.proof_bytes.is_empty(), "proof bytes should not be empty");
        assert!(!result.public_values.is_empty(), "public values should not be empty");
        assert!(result.created_at > 0);

        // Re-verify the proof.
        let valid = prover.verify(&result).expect("SP1 mock verify failed");
        assert!(valid, "SP1 mock proof re-verification should pass");
    }

    #[test]
    fn test_sp1_mock_different_blocks_produce_different_proofs() {
        let elf = match find_guest_elf() {
            Some(elf) => elf,
            None => {
                eprintln!("SKIP: guest ELF not found");
                return;
            }
        };

        let prover = Sp1Prover::new(&elf, Sp1Mode::Mock).unwrap();

        let make_input = |n: u64| crate::prover::BlockExecutionInput {
            block_hash: alloy_primitives::B256::repeat_byte(n as u8),
            block_number: n,
            parent_hash: alloy_primitives::B256::ZERO,
            header_rlp: vec![],
            transactions_rlp: vec![],
            bundle_state_json: vec![],
            parent_state_root: alloy_primitives::B256::ZERO,
        };

        let r1 = prover.prove(&make_input(1)).unwrap();
        let r2 = prover.prove(&make_input(2)).unwrap();

        // Public values should differ (different block_hash, block_number).
        assert_ne!(r1.public_values, r2.public_values);
    }

    #[test]
    fn test_sp1_mock_tampered_proof_fails_verify() {
        let elf = match find_guest_elf() {
            Some(elf) => elf,
            None => {
                eprintln!("SKIP: guest ELF not found");
                return;
            }
        };

        let prover = Sp1Prover::new(&elf, Sp1Mode::Mock).unwrap();

        let input = crate::prover::BlockExecutionInput {
            block_hash: alloy_primitives::B256::repeat_byte(0x11),
            block_number: 100,
            parent_hash: alloy_primitives::B256::ZERO,
            header_rlp: vec![],
            transactions_rlp: vec![],
            bundle_state_json: b"{}".to_vec(),
            parent_state_root: alloy_primitives::B256::ZERO,
        };

        let mut result = prover.prove(&input).unwrap();
        // Corrupt proof bytes.
        if !result.proof_bytes.is_empty() {
            result.proof_bytes[0] ^= 0xFF;
        }
        // Tampered proof should fail verification (or deserialization).
        if let Ok(valid) = prover.verify(&result) {
            assert!(!valid, "tampered proof should not verify");
        }
        // Err case (deserialization failure) is also acceptable.
    }
}
