use alloy_primitives::{Bytes, B256};
use reth_revm::witness::ExecutionWitnessRecord;
use reth_trie_common::HashedPostState;
use revm::database::State;
use std::collections::HashSet;
use tracing::debug;

/// Witness data captured during block execution.
/// Contains all state accessed during EVM execution for independent verification.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
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

    /// Returns the approximate total size of bytecodes in the witness (bytes).
    ///
    /// Useful for capacity planning and monitoring packet sizes before
    /// compaction and transmission to mobile devices.
    pub fn total_code_bytes(&self) -> usize {
        self.codes.iter().map(|c| c.len()).sum()
    }

    /// Compacts the witness by excluding cached bytecodes.
    /// Removes bytecodes from `cached_code_hashes` to reduce transmission size.
    pub fn compact(&self, cached_code_hashes: &HashSet<B256>) -> CompactWitness {
        let mut uncached_codes = Vec::new();
        let mut seen_cached = HashSet::new();
        let mut referenced_cached_hashes = Vec::new();

        for code in &self.codes {
            let hash = alloy_primitives::keccak256(code);
            if cached_code_hashes.contains(&hash) {
                // Deduplicate: only record each cached hash once.
                if seen_cached.insert(hash) {
                    referenced_cached_hashes.push(hash);
                }
            } else {
                uncached_codes.push(code.clone());
            }
        }

        debug!(
            target: "n42::execution",
            total_codes = self.codes.len(),
            cached = referenced_cached_hashes.len(),
            uncached = uncached_codes.len(),
            "witness compacted"
        );

        CompactWitness {
            hashed_state: self.hashed_state.clone(),
            uncached_codes,
            cached_code_hashes: referenced_cached_hashes,
            keys: self.keys.clone(),
            lowest_block_number: self.lowest_block_number,
        }
    }
}

/// A compact witness excluding cached bytecodes.
/// Sent to mobile devices, includes only uncached bytecodes to reduce bandwidth.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
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

    fn bytecode(data: &[u8]) -> Bytes {
        Bytes::from(data.to_vec())
    }

    #[test]
    fn test_execution_witness_default() {
        let witness = ExecutionWitness::default();
        assert!(witness.codes.is_empty());
        assert!(witness.keys.is_empty());
        assert!(witness.lowest_block_number.is_none());
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
        let cached: HashSet<B256> = HashSet::new();
        let compact = witness.compact(&cached);
        assert_eq!(compact.uncached_codes.len(), 2);
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
        let cached: HashSet<B256> = HashSet::from([hash_a, hash_b]);
        let compact = witness.compact(&cached);
        assert!(compact.uncached_codes.is_empty());
        assert_eq!(compact.cached_code_hashes.len(), 2);
    }

    #[test]
    fn test_compact_partial_cache() {
        let code_a = bytecode(&[0xAA, 0xBB, 0xCC]);
        let code_b = bytecode(&[0xDD, 0xEE]);
        let code_c = bytecode(&[0xFF]);
        let hash_a = keccak256(&code_a);
        let hash_c = keccak256(&code_c);
        let witness = ExecutionWitness {
            hashed_state: HashedPostState::default(),
            codes: vec![code_a, code_b.clone(), code_c],
            keys: vec![],
            lowest_block_number: None,
        };
        let cached: HashSet<B256> = HashSet::from([hash_a, hash_c]);
        let compact = witness.compact(&cached);
        assert_eq!(compact.uncached_codes.len(), 1);
        assert_eq!(compact.uncached_codes[0], code_b);
        assert_eq!(compact.cached_code_hashes.len(), 2);
    }

    #[test]
    fn test_execution_witness_serde_json_roundtrip() {
        let witness = ExecutionWitness {
            hashed_state: HashedPostState::default(),
            codes: vec![bytecode(&[0xAA, 0xBB]), bytecode(&[0xCC])],
            keys: vec![bytecode(&[1, 2, 3])],
            lowest_block_number: Some(42),
        };

        let json = serde_json::to_string(&witness).expect("json serialize");
        let deserialized: ExecutionWitness =
            serde_json::from_str(&json).expect("json deserialize");

        assert_eq!(deserialized.codes.len(), 2);
        assert_eq!(deserialized.keys.len(), 1);
        assert_eq!(deserialized.lowest_block_number, Some(42));
    }

    #[test]
    fn test_execution_witness_bincode_roundtrip() {
        let witness = ExecutionWitness {
            hashed_state: HashedPostState::default(),
            codes: vec![bytecode(&[0xDE, 0xAD])],
            keys: vec![],
            lowest_block_number: None,
        };

        let encoded = bincode::serialize(&witness).expect("bincode serialize");
        let decoded: ExecutionWitness =
            bincode::deserialize(&encoded).expect("bincode deserialize");

        assert_eq!(decoded.codes.len(), 1);
        assert_eq!(decoded.codes[0], bytecode(&[0xDE, 0xAD]));
        assert!(decoded.lowest_block_number.is_none());
    }

    #[test]
    fn test_compact_witness_serde_json_roundtrip() {
        let compact = CompactWitness {
            hashed_state: HashedPostState::default(),
            uncached_codes: vec![bytecode(&[0xFF])],
            cached_code_hashes: vec![keccak256(&[0xAA])],
            keys: vec![bytecode(&[4, 5, 6])],
            lowest_block_number: Some(100),
        };

        let json = serde_json::to_string(&compact).expect("json serialize");
        let deserialized: CompactWitness =
            serde_json::from_str(&json).expect("json deserialize");

        assert_eq!(deserialized.uncached_codes.len(), 1);
        assert_eq!(deserialized.cached_code_hashes.len(), 1);
        assert_eq!(deserialized.lowest_block_number, Some(100));
    }

    #[test]
    fn test_compact_witness_bincode_roundtrip() {
        let compact = CompactWitness {
            hashed_state: HashedPostState::default(),
            uncached_codes: vec![],
            cached_code_hashes: vec![],
            keys: vec![bytecode(&[7, 8, 9])],
            lowest_block_number: None,
        };

        let encoded = bincode::serialize(&compact).expect("bincode serialize");
        let decoded: CompactWitness =
            bincode::deserialize(&encoded).expect("bincode deserialize");

        assert_eq!(decoded.keys.len(), 1);
        assert!(decoded.uncached_codes.is_empty());
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

    #[test]
    fn test_compact_deduplicates_cached_hashes() {
        let code = bytecode(&[0xAA, 0xBB, 0xCC]);
        let hash = keccak256(&code);
        let witness = ExecutionWitness {
            hashed_state: HashedPostState::default(),
            codes: vec![code.clone(), code.clone(), code],
            keys: vec![],
            lowest_block_number: None,
        };
        let cached: HashSet<B256> = HashSet::from([hash]);
        let compact = witness.compact(&cached);
        assert!(compact.uncached_codes.is_empty());
        assert_eq!(compact.cached_code_hashes.len(), 1);
        assert_eq!(compact.cached_code_hashes[0], hash);
    }

    #[test]
    fn test_total_code_bytes() {
        let witness = ExecutionWitness {
            hashed_state: HashedPostState::default(),
            codes: vec![
                bytecode(&[0xAA; 100]),
                bytecode(&[0xBB; 200]),
                bytecode(&[0xCC; 50]),
            ],
            keys: vec![],
            lowest_block_number: None,
        };
        assert_eq!(witness.total_code_bytes(), 350);

        let empty = ExecutionWitness::default();
        assert_eq!(empty.total_code_bytes(), 0);
    }

    #[test]
    fn test_compact_empty_witness() {
        let witness = ExecutionWitness::default();
        let compact = witness.compact(&HashSet::new());
        assert!(compact.uncached_codes.is_empty());
        assert!(compact.cached_code_hashes.is_empty());
        assert!(compact.keys.is_empty());
        assert!(compact.lowest_block_number.is_none());
    }
}
