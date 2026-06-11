//! `StateDiff` bridge for the all-DRAM twig engine ([`n42_twig_core`]).
//!
//! Wraps [`ShardedTwig`] with the n42 account/storage encoding so the node can
//! drive it the same way it drives [`crate::ShardedSbmt`]: `apply_diff` flattens a
//! `StateDiff` into canonical `(key, Option<value>)` ops (account/storage keys via
//! the shared `account_key`/`storage_key`, `code_hash` read back from the unified
//! KV when a `Modified` account has no `code_change`), then applies them in
//! keyHash-sorted order. See `docs/devlog-64` (P6).

use crate::keys::{account_key, storage_key};
use crate::tree::{EMPTY_CODE_HASH, decode_code_hash, encode_account_value};

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
    pub fn apply_diff(&mut self, diff: &StateDiff) -> (u64, B256) {
        let ops = self.prepare(diff);
        let (version, root) = self.inner.apply_batch(&ops);
        (version, B256::from(root))
    }

    /// Flatten a `StateDiff` into `(key, Option<value>)` ops (mirrors
    /// [`crate::ShardedSbmt`]'s prepare; `code_hash` read back from the unified KV).
    fn prepare(&self, diff: &StateDiff) -> Vec<(Hash, Option<Vec<u8>>)> {
        let mut ops: Vec<(Hash, Option<Vec<u8>>)> = Vec::new();
        for (address, account_diff) in &diff.accounts {
            let key = account_key(address).0;
            match account_diff.change_type {
                AccountChangeType::Destroyed => {
                    ops.push((key, None));
                    for slot in account_diff.storage.keys() {
                        ops.push((storage_key(address, slot).0, None));
                    }
                }
                AccountChangeType::Created | AccountChangeType::Modified => {
                    let balance = account_diff.balance.as_ref().map(|v| v.to).unwrap_or_default();
                    let nonce = account_diff.nonce.as_ref().map(|v| v.to).unwrap_or(0);
                    let code_hash = match &account_diff.code_change {
                        Some(change) => change.to.unwrap_or(EMPTY_CODE_HASH),
                        None => self.inner.get(&key).map(decode_code_hash).unwrap_or(EMPTY_CODE_HASH),
                    };
                    ops.push((key, Some(encode_account_value(&balance, nonce, &code_hash))));
                    for (slot, change) in &account_diff.storage {
                        let skey = storage_key(address, slot).0;
                        if change.to.is_zero() {
                            ops.push((skey, None));
                        } else {
                            ops.push((skey, Some(change.to.to_be_bytes::<32>().to_vec())));
                        }
                    }
                }
            }
        }
        ops
    }

    /// Build a key-bindable proof for a derived `key` (requires a prior root call).
    pub fn prove(&self, key: Hash) -> Option<ShardedTwigProof> {
        self.inner.prove(&key)
    }

    pub fn snapshot(&self) -> TwigSnapshot {
        self.inner.snapshot()
    }

    pub fn from_snapshot(snap: &TwigSnapshot) -> Self {
        Self {
            inner: ShardedTwig::from_snapshot(snap),
        }
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
        let (v, root) = t.apply_diff(&diff);
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
        t.apply_diff(&StateDiff { accounts });
        let root = t.root();
        let snap = t.snapshot();
        let mut restored = TwigState::from_snapshot(&snap);
        assert_eq!(restored.root(), root);
        assert_eq!(restored.version(), t.version());
    }
}
