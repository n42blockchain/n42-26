//! `StateDiff` bridge for the all-DRAM twig engine ([`n42_twig_core`]).
//!
//! Wraps [`ShardedTwig`] with the n42 account/storage encoding so the node can
//! drive it the same way it drives [`crate::ShardedSbmt`]: `apply_diff` flattens a
//! `StateDiff` into canonical `(key, Option<value>)` ops (account/storage keys via
//! the shared `account_key`/`storage_key`, `code_hash` read back from the unified
//! KV when a `Modified` account has no `code_change`), then applies them in
//! keyHash-sorted order. See `docs/devlog-64` (P6).

use crate::keys::{KeyJob, account_key, derive_keys_batch, storage_key};
#[cfg(test)]
use crate::tree::decode_code_hash;
use crate::tree::{EMPTY_CODE_HASH, decode_code_hash_checked, encode_account_value};

use alloy_primitives::{Address, B256, U256};
use n42_execution::state_diff::{AccountChangeType, StateDiff};
use n42_twig_core::{Hash, ShardedTwig, ShardedTwigProof, TwigSnapshot};

/// Production state tree backed by the twig engine.
pub struct TwigState {
    inner: ShardedTwig,
}

impl Default for TwigState {
    fn default() -> Self {
        Self::new()
    }
}

impl TwigState {
    pub fn new() -> Self {
        Self {
            inner: ShardedTwig::new(),
        }
    }

    pub fn version(&self) -> u64 {
        self.inner.version()
    }

    /// Combined state root.
    pub fn root(&mut self) -> B256 {
        B256::from(self.inner.root())
    }

    /// Read a raw leaf value by its derived key.
    pub fn get(&self, key: &Hash) -> Option<&[u8]> {
        self.inner.get(key)
    }

    /// Seed a genesis account at version 0 (no version bump). Call [`root`] once
    /// after all genesis seeding.
    ///
    /// [`root`]: Self::root
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
        let key = account_key(&address).0;
        self.inner
            .set(key, &encode_account_value(&balance, nonce, &code_hash));
        for (slot, value) in storage {
            if value.is_zero() {
                continue;
            }
            let skey = storage_key(&address, &slot).0;
            self.inner.set(skey, &value.to_be_bytes::<32>());
        }
    }

    /// Apply a block's `StateDiff`. Returns `(new_version, combined_root)`.
    ///
    /// Values are staged in one per-block arena and passed as borrowed slices
    /// (`apply_batch_refs`) — the per-op `Vec<u8>` of an owned-value API was a
    /// profiled allocator hot spot.
    pub fn apply_diff(&mut self, diff: &StateDiff) -> eyre::Result<(u64, B256)> {
        let (meta, buf) = self.prepare(diff)?;
        let ops: Vec<(Hash, Option<&[u8]>)> = meta
            .iter()
            .map(|(k, r)| (*k, r.map(|(o, l)| &buf[o..o + l])))
            .collect();
        let (version, root) = self.inner.apply_batch_refs(&ops);
        Ok((version, B256::from(root)))
    }

    /// Flatten a `StateDiff` into per-op `(key, Option<(offset, len)>)` metadata
    /// plus one flat value arena (mirrors [`crate::ShardedSbmt`]'s prepare;
    /// `code_hash` read back from the unified KV).
    ///
    /// Key derivation (one blake3 per op) runs SIMD-batched up front via
    /// [`derive_keys_batch`]; the second pass consumes the keys in the same
    /// encounter order (the read-back of `code_hash` needs the derived key, so
    /// it must run after derivation).
    #[allow(clippy::type_complexity)]
    fn prepare(
        &self,
        diff: &StateDiff,
    ) -> eyre::Result<(Vec<(Hash, Option<(usize, usize)>)>, Vec<u8>)> {
        // Pass 1: collect every key-derivation job in encounter order.
        let mut jobs: Vec<KeyJob> = Vec::new();
        for (address, account_diff) in &diff.accounts {
            jobs.push(KeyJob::Account(*address));
            for slot in account_diff.storage.keys() {
                jobs.push(KeyJob::Storage(*address, *slot));
            }
        }
        let keys = derive_keys_batch(&jobs);

        // Pass 2: build the op metadata + value arena, consuming keys in order.
        let mut meta: Vec<(Hash, Option<(usize, usize)>)> = Vec::with_capacity(keys.len());
        let mut buf: Vec<u8> = Vec::new();
        let push_val = |buf: &mut Vec<u8>, v: &[u8]| {
            let off = buf.len();
            buf.extend_from_slice(v);
            Some((off, v.len()))
        };
        let mut ki = 0usize;
        for (address, account_diff) in &diff.accounts {
            let key = keys[ki];
            ki += 1;
            match account_diff.change_type {
                AccountChangeType::Destroyed => {
                    meta.push((key, None));
                    for _slot in account_diff.storage.keys() {
                        meta.push((keys[ki], None));
                        ki += 1;
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
                        None => match self.inner.get(&key) {
                            Some(value) => decode_code_hash_checked(value).map_err(|error| {
                                eyre::eyre!(
                                    "twig account leaf for Modified account {address} at key {key:?} is corrupt: {error}"
                                )
                            })?,
                            // A Created account legitimately has no prior leaf.
                            None if matches!(
                                account_diff.change_type,
                                AccountChangeType::Created
                            ) =>
                            {
                                EMPTY_CODE_HASH
                            }
                            // A Modified account with no code_change MUST have an
                            // existing leaf; a miss means the tree lost it (F5).
                            // Defaulting to EMPTY_CODE_HASH would commit a wrong
                            // leaf and permanently diverge the append-ordered twig
                            // root — fail loud instead.
                            None => {
                                return Err(eyre::eyre!(
                                    "twig read-miss: Modified account {address} at key {key:?} has no code_change \
                                     and no existing leaf; refusing to commit EMPTY_CODE_HASH"
                                ));
                            }
                        },
                    };
                    let v = encode_account_value(&balance, nonce, &code_hash);
                    let r = push_val(&mut buf, &v);
                    meta.push((key, r));
                    for change in account_diff.storage.values() {
                        let skey = keys[ki];
                        ki += 1;
                        if change.to.is_zero() {
                            meta.push((skey, None));
                        } else {
                            let r = push_val(&mut buf, &change.to.to_be_bytes::<32>());
                            meta.push((skey, r));
                        }
                    }
                }
            }
        }
        Ok((meta, buf))
    }

    /// Build a key-bindable proof for a derived `key` (requires a prior root call).
    pub fn prove(&self, key: Hash) -> Option<ShardedTwigProof> {
        self.inner.prove(&key)
    }

    /// Cross-check live-counter consistency across all shards (F3).
    pub fn check_consistency(&self) -> eyre::Result<()> {
        self.inner.check_consistency().map_err(|e| eyre::eyre!(e))
    }

    pub fn snapshot(&self) -> TwigSnapshot {
        self.inner.snapshot()
    }

    /// Restore and validate a snapshot loaded from persistent or otherwise
    /// untrusted bytes.
    pub fn try_from_snapshot(snap: &TwigSnapshot) -> eyre::Result<Self> {
        Ok(Self {
            inner: ShardedTwig::try_from_snapshot(snap)
                .map_err(|error| eyre::eyre!("invalid Twig snapshot: {error}"))?,
        })
    }

    /// Restore a trusted in-process snapshot.
    pub fn from_snapshot(snap: &TwigSnapshot) -> Self {
        Self::try_from_snapshot(snap).expect("invalid Twig snapshot")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use n42_execution::state_diff::{AccountChangeType, AccountDiff, StateDiff, ValueChange};
    use std::collections::BTreeMap;

    fn created(addr: Address, balance: u64) -> (Address, AccountDiff) {
        (
            addr,
            AccountDiff {
                change_type: AccountChangeType::Created,
                balance: Some(ValueChange::new(U256::ZERO, U256::from(balance))),
                nonce: Some(ValueChange::new(0, 1)),
                code_change: None,
                storage: BTreeMap::new(),
            },
        )
    }

    #[test]
    fn apply_diff_root_get_prove() {
        let mut t = TwigState::new();
        let mut accounts = BTreeMap::new();
        for i in 0..500u64 {
            let (a, d) = created(Address::with_last_byte((i % 256) as u8), 1000 + i);
            // distinct addresses
            let mut b = [0u8; 20];
            b[..8].copy_from_slice(&i.to_le_bytes());
            accounts.insert(Address::from(b), d.clone());
            let _ = a;
        }
        let diff = StateDiff { accounts };
        let (v, root) = t.apply_diff(&diff).unwrap();
        assert_eq!(v, 1);
        assert_ne!(root, B256::ZERO);

        // read-back + key-bound proof for one account
        let mut b = [0u8; 20];
        b[..8].copy_from_slice(&42u64.to_le_bytes());
        let addr = Address::from(b);
        let key = account_key(&addr).0;
        assert!(t.get(&key).is_some());
        let _ = t.root();
        let p = t.prove(key).unwrap();
        assert!(p.verify_for_key(&root.0, &key).is_ok());
        // #11: wrong key rejected
        let mut b2 = [0u8; 20];
        b2[..8].copy_from_slice(&43u64.to_le_bytes());
        let other = account_key(&Address::from(b2)).0;
        assert!(p.verify_for_key(&root.0, &other).is_err());
    }

    #[test]
    fn snapshot_roundtrip() {
        let mut t = TwigState::new();
        let mut accounts = BTreeMap::new();
        for i in 0..300u64 {
            let mut b = [0u8; 20];
            b[..8].copy_from_slice(&i.to_le_bytes());
            accounts.insert(Address::from(b), created(Address::ZERO, 1000 + i).1);
        }
        t.apply_diff(&StateDiff { accounts }).unwrap();
        let root = t.root();
        let snap = t.snapshot();
        let mut restored = TwigState::from_snapshot(&snap);
        assert_eq!(restored.root(), root);
        assert_eq!(restored.version(), t.version());
    }

    fn modified_no_code(addr: Address) -> (Address, AccountDiff) {
        (
            addr,
            AccountDiff {
                change_type: AccountChangeType::Modified,
                balance: Some(ValueChange::new(U256::ZERO, U256::from(5))),
                nonce: Some(ValueChange::new(0, 1)),
                code_change: None,
                storage: BTreeMap::new(),
            },
        )
    }

    /// F5: a `Modified` account with no code_change whose leaf is absent must
    /// fail loud, not silently commit an `EMPTY_CODE_HASH` leaf that diverges
    /// the append-ordered root.
    #[test]
    fn modified_account_read_miss_fails_loud() {
        let mut t = TwigState::new();
        let (addr, diff) = modified_no_code(Address::with_last_byte(0x11));
        let mut accounts = BTreeMap::new();
        accounts.insert(addr, diff);
        let err = t.apply_diff(&StateDiff { accounts }).unwrap_err();
        assert!(
            err.to_string().contains("read-miss"),
            "a Modified read-miss must fail loud, got: {err}"
        );
    }

    #[test]
    fn modified_account_corrupt_leaf_fails_loud() {
        let mut t = TwigState::new();
        let addr = Address::with_last_byte(0x12);
        let key = account_key(&addr).0;
        t.inner.set(key, &[0xAA, 0xBB]);
        let (addr, diff) = modified_no_code(addr);
        let mut accounts = BTreeMap::new();
        accounts.insert(addr, diff);

        let err = t.apply_diff(&StateDiff { accounts }).unwrap_err();
        assert!(err.to_string().contains("corrupt"));
        assert_eq!(t.version(), 0, "validation must precede tree mutation");
    }

    /// F5 companion: a `Created` account with no code_change legitimately
    /// defaults to `EMPTY_CODE_HASH` and still applies.
    #[test]
    fn created_account_with_no_code_defaults_empty() {
        let mut t = TwigState::new();
        let addr = Address::with_last_byte(0x22);
        let mut accounts = BTreeMap::new();
        accounts.insert(addr, created(addr, 5).1);
        let (v, _root) = t.apply_diff(&StateDiff { accounts }).unwrap();
        assert_eq!(v, 1);
        let key = account_key(&addr).0;
        assert_eq!(decode_code_hash(t.get(&key).unwrap()), EMPTY_CODE_HASH);
    }
}
