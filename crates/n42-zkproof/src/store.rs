use alloy_primitives::B256;
use metrics::gauge;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

use crate::prover::ZkProofResult;

/// Generation statistics for the ZK proof subsystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProofStats {
    pub generated: u64,
    pub failed: u64,
}

/// In-memory store for generated ZK proofs, indexed by block number and hash.
///
/// Thread-safe via `RwLock`. Bounded by `max_entries` with oldest-first eviction.
/// Hash lookups are O(1) via a secondary `HashMap<B256, u64>` index.
pub struct ProofStore {
    proofs: RwLock<StoreInner>,
    max_entries: usize,
    total_generated: AtomicU64,
    total_failed: AtomicU64,
}

struct StoreInner {
    by_block: BTreeMap<u64, ZkProofResult>,
    hash_to_block: HashMap<B256, u64>,
}

impl ProofStore {
    pub fn new(max_entries: usize) -> Self {
        Self {
            proofs: RwLock::new(StoreInner {
                by_block: BTreeMap::new(),
                hash_to_block: HashMap::new(),
            }),
            max_entries: max_entries.max(1),
            total_generated: AtomicU64::new(0),
            total_failed: AtomicU64::new(0),
        }
    }

    /// Insert a proof result. Evicts the oldest entry if at capacity.
    pub fn insert(&self, result: ZkProofResult) {
        let mut inner = self.proofs.write().unwrap_or_else(|e| e.into_inner());
        let block_number = result.block_number;
        let block_hash = result.block_hash;
        let is_overwrite = inner.by_block.contains_key(&block_number);

        // Only count as new generation if not overwriting an existing proof.
        if !is_overwrite {
            self.total_generated.fetch_add(1, Ordering::Relaxed);
        }

        // Remove old hash index if overwriting the same block number.
        if let Some(old_hash) = inner.by_block.get(&block_number).map(|p| p.block_hash) {
            inner.hash_to_block.remove(&old_hash);
        }

        inner.by_block.insert(block_number, result);
        inner.hash_to_block.insert(block_hash, block_number);

        // Evict oldest entries if over capacity.
        while inner.by_block.len() > self.max_entries {
            if let Some(&oldest_key) = inner.by_block.keys().next()
                && let Some(evicted) = inner.by_block.remove(&oldest_key)
            {
                inner.hash_to_block.remove(&evicted.block_hash);
            }
        }
        gauge!("n42_zk_proof_store_size").set(inner.by_block.len() as f64);
    }

    /// Record a failed proof generation attempt.
    pub fn record_failure(&self) {
        self.total_failed.fetch_add(1, Ordering::Relaxed);
    }

    /// Returns generation statistics.
    pub fn stats(&self) -> ProofStats {
        ProofStats {
            generated: self.total_generated.load(Ordering::Relaxed),
            failed: self.total_failed.load(Ordering::Relaxed),
        }
    }

    /// Look up a proof by block number.
    pub fn get_by_block(&self, number: u64) -> Option<ZkProofResult> {
        let inner = self.proofs.read().unwrap_or_else(|e| e.into_inner());
        inner.by_block.get(&number).cloned()
    }

    /// Look up a proof by block hash (O(1) via hash index).
    pub fn get_by_hash(&self, hash: &B256) -> Option<ZkProofResult> {
        let inner = self.proofs.read().unwrap_or_else(|e| e.into_inner());
        let block_number = inner.hash_to_block.get(hash)?;
        inner.by_block.get(block_number).cloned()
    }

    /// Returns the most recently generated proof (highest block number).
    pub fn latest(&self) -> Option<ZkProofResult> {
        let inner = self.proofs.read().unwrap_or_else(|e| e.into_inner());
        inner.by_block.values().next_back().cloned()
    }

    /// Returns the total number of stored proofs.
    pub fn len(&self) -> usize {
        let inner = self.proofs.read().unwrap_or_else(|e| e.into_inner());
        inner.by_block.len()
    }

    /// Returns true if the store is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns true if a proof exists for the given block number.
    pub fn contains(&self, block_number: u64) -> bool {
        let inner = self.proofs.read().unwrap_or_else(|e| e.into_inner());
        inner.by_block.contains_key(&block_number)
    }

    /// Returns up to `limit` proofs starting from `from_block` (inclusive),
    /// ordered by block number ascending.
    pub fn list(&self, from_block: u64, limit: usize) -> Vec<ZkProofResult> {
        let inner = self.proofs.read().unwrap_or_else(|e| e.into_inner());
        inner
            .by_block
            .range(from_block..)
            .take(limit)
            .map(|(_, v)| v.clone())
            .collect()
    }

    /// Returns the (min, max) block number range of stored proofs, or None if empty.
    pub fn block_range(&self) -> Option<(u64, u64)> {
        let inner = self.proofs.read().unwrap_or_else(|e| e.into_inner());
        let min = inner.by_block.keys().next().copied()?;
        let max = inner.by_block.keys().next_back().copied()?;
        Some((min, max))
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
            created_at: 0,
        }
    }

    fn make_result_with_hash(block_number: u64, hash_byte: u8) -> ZkProofResult {
        ZkProofResult {
            block_hash: B256::repeat_byte(hash_byte),
            block_number,
            proof_bytes: vec![1, 2, 3],
            public_values: vec![4, 5, 6],
            proof_type: ProofType::Mock,
            prover_backend: "mock".to_string(),
            generation_ms: 10,
            verified: true,
            created_at: 0,
        }
    }

    // --- Basic CRUD ---

    #[test]
    fn test_insert_and_get_by_block() {
        let store = ProofStore::new(10);
        store.insert(make_result(100));
        assert_eq!(store.len(), 1);

        let proof = store.get_by_block(100).unwrap();
        assert_eq!(proof.block_number, 100);
    }

    #[test]
    fn test_insert_and_get_by_hash() {
        let store = ProofStore::new(10);
        let result = make_result(100);
        let hash = result.block_hash;
        store.insert(result);

        let proof = store.get_by_hash(&hash).unwrap();
        assert_eq!(proof.block_number, 100);
    }

    #[test]
    fn test_get_nonexistent_block() {
        let store = ProofStore::new(10);
        assert!(store.get_by_block(999).is_none());
    }

    #[test]
    fn test_get_nonexistent_hash() {
        let store = ProofStore::new(10);
        assert!(store.get_by_hash(&B256::ZERO).is_none());
    }

    // --- Contains ---

    #[test]
    fn test_contains() {
        let store = ProofStore::new(10);
        assert!(!store.contains(100));

        store.insert(make_result(100));
        assert!(store.contains(100));
        assert!(!store.contains(101));
    }

    // --- Latest ---

    #[test]
    fn test_latest_empty() {
        let store = ProofStore::new(10);
        assert!(store.latest().is_none());
    }

    #[test]
    fn test_latest_single() {
        let store = ProofStore::new(10);
        store.insert(make_result(42));
        assert_eq!(store.latest().unwrap().block_number, 42);
    }

    #[test]
    fn test_latest_out_of_order_inserts() {
        let store = ProofStore::new(10);
        store.insert(make_result(10));
        store.insert(make_result(20));
        store.insert(make_result(15));

        // BTreeMap is ordered, so latest is always highest block number.
        assert_eq!(store.latest().unwrap().block_number, 20);
    }

    // --- Block range ---

    #[test]
    fn test_block_range_empty() {
        let store = ProofStore::new(10);
        assert!(store.block_range().is_none());
    }

    #[test]
    fn test_block_range_single() {
        let store = ProofStore::new(10);
        store.insert(make_result(50));
        assert_eq!(store.block_range(), Some((50, 50)));
    }

    #[test]
    fn test_block_range_multiple() {
        let store = ProofStore::new(10);
        store.insert(make_result(10));
        store.insert(make_result(30));
        store.insert(make_result(20));
        assert_eq!(store.block_range(), Some((10, 30)));
    }

    // --- LRU eviction ---

    #[test]
    fn test_eviction_removes_oldest() {
        let store = ProofStore::new(3);
        store.insert(make_result(1));
        store.insert(make_result(2));
        store.insert(make_result(3));
        assert_eq!(store.len(), 3);

        store.insert(make_result(4));
        assert_eq!(store.len(), 3);
        assert!(!store.contains(1), "block 1 should be evicted");
        assert!(store.contains(2));
        assert!(store.contains(3));
        assert!(store.contains(4));
    }

    #[test]
    fn test_eviction_cascade() {
        let store = ProofStore::new(2);
        store.insert(make_result(1));
        store.insert(make_result(2));
        store.insert(make_result(3));
        store.insert(make_result(4));

        assert_eq!(store.len(), 2);
        assert!(store.contains(3));
        assert!(store.contains(4));
        assert!(!store.contains(1));
        assert!(!store.contains(2));
    }

    #[test]
    fn test_capacity_one() {
        let store = ProofStore::new(1);
        store.insert(make_result(10));
        assert_eq!(store.len(), 1);

        store.insert(make_result(20));
        assert_eq!(store.len(), 1);
        assert!(!store.contains(10));
        assert!(store.contains(20));
    }

    // --- Overwrite same block number ---

    #[test]
    fn test_overwrite_same_block_number() {
        let store = ProofStore::new(10);
        store.insert(make_result_with_hash(100, 0xAA));
        store.insert(make_result_with_hash(100, 0xBB));

        assert_eq!(store.len(), 1);
        let proof = store.get_by_block(100).unwrap();
        assert_eq!(proof.block_hash, B256::repeat_byte(0xBB));
    }

    // --- Empty / is_empty ---

    #[test]
    fn test_empty_store() {
        let store = ProofStore::new(10);
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);

        store.insert(make_result(1));
        assert!(!store.is_empty());
        assert_eq!(store.len(), 1);
    }

    // --- Concurrent access ---

    #[test]
    fn test_concurrent_insert_and_read() {
        use std::sync::Arc;
        use std::thread;

        let store = Arc::new(ProofStore::new(1000));
        let mut handles = vec![];

        // 10 writer threads, each inserting 100 proofs.
        for t in 0..10u64 {
            let s = Arc::clone(&store);
            handles.push(thread::spawn(move || {
                for i in 0..100u64 {
                    s.insert(make_result(t * 1000 + i));
                }
            }));
        }

        // 5 reader threads, querying continuously.
        for _ in 0..5 {
            let s = Arc::clone(&store);
            handles.push(thread::spawn(move || {
                for i in 0..500u64 {
                    let _ = s.get_by_block(i);
                    let _ = s.latest();
                    let _ = s.len();
                    let _ = s.block_range();
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        // All 1000 unique blocks should be stored (capacity=1000).
        assert_eq!(store.len(), 1000);
    }

    // --- Capacity 0 clamped to 1 ---

    #[test]
    fn test_capacity_zero_clamped() {
        let store = ProofStore::new(0);
        store.insert(make_result(1));
        // max_entries clamped to 1, so store holds exactly one entry.
        assert_eq!(store.len(), 1);
        assert!(store.contains(1));

        store.insert(make_result(2));
        assert_eq!(store.len(), 1);
        assert!(!store.contains(1));
        assert!(store.contains(2));
    }

    // --- get_by_hash with multiple proofs ---

    #[test]
    fn test_get_by_hash_correct_match() {
        let store = ProofStore::new(10);
        store.insert(make_result_with_hash(1, 0xAA));
        store.insert(make_result_with_hash(2, 0xBB));
        store.insert(make_result_with_hash(3, 0xCC));

        let proof = store.get_by_hash(&B256::repeat_byte(0xBB)).unwrap();
        assert_eq!(proof.block_number, 2);

        assert!(store.get_by_hash(&B256::repeat_byte(0xFF)).is_none());
    }

    // --- Hash index consistency ---

    #[test]
    fn test_hash_index_cleaned_on_eviction() {
        let store = ProofStore::new(2);
        let r1 = make_result_with_hash(1, 0xAA);
        let hash1 = r1.block_hash;
        store.insert(r1);
        store.insert(make_result_with_hash(2, 0xBB));
        store.insert(make_result_with_hash(3, 0xCC));

        // Block 1 evicted — its hash should no longer resolve.
        assert!(store.get_by_hash(&hash1).is_none());
        // Block 3 should resolve via hash index.
        assert_eq!(store.get_by_hash(&B256::repeat_byte(0xCC)).unwrap().block_number, 3);
    }

    #[test]
    fn test_hash_index_updated_on_overwrite() {
        let store = ProofStore::new(10);
        let old_hash = B256::repeat_byte(0xAA);
        let new_hash = B256::repeat_byte(0xBB);
        store.insert(make_result_with_hash(100, 0xAA));
        store.insert(make_result_with_hash(100, 0xBB));

        // Old hash should not resolve.
        assert!(store.get_by_hash(&old_hash).is_none());
        // New hash should resolve.
        assert_eq!(store.get_by_hash(&new_hash).unwrap().block_number, 100);
    }

    // --- Stats ---

    #[test]
    fn test_stats_initial() {
        let store = ProofStore::new(10);
        let stats = store.stats();
        assert_eq!(stats.generated, 0);
        assert_eq!(stats.failed, 0);
    }

    #[test]
    fn test_stats_after_inserts_and_failures() {
        let store = ProofStore::new(10);
        store.insert(make_result(1));
        store.insert(make_result(2));
        store.record_failure();
        store.insert(make_result(3));
        store.record_failure();
        store.record_failure();

        let stats = store.stats();
        assert_eq!(stats.generated, 3);
        assert_eq!(stats.failed, 3);
    }

    #[test]
    fn test_stats_overwrite_not_double_counted() {
        let store = ProofStore::new(10);
        store.insert(make_result_with_hash(100, 0xAA));
        assert_eq!(store.stats().generated, 1);

        // Overwrite same block_number with different hash — should NOT increment counter.
        store.insert(make_result_with_hash(100, 0xBB));
        assert_eq!(store.stats().generated, 1);
        assert_eq!(store.len(), 1);
    }

    // --- List pagination ---

    #[test]
    fn test_list_basic() {
        let store = ProofStore::new(10);
        for i in [10, 20, 30, 40, 50] {
            store.insert(make_result(i));
        }

        let all = store.list(0, 100);
        assert_eq!(all.len(), 5);
        assert_eq!(all[0].block_number, 10);
        assert_eq!(all[4].block_number, 50);
    }

    #[test]
    fn test_list_with_offset_and_limit() {
        let store = ProofStore::new(10);
        for i in [10, 20, 30, 40, 50] {
            store.insert(make_result(i));
        }

        let page = store.list(25, 2);
        assert_eq!(page.len(), 2);
        assert_eq!(page[0].block_number, 30);
        assert_eq!(page[1].block_number, 40);
    }

    #[test]
    fn test_list_empty_store() {
        let store = ProofStore::new(10);
        assert!(store.list(0, 10).is_empty());
    }
}
