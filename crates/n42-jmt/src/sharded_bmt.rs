//! 16-shard parallel SBMT — the binary-tree counterpart of [`crate::ShardedJmt`].
//!
//! Each shard is an independent [`Sbmt`] plus a per-shard key→value map (the
//! QMDB-style "unified KV + Merkle" split): the SBMT holds `key → blake3(value)`
//! commitments, the KV map holds the raw bytes so accounts can be read back
//! (e.g. to preserve `code_hash` on a balance-only `Modified`). Updates are
//! partitioned by the first nibble of the key and applied in parallel via rayon;
//! the combined root is `blake3(shard_0_root || … || shard_15_root)`, identical
//! in shape to `ShardedJmt` so the two are directly comparable.
//!
//! Phase-2 of the JMT→SBMT switch (devlog-59): brings SBMT to feature/perf parity
//! with the sharded JMT path for end-to-end `apply_diff` benchmarking. Persistence,
//! proof packaging, and mobile verification are later phases.

use crate::keys::{account_key, storage_key};
use crate::snapshot::JmtSnapshot;
use crate::tree::{EMPTY_CODE_HASH, decode_code_hash, encode_account_value};
use n42_bmt_core::{Hash, SHARD_COUNT, Sbmt, ShardedBmtProof, shard_tree_path, shard_tree_root};

use alloy_primitives::{Address, B256, U256};
use n42_execution::state_diff::{AccountChangeType, StateDiff};
use rayon::prelude::*;
use std::collections::HashMap;

#[inline]
fn shard_index(key: &Hash) -> usize {
    n42_bmt_core::shard_index_for_key(key)
}

/// 16-shard parallel Sparse Binary Merkle Tree with per-shard value storage.
///
/// Shards are stored *unlocked*: every caller reaches `ShardedSbmt` through the
/// node's outer `Arc<Mutex<PersistentSbmt>>`, so access is already serialized and
/// a per-shard lock would only double-lock. `apply_diff` takes `&mut self` and
/// fans out with `par_iter_mut`, giving each rayon worker exclusive ownership of
/// one shard — no locks on the critical path.
pub struct ShardedSbmt {
    shards: Vec<Sbmt>,
    /// Per-shard raw key→value store (unified KV layer for read-back).
    values: Vec<HashMap<Hash, Vec<u8>>>,
    version: u64,
}

impl Default for ShardedSbmt {
    fn default() -> Self {
        Self::new()
    }
}

impl ShardedSbmt {
    /// Create an empty 16-shard SBMT.
    ///
    /// Every key in shard `s` shares the top-nibble selector (`key[0] >> 4 == s`),
    /// so the in-shard trees branch from `base_depth = log2(SHARD_COUNT)`, skipping
    /// the otherwise-forced single-child chain for that shared prefix.
    pub fn new() -> Self {
        let base_depth = SHARD_COUNT.trailing_zeros() as usize;
        Self {
            shards: (0..SHARD_COUNT)
                .map(|_| Sbmt::with_base_depth(base_depth))
                .collect(),
            values: (0..SHARD_COUNT).map(|_| HashMap::new()).collect(),
            version: 0,
        }
    }

    /// Current global version.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Combined root hash: the depth-4 merkle root over the 16 shard roots.
    pub fn root_hash(&self) -> B256 {
        let roots: Vec<Hash> = self.shards.iter().map(|s| s.root_hash()).collect();
        B256::from(shard_tree_root(&roots))
    }

    /// Read a key's raw value at the current version.
    pub fn get(&self, key: &Hash) -> Option<Vec<u8>> {
        self.values[shard_index(key)].get(key).cloned()
    }

    fn insert_raw(&mut self, key: Hash, value: Vec<u8>) {
        let si = shard_index(&key);
        self.shards[si].insert(key, &value);
        self.values[si].insert(key, value);
    }

    /// Seed a genesis account without advancing the block-version counter.
    ///
    /// `apply_diff` increments `version` once per committed block. Genesis state
    /// is the starting point for proofs, so it is inserted directly at version 0.
    pub fn seed_genesis_account<I>(
        &mut self,
        address: Address,
        balance: U256,
        nonce: u64,
        code_hash: B256,
        storage: I,
    ) where
        I: IntoIterator<Item = (U256, U256)>,
    {
        let account_key = account_key(&address).0;
        self.insert_raw(
            account_key,
            encode_account_value(&balance, nonce, &code_hash),
        );

        for (slot, value) in storage {
            if value.is_zero() {
                continue;
            }
            let key = storage_key(&address, &slot).0;
            self.insert_raw(key, value.to_be_bytes::<32>().to_vec());
        }
    }

    /// Partition a `StateDiff` into per-shard `(key, Option<value>)` updates.
    ///
    /// Mirrors `ShardedJmt::prepare_shard_updates`: for a `Modified` account with
    /// no `code_change`, the existing `code_hash` is read back and preserved.
    ///
    /// Kept single-pass + serial: a rayon fan-out here measured *slower* than the
    /// per-shard parallelism in `apply_diff` (extra intermediate Vecs + dispatch
    /// dominate for typical block sizes). The lock-free shard storage lets the
    /// `code_hash` read-back hit the value map directly, no `Mutex`.
    fn prepare(&self, diff: &StateDiff) -> Vec<Vec<(Hash, Option<Vec<u8>>)>> {
        let mut shard_updates: Vec<Vec<(Hash, Option<Vec<u8>>)>> =
            (0..SHARD_COUNT).map(|_| Vec::new()).collect();

        for (address, account_diff) in &diff.accounts {
            let key = account_key(address).0;
            let si = shard_index(&key);

            match account_diff.change_type {
                AccountChangeType::Destroyed => {
                    shard_updates[si].push((key, None));
                    for slot in account_diff.storage.keys() {
                        let skey = storage_key(address, slot).0;
                        shard_updates[shard_index(&skey)].push((skey, None));
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
                        Some(change) => change.to.unwrap_or(EMPTY_CODE_HASH),
                        None => self.values[si]
                            .get(&key)
                            .map(|v| decode_code_hash(v))
                            .unwrap_or(EMPTY_CODE_HASH),
                    };
                    let value = encode_account_value(&balance, nonce, &code_hash);
                    shard_updates[si].push((key, Some(value)));

                    for (slot, change) in &account_diff.storage {
                        let skey = storage_key(address, slot).0;
                        let ssi = shard_index(&skey);
                        if change.to.is_zero() {
                            shard_updates[ssi].push((skey, None));
                        } else {
                            shard_updates[ssi]
                                .push((skey, Some(change.to.to_be_bytes::<32>().to_vec())));
                        }
                    }
                }
            }
        }

        shard_updates
    }

    /// Apply a `StateDiff` across all 16 shards in parallel.
    ///
    /// Returns `(new_version, combined_root)`.
    pub fn apply_diff(&mut self, diff: &StateDiff) -> (u64, B256) {
        let new_version = self.version + 1;
        let shard_updates = self.prepare(diff);

        let roots: Vec<Hash> = self
            .shards
            .par_iter_mut()
            .zip(self.values.par_iter_mut())
            .zip(shard_updates.into_par_iter())
            .map(|((tree, kv), updates)| {
                for (key, value) in &updates {
                    match value {
                        Some(bytes) => {
                            tree.insert(*key, bytes);
                            kv.insert(*key, bytes.clone());
                        }
                        None => {
                            tree.remove(key);
                            kv.remove(key);
                        }
                    }
                }
                tree.root_hash()
            })
            .collect();

        self.version = new_version;
        (new_version, B256::from(shard_tree_root(&roots)))
    }

    /// Per-shard `(leaf_count, value_count)` for diagnostics.
    pub fn shard_stats(&self) -> Vec<(usize, usize)> {
        self.shards
            .iter()
            .zip(self.values.iter())
            .map(|(s, v)| (s.len(), v.len()))
            .collect()
    }

    /// Aggregate live-tree node statistics across all 16 shards (for footprint
    /// benchmarking). The depth-4 shard-tree over the 16 shard roots adds 15
    /// transient internal nodes that are recomputed on demand and not counted.
    pub fn node_stats(&self) -> n42_bmt_core::NodeStats {
        let mut agg = n42_bmt_core::NodeStats::default();
        for s in &self.shards {
            let st = s.node_stats();
            agg.internal_nodes += st.internal_nodes;
            agg.leaf_nodes += st.leaf_nodes;
            agg.serialized_bytes += st.serialized_bytes;
        }
        agg
    }

    /// Snapshot all live `(key, value)` pairs plus version and combined root.
    ///
    /// Reuses the shared [`JmtSnapshot`] format (a plain KV dump), so SBMT and
    /// JMT snapshots are interchangeable on disk.
    pub fn snapshot(&self) -> JmtSnapshot {
        let mut entries = Vec::new();
        for vals in &self.values {
            for (key, value) in vals.iter() {
                entries.push((*key, value.clone()));
            }
        }
        JmtSnapshot {
            version: self.version,
            root: self.root_hash().0,
            entries,
        }
    }

    /// Build a self-contained proof for `key` (16 shard roots + in-shard proof +
    /// raw value), the SBMT counterpart of [`crate::JmtProof`] for mobile clients.
    pub fn prove(&self, key: Hash) -> ShardedBmtProof {
        let si = shard_index(&key);
        let inner = self.shards[si].prove(key);
        let value = self.values[si].get(&key).cloned();
        let roots: Vec<Hash> = self.shards.iter().map(|s| s.root_hash()).collect();
        ShardedBmtProof {
            shard_index: si as u8,
            shard_root: roots[si],
            shard_path: shard_tree_path(&roots, si),
            inner,
            value,
        }
    }

    /// Reconstruct a `ShardedSbmt` from a snapshot, verifying the combined root.
    pub fn from_snapshot(snapshot: &JmtSnapshot) -> eyre::Result<Self> {
        let mut t = Self::new();
        for (key, value) in &snapshot.entries {
            let si = shard_index(key);
            t.shards[si].insert(*key, value);
            t.values[si].insert(*key, value.clone());
        }
        t.version = snapshot.version;

        let root = t.root_hash();
        if root.0 != snapshot.root {
            return Err(eyre::eyre!(
                "SBMT snapshot root mismatch: expected {}, got {root}",
                B256::from(snapshot.root),
            ));
        }
        Ok(t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr_of(i: usize) -> Address {
        let mut b = [0u8; 20];
        b[..8].copy_from_slice(&(i as u64).to_le_bytes());
        Address::from(b)
    }
    use alloy_primitives::{Address, U256};
    use n42_execution::state_diff::{AccountDiff, ValueChange};
    use std::collections::BTreeMap;

    fn created_diff(count: usize) -> StateDiff {
        let mut accounts = BTreeMap::new();
        for i in 0..count {
            let mut b = [0u8; 20];
            b[..8].copy_from_slice(&(i as u64).to_le_bytes());
            accounts.insert(
                Address::from(b),
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
    fn basic_apply() {
        let mut t = ShardedSbmt::new();
        let (v, root) = t.apply_diff(&created_diff(100));
        assert_eq!(v, 1);
        assert_ne!(root, B256::ZERO);
        assert_eq!(
            root,
            t.root_hash(),
            "returned root must match recomputed root"
        );
    }

    #[test]
    fn created_eoa_defaults_to_empty_code_hash() {
        let mut t = ShardedSbmt::new();
        let addr = Address::repeat_byte(0xEE);
        let diff = StateDiff {
            accounts: BTreeMap::from([(
                addr,
                AccountDiff {
                    change_type: AccountChangeType::Created,
                    balance: Some(ValueChange::new(U256::ZERO, U256::from(1000))),
                    nonce: Some(ValueChange::new(0, 1)),
                    code_change: None,
                    storage: BTreeMap::new(),
                },
            )]),
        };
        t.apply_diff(&diff);

        let key = account_key(&addr).0;
        let value = t.get(&key).unwrap();
        assert_eq!(decode_code_hash(&value), EMPTY_CODE_HASH);
        assert_ne!(decode_code_hash(&value), B256::ZERO);
    }

    #[test]
    fn deterministic_root() {
        let diff = created_diff(64);
        let mut a = ShardedSbmt::new();
        let mut b = ShardedSbmt::new();
        let (_, ra) = a.apply_diff(&diff);
        let (_, rb) = b.apply_diff(&diff);
        assert_eq!(ra, rb, "same diff → same combined root");
    }

    #[test]
    fn distributes_across_shards() {
        let mut t = ShardedSbmt::new();
        t.apply_diff(&created_diff(200));
        let non_empty = t.shard_stats().iter().filter(|(n, _)| *n > 0).count();
        assert!(
            non_empty >= 8,
            "expected ≥8 non-empty shards, got {non_empty}"
        );
    }

    #[test]
    fn read_back_value() {
        let mut t = ShardedSbmt::new();
        t.apply_diff(&created_diff(10));
        let mut b = [0u8; 20];
        b[..8].copy_from_slice(&3u64.to_le_bytes());
        let key = account_key(&Address::from(b)).0;
        assert!(t.get(&key).is_some(), "account value must be readable back");
    }

    #[test]
    fn snapshot_roundtrip() {
        let mut t = ShardedSbmt::new();
        t.apply_diff(&created_diff(120));
        let root = t.root_hash();
        let version = t.version();

        let snap = t.snapshot();
        assert_eq!(snap.entries.len(), 120);
        assert_eq!(snap.version, version);

        let restored = ShardedSbmt::from_snapshot(&snap).unwrap();
        assert_eq!(restored.root_hash(), root, "restored root must match");
        assert_eq!(restored.version(), version);
    }

    #[test]
    fn from_snapshot_rejects_root_mismatch() {
        let mut t = ShardedSbmt::new();
        t.apply_diff(&created_diff(8));
        let mut snap = t.snapshot();
        snap.root[0] ^= 0xFF;
        assert!(ShardedSbmt::from_snapshot(&snap).is_err());
    }

    #[test]
    fn preserves_code_hash_on_modify() {
        let mut t = ShardedSbmt::new();
        let addr = Address::repeat_byte(0xAB);
        let code_hash = B256::repeat_byte(0xCC);

        let mut a1 = BTreeMap::new();
        a1.insert(
            addr,
            AccountDiff {
                change_type: AccountChangeType::Created,
                balance: Some(ValueChange::new(U256::ZERO, U256::from(100))),
                nonce: Some(ValueChange::new(0, 0)),
                code_change: Some(ValueChange::new(None, Some(code_hash))),
                storage: BTreeMap::new(),
            },
        );
        t.apply_diff(&StateDiff { accounts: a1 });

        // Balance-only modify, code_change = None.
        let mut a2 = BTreeMap::new();
        a2.insert(
            addr,
            AccountDiff {
                change_type: AccountChangeType::Modified,
                balance: Some(ValueChange::new(U256::from(100), U256::from(200))),
                nonce: None,
                code_change: None,
                storage: BTreeMap::new(),
            },
        );
        t.apply_diff(&StateDiff { accounts: a2 });

        let key = account_key(&addr).0;
        let val = t.get(&key).unwrap();
        assert_eq!(
            decode_code_hash(&val),
            code_hash,
            "code_hash must survive modify"
        );
    }

    #[test]
    fn sharded_proof_inclusion_exclusion_tamper() {
        let mut t = ShardedSbmt::new();
        t.apply_diff(&created_diff(500));
        let root = t.root_hash();

        let key = account_key(&addr_of(42)).0;
        let proof = t.prove(key);
        assert!(proof.value.is_some());
        assert!(proof.verify(&root.0).is_ok(), "inclusion proof must verify");

        let absent = account_key(&addr_of(999_999)).0;
        let ep = t.prove(absent);
        assert!(ep.value.is_none());
        assert!(ep.verify(&root.0).is_ok(), "exclusion proof must verify");

        let mut bad = proof.clone();
        bad.shard_root[0] ^= 0xFF;
        assert!(
            bad.verify(&root.0).is_err(),
            "tampered shard root must fail"
        );
    }

    #[test]
    fn proof_size_vs_jmt() {
        let diff = created_diff(5_000);
        let mut sbmt = ShardedSbmt::new();
        sbmt.apply_diff(&diff);
        let mut jmt = crate::ShardedJmt::new();
        jmt.apply_diff(&diff).unwrap();

        let key = account_key(&addr_of(42)).0;
        let sp = sbmt.prove(key);
        let jp = crate::proof::build_proof(&jmt, jmt::KeyHash(key))
            .unwrap()
            .unwrap();

        assert!(sp.verify(&sbmt.root_hash().0).is_ok());
        assert!(jp.verify(&jmt.root_hash().unwrap()).is_ok());

        eprintln!(
            "[proof-size @5000 distinct accts] SBMT total={}B path={}B depth={} | JMT total={}B path_bytes={}B",
            sp.estimated_size(),
            sp.path_bytes(),
            sp.inner.path_len(),
            jp.estimated_size(),
            jp.proof_bytes.len(),
        );
    }
}
