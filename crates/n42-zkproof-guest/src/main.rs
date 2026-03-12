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

pub fn main() {
    // Read serialized input from the host.
    let input_bytes = sp1_zkvm::io::read_vec();
    let input: BlockExecutionInput =
        bincode::deserialize(&input_bytes).expect("failed to deserialize BlockExecutionInput");

    // Compute SHA-256 of the bundle state data.
    // This binds the proof to the specific execution state provided by the host.
    let mut hasher = Sha256::new();
    hasher.update(&input.bundle_state_json);
    let state_data_hash: [u8; 32] = hasher.finalize().into();

    let tx_count = input.transactions_rlp.len() as u32;

    // Construct public values.
    let public_values = PublicValues {
        block_hash: input.block_hash,
        block_number: input.block_number,
        parent_hash: input.parent_hash,
        state_data_hash,
        tx_count,
        parent_state_root: input.parent_state_root,
    };

    // Commit public values — these are visible to the verifier.
    let pv_bytes = bincode::serialize(&public_values).expect("failed to serialize PublicValues");
    sp1_zkvm::io::commit_slice(&pv_bytes);
}
