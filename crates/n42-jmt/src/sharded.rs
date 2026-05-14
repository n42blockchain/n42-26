use crate::hasher::Blake3Hasher;
use crate::keys::{account_key, storage_key};
use crate::metrics;
use crate::store::{MemTreeStore, TreeStore};
use crate::tree::{N42JmtTree, decode_code_hash, encode_account_value};

use alloy_primitives::B256;
use jmt::{KeyHash, OwnedValue, Version};
use n42_execution::state_diff::{AccountChangeType, StateDiff};
use parking_lot::Mutex;
use rayon::prelude::*;
use std::sync::Arc;
use std::time::Instant;
use tracing::debug;

/// Number of shards. Using 16 (first nibble of key hash) for even distribution.
pub const SHARD_COUNT: usize = 16;

/// Assigns a `KeyHash` to a shard by its first nibble (upper 4 bits of first byte).
#[inline]
fn shard_index(key: &KeyHash) -> usize {
    (key.0[0] >> 4) as usize
}

/// Compute the combined root hash from an array of shard roots.
///
/// `blake3(shard_0_root || shard_1_root || ... || shard_15_root)`
///
/// Shared by `ShardedJmt::root_hash()`, `apply_diff()`, and `JmtProof::verify()`.
pub fn combine_shard_roots(roots: &[[u8; 32]]) -> B256 {
    let mut hasher = blake3::Hasher::new();
    for root in roots {
        hasher.update(root);
    }
    B256::from(*hasher.finalize().as_bytes())
}

/// 16-shard parallel JMT.
///
/// Each shard is an independent `N42JmtTree` with its own store.
/// Updates are partitioned by the first nibble of the key hash and applied
/// in parallel via rayon. The final root is the Blake3 hash of all 16 shard roots
/// concatenated in order.
///
/// This gives ~10-14x speedup on multi-core machines for large state diffs,
/// while maintaining deterministic root computation.
///
/// Generic over `S: TreeStore`; defaults to `MemTreeStore` for backward compatibility.
pub struct ShardedJmt<S: TreeStore = MemTreeStore> {
    shards: Vec<Mutex<N42JmtTree<S>>>,
    version: Version,
}

impl ShardedJmt<MemTreeStore> {
    /// Create a new 16-shard JMT with in-memory stores.
    pub fn new() -> Self {
        let shards = (0..SHARD_COUNT)
            .map(|_| Mutex::new(N42JmtTree::with_store(Arc::new(MemTreeStore::new()))))
            .collect();
        Self { shards, version: 0 }
    }
}

impl<S: TreeStore> ShardedJmt<S> {
    /// Construct from pre-built shards (used by snapshot restore).
    pub(crate) fn from_parts(shards: Vec<Mutex<N42JmtTree<S>>>, version: Version) -> Self {
        assert_eq!(shards.len(), SHARD_COUNT);
        Self { shards, version }
    }

    /// Access the internal shards (used by snapshot).
    pub(crate) fn shards(&self) -> &[Mutex<N42JmtTree<S>>] {
        &self.shards
    }

    /// Current global version.
    pub fn version(&self) -> Version {
        self.version
    }

    /// Collect all 16 shard root hashes (for proof construction).
    pub fn shard_roots(&self) -> eyre::Result<Vec<[u8; 32]>> {
        let mut roots = Vec::with_capacity(SHARD_COUNT);
        for shard in &self.shards {
            let tree = shard.lock();
            let root = tree.root_hash()?;
            roots.push(root.0);
        }
        Ok(roots)
    }

    /// Compute the combined root hash from all 16 shard roots.
    ///
    /// `combined_root = blake3(shard_0_root || shard_1_root || ... || shard_15_root)`
    pub fn root_hash(&self) -> eyre::Result<B256> {
        let roots = self.shard_roots()?;
        Ok(combine_shard_roots(&roots))
    }

    /// Read existing code_hashes and partition a StateDiff into per-shard update vectors.
    ///
    /// Shared by `apply_diff` and `apply_diff_atomic` to avoid duplication.
    /// Returns `(per_shard_updates, total_key_count)`.
    #[allow(clippy::type_complexity)]
    fn prepare_shard_updates(
        &self,
        diff: &StateDiff,
    ) -> eyre::Result<(Vec<Vec<(KeyHash, Option<OwnedValue>)>>, usize)> {
        // Phase 1: Read existing code_hashes for Modified accounts that need them.
        let mut existing_code_hashes: std::collections::HashMap<alloy_primitives::Address, B256> =
            std::collections::HashMap::new();
        for (address, account_diff) in &diff.accounts {
            if account_diff.change_type == AccountChangeType::Modified
                && account_diff.code_change.is_none()
            {
                let key = account_key(address);
                let si = shard_index(&key);
                let tree = self.shards[si].lock();
                if let Some(val) = tree.get(key)? {
                    existing_code_hashes.insert(*address, decode_code_hash(&val));
                }
            }
        }

        // Phase 2: Partition updates by shard.
        let mut shard_updates: Vec<Vec<(KeyHash, Option<OwnedValue>)>> =
            (0..SHARD_COUNT).map(|_| Vec::new()).collect();
        let mut total_keys = 0usize;

        for (address, account_diff) in &diff.accounts {
            let key = account_key(address);
            let si = shard_index(&key);

            match account_diff.change_type {
                AccountChangeType::Destroyed => {
                    shard_updates[si].push((key, None));
                    total_keys += 1;
                    for slot in account_diff.storage.keys() {
                        let skey = storage_key(address, slot);
                        let ssi = shard_index(&skey);
                        shard_updates[ssi].push((skey, None));
                        total_keys += 1;
                    }
                }
                AccountChangeType::Created | AccountChangeType::Modified => {
                    let balance = account_diff
                        .balance
                        .as_ref()
                        .map(|v| v.to)
                        .unwrap_or_default();
                    let nonce = account_diff.nonce.as_ref().map(|v| v.to).unwrap_or(0);
                    let code_hash = match &account_diff.code_change {
                        Some(change) => change.to.unwrap_or(B256::ZERO),
                        None => existing_code_hashes
                            .get(address)
                            .copied()
                            .unwrap_or(B256::ZERO),
                    };
                    let value = encode_account_value(&balance, nonce, &code_hash);
                    shard_updates[si].push((key, Some(value)));
                    total_keys += 1;

                    for (slot, change) in &account_diff.storage {
                        let skey = storage_key(address, slot);
                        let ssi = shard_index(&skey);
                        if change.to.is_zero() {
                            shard_updates[ssi].push((skey, None));
                        } else {
                            shard_updates[ssi]
                                .push((skey, Some(change.to.to_be_bytes::<32>().to_vec())));
                        }
                        total_keys += 1;
                    }
                }
            }
        }

        Ok((shard_updates, total_keys))
    }

    /// Apply a `StateDiff` across all 16 shards in parallel.
    ///
    /// For Modified accounts without `code_change`, reads existing code_hash
    /// from the shard to preserve it (see P0-2 fix).
    ///
    /// Returns `(new_version, combined_root_hash)`.
    pub fn apply_diff(&mut self, diff: &StateDiff) -> eyre::Result<(Version, B256)> {
        let start = Instant::now();
        let new_version = self.version + 1;
        let (shard_updates, total_keys) = self.prepare_shard_updates(diff)?;

        // Phase 3: Apply each shard's updates in parallel.
        let results: Vec<eyre::Result<B256>> = self
            .shards
            .par_iter()
            .zip(shard_updates.into_par_iter())
            .map(|(shard, updates)| {
                let mut tree = shard.lock();
                let (_, root) = tree.apply_batch(updates)?;
                Ok(root)
            })
            .collect();

        // Phase 4: Combine shard roots.
        let mut shard_roots = Vec::with_capacity(SHARD_COUNT);
        for result in results {
            let root = result?;
            shard_roots.push(root.0);
        }
        let combined = combine_shard_roots(&shard_roots);
        self.version = new_version;

        let ms = start.elapsed().as_secs_f64() * 1000.0;
        metrics::record_sharded_update(SHARD_COUNT, total_keys, ms);
        debug!(
            target: "n42::jmt",
            version = new_version,
            shards = SHARD_COUNT,
            total_keys,
            update_ms = format!("{ms:.2}"),
            root = %combined,
            "sharded JMT updated"
        );

        Ok((new_version, combined))
    }

    /// Generate proof for a key. Identifies the correct shard automatically.
    ///
    /// Returns `None` if the target shard is empty (key guaranteed absent).
    pub fn get_proof(
        &self,
        key_hash: KeyHash,
    ) -> eyre::Result<Option<jmt::proof::SparseMerkleProof<Blake3Hasher>>> {
        let start = Instant::now();
        let idx = shard_index(&key_hash);
        let tree = self.shards[idx].lock();
        let proof = tree.get_proof(key_hash)?;
        let ms = start.elapsed().as_secs_f64() * 1000.0;

        if proof.is_some() {
            let est_size = 32 * 10;
            metrics::record_proof_generation(est_size, ms);
        }

        Ok(proof)
    }

    /// Get the value for a key at the current version.
    pub fn get(&self, key_hash: KeyHash) -> eyre::Result<Option<OwnedValue>> {
        let idx = shard_index(&key_hash);
        let tree = self.shards[idx].lock();
        tree.get(key_hash)
    }

    /// Get per-shard node and key counts (for diagnostics).
    pub fn shard_stats(&self) -> Vec<(usize, usize)> {
        self.shards
            .iter()
            .map(|s| {
                let tree = s.lock();
                let store = tree.store();
                (store.node_count(), store.key_count())
            })
            .collect()
    }

    /// Prune old versions across all shards to reclaim memory.
    pub fn prune(&self, keep_versions: u64) {
        for shard in &self.shards {
            let tree = shard.lock();
            let store = tree.store();
            store.prune(tree.version(), keep_versions);
        }
    }
}

impl Default for ShardedJmt<MemTreeStore> {
    fn default() -> Self {
        Self::new()
    }
}

impl ShardedJmt<crate::DiskTreeStore> {
    /// Open a disk-backed 16-shard JMT from an MDBX environment directory.
    ///
    /// Creates or reopens the MDBX environment at `path`, with one `DiskTreeStore`
    /// per shard. Restores each shard's version from metadata so the tree can
    /// resume from where it left off.
    pub fn open_disk(
        path: impl AsRef<std::path::Path>,
        cache_size_per_shard: usize,
    ) -> eyre::Result<Self> {
        let env = crate::disk_store::open_jmt_env(path)?;

        let shards = (0..SHARD_COUNT)
            .map(|i| {
                let store = Arc::new(crate::DiskTreeStore::open(
                    env.clone(),
                    i as u8,
                    cache_size_per_shard,
                )?);
                let version = store.read_latest_version();
                Ok(Mutex::new(N42JmtTree::with_store_and_version(
                    store, version,
                )))
            })
            .collect::<eyre::Result<Vec<_>>>()?;

        let version = shards.iter().map(|s| s.lock().version()).max().unwrap_or(0);
        Ok(Self { shards, version })
    }

    /// Apply a `StateDiff` with atomic single-transaction MDBX commit.
    ///
    /// Same logic as `apply_diff`, but:
    /// - Phase 3 computes `NodeBatch` per shard **without writing** (parallel via rayon)
    /// - Phase 4 writes all 16 shards in a **single MDBX write transaction** (atomic)
    ///
    /// This avoids MDBX's single-writer serialization during parallel computation
    /// and guarantees all-or-nothing commit across shards.
    pub fn apply_diff_atomic(&mut self, diff: &StateDiff) -> eyre::Result<(Version, B256)> {
        let start = Instant::now();
        let new_version = self.version + 1;
        let (shard_updates, total_keys) = self.prepare_shard_updates(diff)?;

        // Phase 3: Compute NodeBatch per shard in parallel (NO disk write).
        let compute_results: Vec<eyre::Result<(Version, B256, jmt::storage::NodeBatch)>> = self
            .shards
            .par_iter()
            .zip(shard_updates.into_par_iter())
            .map(|(shard, updates)| {
                let tree = shard.lock();
                tree.compute_batch(updates)
            })
            .collect();

        // Collect results, bail on first error.
        let mut batches = Vec::with_capacity(SHARD_COUNT);
        let mut shard_roots = Vec::with_capacity(SHARD_COUNT);
        for result in compute_results {
            let (ver, root, batch) = result?;
            batches.push((ver, batch));
            shard_roots.push(root.0);
        }

        // Phase 4: Single MDBX write transaction for all 16 shards.
        let env = self.shards[0].lock().store().env();
        let tx = env
            .begin_rw_txn()
            .map_err(|e| eyre::eyre!("begin_rw_txn: {e}"))?;
        for (i, (_, batch)) in batches.iter().enumerate() {
            if !batch.is_empty() {
                let tree = self.shards[i].lock();
                tree.store()
                    .write_batch_in_txn(&tx, batch)
                    .map_err(|e| eyre::eyre!("{e}"))?;
            }
        }
        tx.commit().map_err(|e| eyre::eyre!("atomic commit: {e}"))?;

        // Phase 5: Advance shard versions after successful commit.
        for (i, (ver, _)) in batches.iter().enumerate() {
            let mut tree = self.shards[i].lock();
            if *ver > tree.version() {
                tree.set_version(*ver);
            }
        }

        let combined = combine_shard_roots(&shard_roots);
        self.version = new_version;

        let ms = start.elapsed().as_secs_f64() * 1000.0;
        metrics::record_sharded_update(SHARD_COUNT, total_keys, ms);
        debug!(
            target: "n42::jmt",
            version = new_version,
            shards = SHARD_COUNT,
            total_keys,
            update_ms = format!("{ms:.2}"),
            root = %combined,
            "sharded JMT updated (atomic)"
        );

        Ok((new_version, combined))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, U256};
    use n42_execution::state_diff::{AccountDiff, ValueChange};
    use std::collections::BTreeMap;

    fn multi_account_diff(count: usize) -> StateDiff {
        let mut accounts = BTreeMap::new();
        for i in 0..count {
            let addr = Address::with_last_byte(i as u8);
            accounts.insert(
                addr,
                AccountDiff {
                    change_type: AccountChangeType::Created,
                    balance: Some(ValueChange::new(U256::ZERO, U256::from(1000 + i as u64))),
                    nonce: Some(ValueChange::new(0, 1)),
                    code_change: None,
                    storage: BTreeMap::new(),
                },
            );
        }
        StateDiff { accounts }
    }

    #[test]
    fn sharded_basic() {
        let mut jmt = ShardedJmt::new();
        assert_eq!(jmt.version(), 0);

        let diff = multi_account_diff(100);
        let (version, root) = jmt.apply_diff(&diff).unwrap();
        assert_eq!(version, 1);
        assert_ne!(root, B256::ZERO);
    }

    #[test]
    fn sharded_deterministic() {
        let diff = multi_account_diff(50);

        let mut jmt1 = ShardedJmt::new();
        let (_, r1) = jmt1.apply_diff(&diff).unwrap();

        let mut jmt2 = ShardedJmt::new();
        let (_, r2) = jmt2.apply_diff(&diff).unwrap();

        assert_eq!(r1, r2, "same diff should produce same root");
    }

    #[test]
    fn sharded_distributes_across_shards() {
        let mut jmt = ShardedJmt::new();
        let diff = multi_account_diff(200);
        jmt.apply_diff(&diff).unwrap();

        let stats = jmt.shard_stats();
        let non_empty = stats.iter().filter(|(_, keys)| *keys > 0).count();
        assert!(
            non_empty >= 8,
            "expected at least 8 non-empty shards, got {non_empty}"
        );
    }

    #[test]
    fn sharded_get_and_proof() {
        let mut jmt = ShardedJmt::new();
        let addr = Address::repeat_byte(0xAB);
        let diff = {
            let mut accounts = BTreeMap::new();
            accounts.insert(
                addr,
                AccountDiff {
                    change_type: AccountChangeType::Created,
                    balance: Some(ValueChange::new(U256::ZERO, U256::from(999))),
                    nonce: Some(ValueChange::new(0, 5)),
                    code_change: None,
                    storage: BTreeMap::new(),
                },
            );
            StateDiff { accounts }
        };

        jmt.apply_diff(&diff).unwrap();

        let key = account_key(&addr);
        let val = jmt.get(key).unwrap();
        assert!(val.is_some());

        let proof = jmt.get_proof(key).unwrap().expect("shard should have data");
        let roots = jmt.shard_roots().unwrap();
        let shard_idx = shard_index(&key);
        proof
            .verify(jmt::RootHash(roots[shard_idx]), key, val.as_deref())
            .expect("proof should verify");
    }

    #[test]
    fn sharded_sequential_versions() {
        let mut jmt = ShardedJmt::new();

        for i in 1..=5u64 {
            let diff = multi_account_diff(10);
            let (v, _) = jmt.apply_diff(&diff).unwrap();
            assert_eq!(v, i, "global version should increment");
        }
        assert_eq!(jmt.version(), 5);
    }

    #[test]
    fn sharded_preserves_code_hash_on_modify() {
        let mut jmt = ShardedJmt::new();
        let addr = Address::repeat_byte(0xAB);
        let code_hash = B256::repeat_byte(0xCC);

        // Create account with code.
        let mut accounts = BTreeMap::new();
        accounts.insert(
            addr,
            AccountDiff {
                change_type: AccountChangeType::Created,
                balance: Some(ValueChange::new(U256::ZERO, U256::from(100))),
                nonce: Some(ValueChange::new(0, 0)),
                code_change: Some(ValueChange::new(None, Some(code_hash))),
                storage: BTreeMap::new(),
            },
        );
        jmt.apply_diff(&StateDiff { accounts }).unwrap();

        // Modify balance only (code_change = None).
        let mut accounts2 = BTreeMap::new();
        accounts2.insert(
            addr,
            AccountDiff {
                change_type: AccountChangeType::Modified,
                balance: Some(ValueChange::new(U256::from(100), U256::from(200))),
                nonce: None,
                code_change: None,
                storage: BTreeMap::new(),
            },
        );
        jmt.apply_diff(&StateDiff {
            accounts: accounts2,
        })
        .unwrap();

        let key = account_key(&addr);
        let val = jmt.get(key).unwrap().unwrap();
        assert_eq!(
            decode_code_hash(&val),
            code_hash,
            "code_hash must be preserved in sharded mode"
        );
    }

    #[test]
    fn shard_roots_method() {
        let mut jmt = ShardedJmt::new();
        let diff = multi_account_diff(50);
        jmt.apply_diff(&diff).unwrap();

        let roots = jmt.shard_roots().unwrap();
        assert_eq!(roots.len(), SHARD_COUNT);
    }

    #[test]
    fn disk_backed_basic() {
        let dir = tempfile::tempdir().unwrap();
        let mut jmt = ShardedJmt::open_disk(dir.path(), 1000).unwrap();

        let diff = multi_account_diff(100);
        let (version, root) = jmt.apply_diff(&diff).unwrap();
        assert_eq!(version, 1);
        assert_ne!(root, B256::ZERO);

        // Verify data is readable
        let key = crate::keys::account_key(&Address::with_last_byte(0));
        let val = jmt.get(key).unwrap();
        assert!(val.is_some());
    }

    #[test]
    fn disk_backed_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let diff = multi_account_diff(50);

        let root1;
        // Write
        {
            let mut jmt = ShardedJmt::open_disk(dir.path(), 1000).unwrap();
            let (_, root) = jmt.apply_diff(&diff).unwrap();
            root1 = root;
        }

        // Reopen and verify
        {
            let jmt = ShardedJmt::open_disk(dir.path(), 1000).unwrap();
            assert_eq!(jmt.version(), 1);
            let root2 = jmt.root_hash().unwrap();
            assert_eq!(root1, root2, "root must survive restart");
        }
    }

    #[test]
    fn disk_backed_deterministic() {
        let diff = multi_account_diff(50);

        let dir1 = tempfile::tempdir().unwrap();
        let mut jmt1 = ShardedJmt::open_disk(dir1.path(), 1000).unwrap();
        let (_, r1) = jmt1.apply_diff(&diff).unwrap();

        let dir2 = tempfile::tempdir().unwrap();
        let mut jmt2 = ShardedJmt::open_disk(dir2.path(), 1000).unwrap();
        let (_, r2) = jmt2.apply_diff(&diff).unwrap();

        assert_eq!(
            r1, r2,
            "same diff should produce same root across disk instances"
        );

        // Also compare with in-memory
        let mut jmt_mem = ShardedJmt::new();
        let (_, r_mem) = jmt_mem.apply_diff(&diff).unwrap();
        assert_eq!(r1, r_mem, "disk and memory roots must match");
    }

    #[test]
    fn disk_atomic_matches_regular() {
        let diff = multi_account_diff(100);

        // Regular per-shard write path
        let dir1 = tempfile::tempdir().unwrap();
        let mut jmt1 = ShardedJmt::open_disk(dir1.path(), 1000).unwrap();
        let (v1, r1) = jmt1.apply_diff(&diff).unwrap();

        // Atomic single-transaction path
        let dir2 = tempfile::tempdir().unwrap();
        let mut jmt2 = ShardedJmt::open_disk(dir2.path(), 1000).unwrap();
        let (v2, r2) = jmt2.apply_diff_atomic(&diff).unwrap();

        assert_eq!(v1, v2, "versions must match");
        assert_eq!(r1, r2, "atomic root must match regular root");

        // Verify persistence after atomic write
        drop(jmt2);
        let jmt2_reopened = ShardedJmt::open_disk(dir2.path(), 1000).unwrap();
        assert_eq!(jmt2_reopened.version(), v2);
        assert_eq!(
            jmt2_reopened.root_hash().unwrap(),
            r2,
            "atomic write must persist"
        );
    }

    #[test]
    fn disk_atomic_multi_block() {
        let dir = tempfile::tempdir().unwrap();
        let mut jmt = ShardedJmt::open_disk(dir.path(), 1000).unwrap();

        for i in 0..5u8 {
            let diff = multi_account_diff((10 + i * 5) as usize);
            let (ver, root) = jmt.apply_diff_atomic(&diff).unwrap();
            assert_eq!(ver, (i + 1) as u64);
            assert_ne!(root, B256::ZERO);
        }

        assert_eq!(jmt.version(), 5);

        // Verify after reopen
        drop(jmt);
        let jmt = ShardedJmt::open_disk(dir.path(), 1000).unwrap();
        assert_eq!(jmt.version(), 5);
        assert_ne!(jmt.root_hash().unwrap(), B256::ZERO);
    }
}
