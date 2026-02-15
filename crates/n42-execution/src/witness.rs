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
        let mut uncached_codes = Vec::new();
        let mut referenced_cached_hashes = Vec::new();

        for code in &self.codes {
            let hash = alloy_primitives::keccak256(code);
            if cached_code_hashes.contains(&hash) {
                referenced_cached_hashes.push(hash);
            } else {
                uncached_codes.push(code.clone());
            }
        }

        CompactWitness {
            hashed_state: self.hashed_state.clone(),
            uncached_codes,
            cached_code_hashes: referenced_cached_hashes,
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
    /// Code hashes that the verifier should load from its local cache.
    pub cached_code_hashes: Vec<B256>,
    /// Preimage keys for state access verification.
    pub keys: Vec<Bytes>,
    /// Lowest block number referenced by BLOCKHASH.
    pub lowest_block_number: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{keccak256, Bytes, B256};
    use std::collections::HashSet;

    /// Helper: create a simple bytecode for testing.
    fn bytecode(data: &[u8]) -> Bytes {
        Bytes::from(data.to_vec())
    }

    #[test]
    fn test_execution_witness_default() {
        let witness = ExecutionWitness::default();
        assert!(witness.codes.is_empty(), "default witness should have no codes");
        assert!(witness.keys.is_empty(), "default witness should have no keys");
        assert!(
            witness.lowest_block_number.is_none(),
            "default witness should have no lowest_block_number"
        );
    }

    #[test]
    fn test_compact_witness_default() {
        let cw = CompactWitness::default();
        assert!(cw.uncached_codes.is_empty());
        assert!(cw.keys.is_empty());
        assert!(cw.lowest_block_number.is_none());
    }

    #[test]
    fn test_compact_no_cached_codes() {
        let code_a = bytecode(&[0xAA, 0xBB, 0xCC]);
        let code_b = bytecode(&[0xDD, 0xEE]);

        let witness = ExecutionWitness {
            hashed_state: HashedPostState::default(),
            codes: vec![code_a.clone(), code_b.clone()],
            keys: vec![bytecode(&[1, 2, 3])],
            lowest_block_number: Some(100),
        };

        // No cached hashes → all codes should be included
        let cached: HashSet<B256> = HashSet::new();
        let compact = witness.compact(&cached);

        assert_eq!(
            compact.uncached_codes.len(),
            2,
            "all codes should be uncached when cache is empty"
        );
        assert_eq!(compact.keys.len(), 1);
        assert_eq!(compact.lowest_block_number, Some(100));
    }

    #[test]
    fn test_compact_all_cached() {
        let code_a = bytecode(&[0xAA, 0xBB, 0xCC]);
        let code_b = bytecode(&[0xDD, 0xEE]);

        let hash_a = keccak256(&code_a);
        let hash_b = keccak256(&code_b);

        let witness = ExecutionWitness {
            hashed_state: HashedPostState::default(),
            codes: vec![code_a, code_b],
            keys: vec![],
            lowest_block_number: None,
        };

        // All codes are cached → uncached_codes should be empty
        let cached: HashSet<B256> = HashSet::from([hash_a, hash_b]);
        let compact = witness.compact(&cached);

        assert!(
            compact.uncached_codes.is_empty(),
            "all codes are cached, uncached_codes should be empty"
        );
        assert_eq!(
            compact.cached_code_hashes.len(),
            2,
            "both code hashes should be referenced"
        );
    }

    #[test]
    fn test_compact_partial_cache() {
        let code_a = bytecode(&[0xAA, 0xBB, 0xCC]);
        let code_b = bytecode(&[0xDD, 0xEE]);
        let code_c = bytecode(&[0xFF]);

        let hash_a = keccak256(&code_a);
        // hash_b is NOT cached
        let hash_c = keccak256(&code_c);

        let witness = ExecutionWitness {
            hashed_state: HashedPostState::default(),
            codes: vec![code_a, code_b.clone(), code_c],
            keys: vec![],
            lowest_block_number: None,
        };

        // Only code_a and code_c are cached
        let cached: HashSet<B256> = HashSet::from([hash_a, hash_c]);
        let compact = witness.compact(&cached);

        assert_eq!(
            compact.uncached_codes.len(),
            1,
            "only code_b should be uncached"
        );
        assert_eq!(compact.uncached_codes[0], code_b);
        assert_eq!(
            compact.cached_code_hashes.len(),
            2,
            "code_a and code_c hashes should be referenced"
        );
    }

    #[test]
    fn test_compact_preserves_keys_and_hashed_state() {
        let key1 = bytecode(&[1, 2, 3]);
        let key2 = bytecode(&[4, 5, 6]);

        let witness = ExecutionWitness {
            hashed_state: HashedPostState::default(),
            codes: vec![],
            keys: vec![key1.clone(), key2.clone()],
            lowest_block_number: Some(42),
        };

        let compact = witness.compact(&HashSet::new());

        assert_eq!(compact.keys.len(), 2);
        assert_eq!(compact.keys[0], key1);
        assert_eq!(compact.keys[1], key2);
        assert_eq!(compact.lowest_block_number, Some(42));
    }
}
