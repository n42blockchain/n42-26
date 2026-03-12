use alloy_primitives::B256;
use std::collections::BTreeMap;
use std::sync::RwLock;

use crate::prover::ZkProofResult;

/// In-memory store for generated ZK proofs, indexed by block number.
///
/// Thread-safe via `RwLock`. Bounded by `max_entries` with oldest-first eviction.
pub struct ProofStore {
    proofs: RwLock<BTreeMap<u64, ZkProofResult>>,
    max_entries: usize,
}

impl ProofStore {
    pub fn new(max_entries: usize) -> Self {
        Self {
            proofs: RwLock::new(BTreeMap::new()),
            max_entries,
        }
    }

    /// Insert a proof result. Evicts the oldest entry if at capacity.
    pub fn insert(&self, result: ZkProofResult) {
        let mut proofs = self.proofs.write().unwrap_or_else(|e| e.into_inner());
        proofs.insert(result.block_number, result);
        // Evict oldest entries if over capacity.
        while proofs.len() > self.max_entries {
            if let Some(&oldest_key) = proofs.keys().next() {
                proofs.remove(&oldest_key);
            }
        }
    }

    /// Look up a proof by block number.
    pub fn get_by_block(&self, number: u64) -> Option<ZkProofResult> {
        let proofs = self.proofs.read().unwrap_or_else(|e| e.into_inner());
        proofs.get(&number).cloned()
    }

    /// Look up a proof by block hash.
    pub fn get_by_hash(&self, hash: &B256) -> Option<ZkProofResult> {
        let proofs = self.proofs.read().unwrap_or_else(|e| e.into_inner());
        proofs.values().find(|p| p.block_hash == *hash).cloned()
    }

    /// Returns the most recently generated proof (highest block number).
    pub fn latest(&self) -> Option<ZkProofResult> {
        let proofs = self.proofs.read().unwrap_or_else(|e| e.into_inner());
        proofs.values().next_back().cloned()
    }

    /// Returns the total number of stored proofs.
    pub fn len(&self) -> usize {
        let proofs = self.proofs.read().unwrap_or_else(|e| e.into_inner());
        proofs.len()
    }

    /// Returns true if the store is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prover::ProofType;

    fn make_result(block_number: u64) -> ZkProofResult {
        let mut hash_bytes = [0u8; 32];
        hash_bytes[..8].copy_from_slice(&block_number.to_le_bytes());
        ZkProofResult {
            block_hash: B256::from(hash_bytes),
            block_number,
            proof_bytes: vec![1, 2, 3],
            public_values: vec![4, 5, 6],
            proof_type: ProofType::Mock,
            prover_backend: "mock".to_string(),
            generation_ms: 10,
            verified: true,
        }
    }

    #[test]
    fn test_insert_and_get() {
        let store = ProofStore::new(10);
        let result = make_result(100);
        let hash = result.block_hash;

        store.insert(result);
        assert_eq!(store.len(), 1);

        let by_block = store.get_by_block(100).unwrap();
        assert_eq!(by_block.block_number, 100);

        let by_hash = store.get_by_hash(&hash).unwrap();
        assert_eq!(by_hash.block_number, 100);
    }

    #[test]
    fn test_latest() {
        let store = ProofStore::new(10);
        assert!(store.latest().is_none());

        store.insert(make_result(10));
        store.insert(make_result(20));
        store.insert(make_result(15));

        let latest = store.latest().unwrap();
        assert_eq!(latest.block_number, 20);
    }

    #[test]
    fn test_lru_eviction() {
        let store = ProofStore::new(3);
        store.insert(make_result(1));
        store.insert(make_result(2));
        store.insert(make_result(3));
        assert_eq!(store.len(), 3);

        // Inserting a 4th should evict the oldest (block 1).
        store.insert(make_result(4));
        assert_eq!(store.len(), 3);
        assert!(store.get_by_block(1).is_none(), "block 1 should be evicted");
        assert!(store.get_by_block(2).is_some());
        assert!(store.get_by_block(3).is_some());
        assert!(store.get_by_block(4).is_some());
    }

    #[test]
    fn test_get_nonexistent() {
        let store = ProofStore::new(10);
        assert!(store.get_by_block(999).is_none());
        assert!(store.get_by_hash(&B256::ZERO).is_none());
    }
}
