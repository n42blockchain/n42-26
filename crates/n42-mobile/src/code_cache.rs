use alloy_primitives::{Bytes, B256};
use lru::LruCache;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::num::NonZeroUsize;

/// LRU cache for high-frequency contract bytecodes on mobile devices.
///
/// Mobile verifiers cache commonly-used contract bytecodes locally
/// (ERC-20, DEX routers, etc.) to reduce verification packet sizes.
/// The IDC node tracks which codes each phone has cached and excludes
/// them from the `VerificationPacket.uncached_bytecodes` field.
///
/// Cache keys are `keccak256(bytecode)` (the code hash).
pub struct CodeCache {
    /// LRU cache: code_hash → bytecode.
    cache: LruCache<B256, Bytes>,
}

impl CodeCache {
    /// Creates a new code cache with the given capacity.
    ///
    /// Typical capacity for mobile devices: 500-2000 contracts.
    pub fn new(capacity: usize) -> Self {
        Self {
            cache: LruCache::new(
                NonZeroUsize::new(capacity).expect("cache capacity must be > 0"),
            ),
        }
    }

    /// Inserts a bytecode into the cache.
    pub fn insert(&mut self, code_hash: B256, bytecode: Bytes) {
        self.cache.put(code_hash, bytecode);
    }

    /// Retrieves a bytecode from the cache (marks as recently used).
    pub fn get(&mut self, code_hash: &B256) -> Option<&Bytes> {
        self.cache.get(code_hash)
    }

    /// Checks if a code hash is in the cache without updating LRU order.
    pub fn contains(&self, code_hash: &B256) -> bool {
        self.cache.contains(code_hash)
    }

    /// Returns the set of all cached code hashes.
    ///
    /// Used by the IDC node to determine which bytecodes to exclude
    /// from the verification packet for this phone.
    pub fn cached_hashes(&self) -> Vec<B256> {
        self.cache.iter().map(|(k, _)| *k).collect()
    }

    /// Returns the number of cached entries.
    pub fn len(&self) -> usize {
        self.cache.len()
    }

    /// Returns true if the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }

    /// Clears all cached entries.
    pub fn clear(&mut self) {
        self.cache.clear();
    }
}

/// Tracks hot contract usage across all mobile verifiers.
///
/// The IDC node maintains this tracker to identify frequently-accessed
/// contracts and proactively push their bytecodes to phones during
/// cache sync, even before those contracts appear in a verification packet.
pub struct HotContractTracker {
    /// Cumulative access count per code hash.
    access_counts: HashMap<B256, u64>,
    /// Number of blocks tracked.
    blocks_tracked: u64,
}

impl HotContractTracker {
    /// Creates a new hot contract tracker.
    pub fn new() -> Self {
        Self {
            access_counts: HashMap::new(),
            blocks_tracked: 0,
        }
    }

    /// Records contract accesses from a block's execution witness.
    ///
    /// Call this after each block execution with the set of code hashes
    /// that were accessed during execution.
    pub fn record_block_accesses(&mut self, code_hashes: &[B256]) {
        self.blocks_tracked += 1;
        for hash in code_hashes {
            *self.access_counts.entry(*hash).or_insert(0) += 1;
        }
    }

    /// Returns the top-N hottest contracts by access count.
    pub fn top_contracts(&self, n: usize) -> Vec<(B256, u64)> {
        let mut sorted: Vec<_> = self.access_counts.iter().map(|(k, v)| (*k, *v)).collect();
        sorted.sort_by(|a, b| b.1.cmp(&a.1));
        sorted.truncate(n);
        sorted
    }

    /// Returns the total number of blocks tracked.
    pub fn blocks_tracked(&self) -> u64 {
        self.blocks_tracked
    }

    /// Decays all access counts by the given factor (0.0 - 1.0).
    ///
    /// Call periodically (e.g., every epoch) to prevent stale contracts
    /// from occupying top positions indefinitely.
    pub fn decay(&mut self, factor: f64) {
        let factor = factor.clamp(0.0, 1.0);
        self.access_counts.retain(|_, count| {
            *count = (*count as f64 * factor) as u64;
            *count > 0
        });
    }
}

impl Default for HotContractTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Cache synchronization message sent from IDC to mobile devices.
///
/// Periodically, the IDC pushes hot contract bytecodes to phones
/// so they can pre-populate their caches before verification packets arrive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheSyncMessage {
    /// Bytecodes to add to the cache: (code_hash, bytecode).
    pub codes: Vec<(B256, Bytes)>,
    /// Code hashes that are no longer hot and can be evicted.
    pub evict_hints: Vec<B256>,
}
