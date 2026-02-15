use alloy_primitives::{Bytes, B256};
use reth_revm::witness::ExecutionWitnessRecord;
use reth_trie_common::HashedPostState;
use revm::database::State;
use std::collections::HashSet;

/// Witness data captured during block execution.
///
/// Contains all state accessed during EVM execution, which is sufficient
/// for an independent verifier (e.g., a mobile device) to re-execute the block
/// without access to the full state trie.
#[derive(Debug, Clone, Default)]
pub struct ExecutionWitness {
    /// Hashed post-state: all accounts and storage slots accessed/modified.
    pub hashed_state: HashedPostState,
    /// Contract bytecodes accessed during execution (keyed by keccak256(code)).
    pub codes: Vec<Bytes>,
    /// Preimage keys: unhashed addresses and storage slots that were accessed.
    pub keys: Vec<Bytes>,
    /// The lowest block number referenced by BLOCKHASH opcode calls.
    pub lowest_block_number: Option<u64>,
}

impl ExecutionWitness {
    /// Records the execution witness from EVM state after block execution.
    ///
    /// This should be called while the `State<DB>` is still available,
    /// before `take_bundle()` consumes the state changes.
    pub fn from_state<DB>(state: &State<DB>) -> Self {
        let record = ExecutionWitnessRecord::from_executed_state(state);
        Self {
            hashed_state: record.hashed_state,
            codes: record.codes,
            keys: record.keys,
            lowest_block_number: record.lowest_block_number,
        }
    }

    /// Compacts the witness by excluding bytecodes that the verifier already has cached.
    ///
    /// Mobile devices maintain a local LRU cache of frequently-used contract bytecodes
    /// (e.g., ERC-20, DEX routers). This method strips those cached codes from the witness
    /// to reduce the data that needs to be transmitted.
    pub fn compact(&self, cached_code_hashes: &HashSet<B256>) -> CompactWitness {
        let uncached_codes: Vec<Bytes> = self
            .codes
            .iter()
            .filter(|code| {
                let hash = alloy_primitives::keccak256(code);
                !cached_code_hashes.contains(&hash)
            })
            .cloned()
            .collect();

        CompactWitness {
            hashed_state: self.hashed_state.clone(),
            uncached_codes,
            keys: self.keys.clone(),
            lowest_block_number: self.lowest_block_number,
        }
    }
}

/// A compact witness that excludes bytecodes the verifier already has cached.
///
/// This is the format sent to mobile devices for verification. It reduces bandwidth
/// by only including contract bytecodes that the mobile device doesn't have in its
/// local cache.
#[derive(Debug, Clone, Default)]
pub struct CompactWitness {
    /// Hashed post-state for trie verification.
    pub hashed_state: HashedPostState,
    /// Only bytecodes not in the verifier's cache.
    pub uncached_codes: Vec<Bytes>,
    /// Preimage keys for state access verification.
    pub keys: Vec<Bytes>,
    /// Lowest block number referenced by BLOCKHASH.
    pub lowest_block_number: Option<u64>,
}
