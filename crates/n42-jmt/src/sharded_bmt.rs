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

use crate::bmt::{BmtProof, Hash, Sbmt, hash_value};
use crate::keys::{account_key, storage_key};
use crate::proof::VerifyError;
use crate::sharded::SHARD_COUNT;
use crate::snapshot::JmtSnapshot;
use crate::tree::{decode_code_hash, encode_account_value};

use alloy_primitives::B256;
use n42_execution::state_diff::{AccountChangeType, StateDiff};
use parking_lot::Mutex;
use rayon::prelude::*;
use std::collections::HashMap;

#[inline]
fn shard_index(key: &Hash) -> usize {
    (key[0] >> 4) as usize
}

// ---------------------------------------------------------------------------
// Shard-root commitment: a depth-4 binary merkle tree over the 16 shard roots.
//
// Replaces the flat `blake3(concat 16 roots)` so a proof only carries the target
// shard root + its 4 siblings (160 B) instead of all 16 roots (512 B). SBMT runs
// from a fresh genesis, so changing the combined-root algorithm has no compat cost.
// ---------------------------------------------------------------------------

#[inline]
fn hash_pair(a: &Hash, b: &Hash) -> Hash {
    let mut h = blake3::Hasher::new();
    h.update(a);
    h.update(b);
    *h.finalize().as_bytes()
}

/// Merkle root over the 16 shard roots (16 → 8 → 4 → 2 → 1, depth 4).
fn shard_tree_root(leaves: &[Hash]) -> Hash {
    let mut level: Vec<Hash> = leaves.to_vec();
    while level.len() > 1 {
        level = level.chunks(2).map(|c| hash_pair(&c[0], &c[1])).collect();
    }
    level[0]
}

/// Authentication path (siblings, bottom-up) from `index` to the shard-tree root.
fn shard_tree_path(leaves: &[Hash], index: usize) -> Vec<Hash> {
    let mut path = Vec::new();
    let mut level: Vec<Hash> = leaves.to_vec();
    let mut idx = index;
    while level.len() > 1 {
        path.push(level[idx ^ 1]);
        level = level.chunks(2).map(|c| hash_pair(&c[0], &c[1])).collect();
        idx >>= 1;
    }
    path
}

/// Recompute the shard-tree root from a leaf shard root + its authentication path.
fn shard_tree_root_from_path(leaf: Hash, index: usize, path: &[Hash]) -> Hash {
    let mut cur = leaf;
    let mut idx = index;
    for sib in path {
        cur = if idx & 1 == 0 {
            hash_pair(&cur, sib)
        } else {
            hash_pair(sib, &cur)
        };
        idx >>= 1;
    }
    cur
}

/// 16-shard parallel Sparse Binary Merkle Tree with per-shard value storage.
pub struct ShardedSbmt {
    shards: Vec<Mutex<Sbmt>>,
    /// Per-shard raw key→value store (unified KV layer for read-back).
    values: Vec<Mutex<HashMap<Hash, Vec<u8>>>>,
    version: u64,
}

impl Default for ShardedSbmt {
    fn default() -> Self {
        Self::new()
    }
}

impl ShardedSbmt {
    /// Create an empty 16-shard SBMT.
    pub fn new() -> Self {
        Self {
            shards: (0..SHARD_COUNT).map(|_| Mutex::new(Sbmt::new())).collect(),
            values: (0..SHARD_COUNT)
                .map(|_| Mutex::new(HashMap::new()))
                .collect(),
            version: 0,
        }
    }

    /// Current global version.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Combined root hash: the depth-4 merkle root over the 16 shard roots.
    pub fn root_hash(&self) -> B256 {
        let roots: Vec<Hash> = self.shards.iter().map(|s| s.lock().root_hash()).collect();
        B256::from(shard_tree_root(&roots))
    }

    /// Read a key's raw value at the current version.
    pub fn get(&self, key: &Hash) -> Option<Vec<u8>> {
        self.values[shard_index(key)].lock().get(key).cloned()
    }

    /// Read the existing `code_hash` for an account key (or ZERO if absent).
    fn existing_code_hash(&self, account_key_bytes: &Hash) -> B256 {
        self.values[shard_index(account_key_bytes)]
            .lock()
            .get(account_key_bytes)
            .map(|v| decode_code_hash(v))
            .unwrap_or(B256::ZERO)
    }

    /// Partition a `StateDiff` into per-shard `(key, Option<value>)` updates.
    ///
    /// Mirrors `ShardedJmt::prepare_shard_updates`: for a `Modified` account with
    /// no `code_change`, the existing `code_hash` is read back and preserved.
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
                        Some(change) => change.to.unwrap_or(B256::ZERO),
                        None => self.existing_code_hash(&key),
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
            .par_iter()
            .zip(self.values.par_iter())
            .zip(shard_updates.into_par_iter())
            .map(|((shard, vals), updates)| {
                let mut tree = shard.lock();
                let mut kv = vals.lock();
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
            .map(|(s, v)| (s.lock().len(), v.lock().len()))
            .collect()
    }

    /// Snapshot all live `(key, value)` pairs plus version and combined root.
    ///
    /// Reuses the shared [`JmtSnapshot`] format (a plain KV dump), so SBMT and
    /// JMT snapshots are interchangeable on disk.
    pub fn snapshot(&self) -> JmtSnapshot {
        let mut entries = Vec::new();
        for vals in &self.values {
            for (key, value) in vals.lock().iter() {
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
        let inner = self.shards[si].lock().prove(key);
        let value = self.values[si].lock().get(&key).cloned();
        let roots: Vec<Hash> = self.shards.iter().map(|s| s.lock().root_hash()).collect();
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
            t.shards[si].lock().insert(*key, value);
            t.values[si].lock().insert(*key, value.clone());
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

/// A self-contained proof for a key in a [`ShardedSbmt`].
///
/// Carries the target shard root + its depth-4 shard-tree path (instead of all 16
/// shard roots), the in-shard [`BmtProof`], and the raw value. Verifiable with
/// only the block header's combined root — no tree access (mobile-callable).
#[derive(Debug, Clone)]
pub struct ShardedBmtProof {
    pub shard_index: u8,
    /// The target shard's root.
    pub shard_root: Hash,
    /// Authentication path from `shard_root` up to the combined root (4 siblings).
    pub shard_path: Vec<Hash>,
    pub inner: BmtProof,
    pub value: Option<Vec<u8>>,
}

impl ShardedBmtProof {
    /// Verify against a known combined root hash.
    pub fn verify(&self, combined_root: &B256) -> Result<(), VerifyError> {
        if self.shard_index as usize >= SHARD_COUNT {
            return Err(VerifyError::ShardIndexOutOfRange(self.shard_index));
        }
        // Step 1: recompute the combined root from shard_root + shard-tree path.
        let computed = B256::from(shard_tree_root_from_path(
            self.shard_root,
            self.shard_index as usize,
            &self.shard_path,
        ));
        if computed != *combined_root {
            return Err(VerifyError::CombinedRootMismatch {
                expected: *combined_root,
                computed,
            });
        }
        // Step 2: the raw value must hash to the value_hash the in-shard proof commits to.
        let expected_vh = self.value.as_ref().map(|v| hash_value(v));
        if self.inner.value_hash != expected_vh {
            return Err(VerifyError::ShardProofFailed("value hash mismatch".into()));
        }
        // Step 3: verify the in-shard binary proof against the shard root.
        if !self.inner.verify(&self.shard_root, expected_vh) {
            return Err(VerifyError::ShardProofFailed("in-shard proof failed".into()));
        }
        Ok(())
    }

    /// Estimated serialized size in bytes (same basis as `JmtProof::estimated_size`).
    pub fn estimated_size(&self) -> usize {
        1 + 32                               // shard_index + shard_root
            + 4 + self.shard_path.len() * 32 // shard-tree path (len prefix + siblings)
            + self.inner.encoded_len()
            + self.value.as_ref().map_or(0, |v| v.len())
    }

    /// Bytes of the in-shard merkle authentication path (the part that differs from JMT).
    pub fn path_bytes(&self) -> usize {
        self.inner.path_len() * 32
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
        assert_eq!(root, t.root_hash(), "returned root must match recomputed root");
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
        assert!(non_empty >= 8, "expected ≥8 non-empty shards, got {non_empty}");
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
        assert_eq!(decode_code_hash(&val), code_hash, "code_hash must survive modify");
    }

    #[test]
    fn sharded_proof_inclusion_exclusion_tamper() {
        let mut t = ShardedSbmt::new();
        t.apply_diff(&created_diff(500));
        let root = t.root_hash();

        let key = account_key(&addr_of(42)).0;
        let proof = t.prove(key);
        assert!(proof.value.is_some());
        assert!(proof.verify(&root).is_ok(), "inclusion proof must verify");

        let absent = account_key(&addr_of(999_999)).0;
        let ep = t.prove(absent);
        assert!(ep.value.is_none());
        assert!(ep.verify(&root).is_ok(), "exclusion proof must verify");

        let mut bad = proof.clone();
        bad.shard_root[0] ^= 0xFF;
        assert!(bad.verify(&root).is_err(), "tampered shard root must fail");
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

        assert!(sp.verify(&sbmt.root_hash()).is_ok());
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
