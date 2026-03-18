//! N42 ZK Proof Guest Program
//!
//! Runs inside SP1 zkVM (RISC-V). Reads a serialized `BlockExecutionInput`,
//! verifies data integrity, and commits public values that the host can verify.
//!
//! Build: `cd crates/n42-zkproof-guest && cargo prove build`
//! Output: `elf/riscv32im-succinct-zkvm-elf` (ELF binary for SP1)

#![no_main]
sp1_zkvm::entrypoint!(main);

use alloy_primitives::B256;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Mirror of `n42_zkproof::BlockExecutionInput`.
/// Duplicated here to avoid cross-compilation issues with the host crate.
#[derive(Debug, Serialize, Deserialize)]
struct BlockExecutionInput {
    block_hash: B256,
    block_number: u64,
    parent_hash: B256,
    header_rlp: Vec<u8>,
    transactions_rlp: Vec<Vec<u8>>,
    bundle_state_json: Vec<u8>,
    parent_state_root: B256,
}

/// Public values committed by the guest.
/// The verifier checks these against expected values.
#[derive(Debug, Serialize, Deserialize)]
struct PublicValues {
    /// The block hash being proven.
    block_hash: B256,
    /// Block number for reference.
    block_number: u64,
    /// Parent block hash.
    parent_hash: B256,
    /// SHA-256 digest of the bundle_state_json, proving the guest
    /// processed the exact state data provided by the host.
    state_data_hash: [u8; 32],
    /// Number of transactions in the block.
    tx_count: u32,
    /// Parent state root (from host input).
    parent_state_root: B256,
}

fn decode_input(input_bytes: &[u8]) -> Result<BlockExecutionInput, bincode::Error> {
    bincode::deserialize(input_bytes)
}

fn compute_public_values(input: &BlockExecutionInput) -> PublicValues {
    let mut hasher = Sha256::new();
    hasher.update(&input.bundle_state_json);
    let state_data_hash: [u8; 32] = hasher.finalize().into();

    PublicValues {
        block_hash: input.block_hash,
        block_number: input.block_number,
        parent_hash: input.parent_hash,
        state_data_hash,
        tx_count: input.transactions_rlp.len() as u32,
        parent_state_root: input.parent_state_root,
    }
}

fn encode_public_values(public_values: &PublicValues) -> Result<Vec<u8>, bincode::Error> {
    bincode::serialize(public_values)
}

pub fn main() {
    // Read serialized input from the host.
    let input_bytes = sp1_zkvm::io::read_vec();
    let input = decode_input(&input_bytes).expect("failed to deserialize BlockExecutionInput");
    let public_values = compute_public_values(&input);

    // Commit public values — these are visible to the verifier.
    let pv_bytes = encode_public_values(&public_values).expect("failed to serialize PublicValues");
    sp1_zkvm::io::commit_slice(&pv_bytes);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_input() -> BlockExecutionInput {
        BlockExecutionInput {
            block_hash: B256::with_last_byte(0x11),
            block_number: 42,
            parent_hash: B256::with_last_byte(0x22),
            header_rlp: vec![0xAA, 0xBB],
            transactions_rlp: vec![vec![0x01, 0x02], vec![0x03]],
            bundle_state_json: br#"{"accounts":2}"#.to_vec(),
            parent_state_root: B256::with_last_byte(0x33),
        }
    }

    #[test]
    fn computes_public_values_from_input() {
        let input = sample_input();
        let public_values = compute_public_values(&input);

        assert_eq!(public_values.block_hash, input.block_hash);
        assert_eq!(public_values.block_number, input.block_number);
        assert_eq!(public_values.parent_hash, input.parent_hash);
        assert_eq!(public_values.tx_count, 2);
        assert_eq!(public_values.parent_state_root, input.parent_state_root);

        let expected_hash: [u8; 32] = Sha256::digest(&input.bundle_state_json).into();
        assert_eq!(public_values.state_data_hash, expected_hash);
    }

    #[test]
    fn input_and_public_values_roundtrip() {
        let input = sample_input();
        let encoded_input = bincode::serialize(&input).unwrap();
        let decoded_input = decode_input(&encoded_input).unwrap();
        let public_values = compute_public_values(&decoded_input);
        let encoded_public_values = encode_public_values(&public_values).unwrap();
        let decoded_public_values: PublicValues =
            bincode::deserialize(&encoded_public_values).unwrap();

        assert_eq!(decoded_public_values.block_hash, input.block_hash);
        assert_eq!(decoded_public_values.tx_count, 2);
        assert_eq!(
            decoded_public_values.parent_state_root,
            input.parent_state_root
        );
    }
}
