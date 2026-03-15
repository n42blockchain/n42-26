use crate::hasher::Blake3Hasher;
use crate::keys::{account_key, storage_key};
use crate::metrics;
use crate::store::MemTreeStore;

use alloy_primitives::B256;
use jmt::storage::TreeWriter;
use jmt::{JellyfishMerkleTree, KeyHash, OwnedValue, Version};
use n42_execution::state_diff::{AccountChangeType, StateDiff};
use std::sync::Arc;
use std::time::Instant;
use tracing::debug;

/// Fixed-size account leaf value: 72 bytes.
/// Format: `[balance_32_be][nonce_8_be][code_hash_32]`
const ACCOUNT_VALUE_LEN: usize = 72;

/// Encodes an account leaf value for JMT storage.
///
/// Format: `[balance_32_be][nonce_8_be][code_hash_32_or_zeros]` = 72 bytes fixed.
/// Compact, deterministic, no serde overhead.
pub fn encode_account_value(
    balance: &alloy_primitives::U256,
    nonce: u64,
    code_hash: &B256,
) -> OwnedValue {
    let mut buf = Vec::with_capacity(ACCOUNT_VALUE_LEN);
    buf.extend_from_slice(&balance.to_be_bytes::<32>());
    buf.extend_from_slice(&nonce.to_be_bytes());
    buf.extend_from_slice(code_hash.as_slice());
    buf
}

/// Decode the code_hash field from a 72-byte account leaf value.
/// Returns `B256::ZERO` if the value is too short (shouldn't happen with well-formed data).
pub fn decode_code_hash(value: &[u8]) -> B256 {
    if value.len() >= ACCOUNT_VALUE_LEN {
        B256::from_slice(&value[40..72])
    } else {
        B256::ZERO
    }
}

/// Helper to convert anyhow::Error to eyre::Report.
fn to_eyre(e: anyhow::Error) -> eyre::Report {
    eyre::eyre!("{e:#}")
}

/// N42 JMT tree backed by an in-memory store.
///
/// Maintains a versioned Jellyfish Merkle Tree using Blake3 hashing.
/// Each block application increments the version and produces a new root hash.
///
/// The tree is updated incrementally from `StateDiff` (extracted from revm BundleState),
/// so only changed accounts/slots are touched per block.
pub struct N42JmtTree {
    store: Arc<MemTreeStore>,
    version: Version,
}

impl N42JmtTree {
    /// Create a new empty JMT tree at version 0.
    pub fn new() -> Self {
        Self {
            store: Arc::new(MemTreeStore::new()),
            version: 0,
        }
    }

    /// Create a JMT tree wrapping an existing store (for shard use).
    pub fn with_store(store: Arc<MemTreeStore>) -> Self {
        Self { store, version: 0 }
    }

    /// Current tree version (= number of blocks applied).
    pub fn version(&self) -> Version {
        self.version
    }

    /// Current root hash as `B256`. Returns `B256::ZERO` for empty tree (no data written yet).
    pub fn root_hash(&self) -> eyre::Result<B256> {
        if self.store.node_count() == 0 {
            return Ok(B256::ZERO);
        }
        let start = Instant::now();
        let tree = JellyfishMerkleTree::<_, Blake3Hasher>::new(self.store.as_ref());
        let root = tree.get_root_hash(self.version).map_err(to_eyre)?;
        let ms = start.elapsed().as_secs_f64() * 1000.0;
        metrics::record_root_computation(ms);
        Ok(B256::from(root.0))
    }

    /// Apply a `StateDiff` to the tree, producing a new version and root hash.
    ///
    /// For Modified accounts, if `code_change` is `None` (code unchanged),
    /// the existing code_hash is read from the current leaf to preserve it.
    ///
    /// Returns `(new_version, jmt_root_hash)`.
    pub fn apply_diff(&mut self, diff: &StateDiff) -> eyre::Result<(Version, B256)> {
        let start = Instant::now();
        let new_version = self.version + 1;
        let mut updates: Vec<(KeyHash, Option<OwnedValue>)> = Vec::new();

        for (address, account_diff) in &diff.accounts {
            let key = account_key(address);

            match account_diff.change_type {
                AccountChangeType::Destroyed => {
                    updates.push((key, None));
                    for (slot, _) in &account_diff.storage {
                        updates.push((storage_key(address, slot), None));
                    }
                }
                AccountChangeType::Created | AccountChangeType::Modified => {
                    let balance = account_diff
                        .balance
                        .as_ref()
                        .map(|v| v.to)
                        .unwrap_or_default();
                    let nonce = account_diff.nonce.as_ref().map(|v| v.to).unwrap_or(0);

                    // Resolve code_hash: use new value if changed, otherwise
                    // read existing code_hash from current leaf to preserve it.
                    let code_hash = match &account_diff.code_change {
                        Some(change) => change.to.unwrap_or(B256::ZERO),
                        None => {
                            // Code unchanged — read existing code_hash from tree.
                            self.get(key)?
                                .as_deref()
                                .map(decode_code_hash)
                                .unwrap_or(B256::ZERO)
                        }
                    };

                    let value = encode_account_value(&balance, nonce, &code_hash);
                    updates.push((key, Some(value)));

                    for (slot, change) in &account_diff.storage {
                        let skey = storage_key(address, slot);
                        if change.to.is_zero() {
                            updates.push((skey, None));
                        } else {
                            updates.push((skey, Some(change.to.to_be_bytes::<32>().to_vec())));
                        }
                    }
                }
            }
        }

        let leaf_count = updates.len();

        // Always bump version to stay aligned with block numbers,
        // even for empty diffs (empty blocks).
        if updates.is_empty() {
            debug!(target: "n42::jmt", version = new_version, "empty diff, advancing version");
            // Write an empty value set to create a root node at the new version.
            let tree = JellyfishMerkleTree::<_, Blake3Hasher>::new(self.store.as_ref());
            let (root_hash, batch) = tree
                .put_value_set(
                    std::iter::empty::<(KeyHash, Option<OwnedValue>)>(),
                    new_version,
                )
                .map_err(to_eyre)?;
            self.store
                .write_node_batch(&batch.node_batch)
                .map_err(to_eyre)?;
            self.version = new_version;
            return Ok((new_version, B256::from(root_hash.0)));
        }

        let tree = JellyfishMerkleTree::<_, Blake3Hasher>::new(self.store.as_ref());
        let (root_hash, batch) = tree.put_value_set(updates, new_version).map_err(to_eyre)?;
        self.store
            .write_node_batch(&batch.node_batch)
            .map_err(to_eyre)?;
        self.version = new_version;

        let ms = start.elapsed().as_secs_f64() * 1000.0;
        let root = B256::from(root_hash.0);

        metrics::record_update(new_version, leaf_count, ms);
        debug!(
            target: "n42::jmt",
            version = new_version,
            leaves = leaf_count,
            update_ms = format!("{ms:.2}"),
            root = %root,
            "JMT updated"
        );

        Ok((new_version, root))
    }

    /// Apply a raw key-value batch. Used by sharded tree for partition updates.
    ///
    /// Empty batches are no-ops: shard version stays unchanged, returns current root.
    /// The *global* version is tracked by `ShardedJmt`, not individual shards.
    pub fn apply_batch(
        &mut self,
        updates: Vec<(KeyHash, Option<OwnedValue>)>,
    ) -> eyre::Result<(Version, B256)> {
        if updates.is_empty() {
            return self.root_hash().map(|r| (self.version, r));
        }

        let new_version = self.version + 1;
        let tree = JellyfishMerkleTree::<_, Blake3Hasher>::new(self.store.as_ref());
        let (root_hash, batch) = tree.put_value_set(updates, new_version).map_err(to_eyre)?;
        self.store
            .write_node_batch(&batch.node_batch)
            .map_err(to_eyre)?;
        self.version = new_version;
        Ok((new_version, B256::from(root_hash.0)))
    }

    /// Generate an inclusion/exclusion proof for a key at the current version.
    ///
    /// Returns `None` if the tree is empty (no data written yet).
    pub fn get_proof(
        &self,
        key_hash: KeyHash,
    ) -> eyre::Result<Option<jmt::proof::SparseMerkleProof<Blake3Hasher>>> {
        if self.store.node_count() == 0 {
            return Ok(None);
        }
        let tree = JellyfishMerkleTree::<_, Blake3Hasher>::new(self.store.as_ref());
        let (_, proof) = tree
            .get_with_proof(key_hash, self.version)
            .map_err(to_eyre)?;
        Ok(Some(proof))
    }

    /// Get the value for a key at the current version.
    ///
    /// Uses `TreeReader::get_value_option` directly instead of `get_with_proof`
    /// to avoid unnecessary proof computation on the hot path.
    pub fn get(&self, key_hash: KeyHash) -> eyre::Result<Option<OwnedValue>> {
        use jmt::storage::TreeReader;
        self.store
            .get_value_option(self.version, key_hash)
            .map_err(to_eyre)
    }

    /// Access the underlying store (for shard merging, persistence, etc.).
    pub fn store(&self) -> &Arc<MemTreeStore> {
        &self.store
    }
}

impl Default for N42JmtTree {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, U256};
    use n42_execution::state_diff::{AccountDiff, ValueChange};
    use std::collections::BTreeMap;

    fn simple_created_diff(addr: Address, balance: u64, nonce: u64) -> StateDiff {
        let mut accounts = BTreeMap::new();
        accounts.insert(
            addr,
            AccountDiff {
                change_type: AccountChangeType::Created,
                balance: Some(ValueChange::new(U256::ZERO, U256::from(balance))),
                nonce: Some(ValueChange::new(0, nonce)),
                code_change: None,
                storage: BTreeMap::new(),
            },
        );
        StateDiff { accounts }
    }

    #[test]
    fn new_tree_version_zero() {
        let tree = N42JmtTree::new();
        assert_eq!(tree.version(), 0);
    }

    #[test]
    fn apply_single_account() {
        let mut tree = N42JmtTree::new();
        let addr = Address::repeat_byte(0x01);
        let diff = simple_created_diff(addr, 1000, 1);

        let (version, root) = tree.apply_diff(&diff).unwrap();
        assert_eq!(version, 1);
        assert_ne!(root, B256::ZERO);

        let key = account_key(&addr);
        let value = tree.get(key).unwrap();
        assert!(value.is_some());
    }

    #[test]
    fn different_diffs_produce_different_roots() {
        let mut tree1 = N42JmtTree::new();
        let mut tree2 = N42JmtTree::new();

        let d1 = simple_created_diff(Address::repeat_byte(0x01), 100, 0);
        let d2 = simple_created_diff(Address::repeat_byte(0x02), 200, 0);

        let (_, r1) = tree1.apply_diff(&d1).unwrap();
        let (_, r2) = tree2.apply_diff(&d2).unwrap();
        assert_ne!(r1, r2);
    }

    #[test]
    fn destroy_removes_account() {
        let mut tree = N42JmtTree::new();
        let addr = Address::repeat_byte(0x01);

        let d1 = simple_created_diff(addr, 1000, 1);
        tree.apply_diff(&d1).unwrap();
        assert!(tree.get(account_key(&addr)).unwrap().is_some());

        let mut accounts = BTreeMap::new();
        accounts.insert(
            addr,
            AccountDiff {
                change_type: AccountChangeType::Destroyed,
                balance: Some(ValueChange::new(U256::from(1000), U256::ZERO)),
                nonce: Some(ValueChange::new(1, 0)),
                code_change: None,
                storage: BTreeMap::new(),
            },
        );
        let d2 = StateDiff { accounts };
        tree.apply_diff(&d2).unwrap();

        assert!(tree.get(account_key(&addr)).unwrap().is_none());
    }

    #[test]
    fn storage_slot_updates() {
        let mut tree = N42JmtTree::new();
        let addr = Address::repeat_byte(0x01);

        let mut accounts = BTreeMap::new();
        let mut storage = BTreeMap::new();
        storage.insert(U256::from(0), ValueChange::new(U256::ZERO, U256::from(42)));
        accounts.insert(
            addr,
            AccountDiff {
                change_type: AccountChangeType::Created,
                balance: Some(ValueChange::new(U256::ZERO, U256::from(100))),
                nonce: Some(ValueChange::new(0, 0)),
                code_change: None,
                storage,
            },
        );
        let diff = StateDiff { accounts };

        let (_, root) = tree.apply_diff(&diff).unwrap();
        assert_ne!(root, B256::ZERO);

        let skey = storage_key(&addr, &U256::from(0));
        let val = tree.get(skey).unwrap();
        assert!(val.is_some());
    }

    #[test]
    fn proof_generation() {
        let mut tree = N42JmtTree::new();
        let addr = Address::repeat_byte(0x01);
        let diff = simple_created_diff(addr, 500, 1);
        tree.apply_diff(&diff).unwrap();

        let key = account_key(&addr);
        let proof = tree.get_proof(key).unwrap().expect("tree should have data");

        let root = tree.root_hash().unwrap();
        let value = tree.get(key).unwrap();
        proof
            .verify(jmt::RootHash(root.0), key, value.as_deref())
            .expect("proof should verify");
    }

    #[test]
    fn empty_diff_bumps_version() {
        let mut tree = N42JmtTree::new();
        // Apply a non-empty diff first so the tree has a root.
        let d1 = simple_created_diff(Address::repeat_byte(0x01), 100, 0);
        let (v1, r1) = tree.apply_diff(&d1).unwrap();
        assert_eq!(v1, 1);

        // Empty diff should still bump version.
        let d2 = StateDiff::default();
        let (v2, r2) = tree.apply_diff(&d2).unwrap();
        assert_eq!(v2, 2, "version must bump even for empty diff");
        assert_eq!(r1, r2, "root should not change for empty diff");
    }

    #[test]
    fn modified_account_preserves_code_hash() {
        let mut tree = N42JmtTree::new();
        let addr = Address::repeat_byte(0x01);
        let code_hash = B256::repeat_byte(0xAA);

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
        tree.apply_diff(&StateDiff { accounts }).unwrap();

        // Verify code_hash is stored.
        let key = account_key(&addr);
        let val = tree.get(key).unwrap().unwrap();
        assert_eq!(decode_code_hash(&val), code_hash);

        // Modify only balance — code_change is None (unchanged).
        let mut accounts2 = BTreeMap::new();
        accounts2.insert(
            addr,
            AccountDiff {
                change_type: AccountChangeType::Modified,
                balance: Some(ValueChange::new(U256::from(100), U256::from(200))),
                nonce: None,
                code_change: None, // code not changed
                storage: BTreeMap::new(),
            },
        );
        tree.apply_diff(&StateDiff {
            accounts: accounts2,
        })
        .unwrap();

        // code_hash should still be 0xAA, not zeroed.
        let val2 = tree.get(key).unwrap().unwrap();
        assert_eq!(
            decode_code_hash(&val2),
            code_hash,
            "code_hash must be preserved when code_change is None"
        );
    }

    #[test]
    fn code_change_updates_hash() {
        let mut tree = N42JmtTree::new();
        let addr = Address::repeat_byte(0x01);
        let old_hash = B256::repeat_byte(0xAA);
        let new_hash = B256::repeat_byte(0xBB);

        // Create with old code hash.
        let mut accounts = BTreeMap::new();
        accounts.insert(
            addr,
            AccountDiff {
                change_type: AccountChangeType::Created,
                balance: Some(ValueChange::new(U256::ZERO, U256::from(100))),
                nonce: Some(ValueChange::new(0, 0)),
                code_change: Some(ValueChange::new(None, Some(old_hash))),
                storage: BTreeMap::new(),
            },
        );
        tree.apply_diff(&StateDiff { accounts }).unwrap();

        // Modify code.
        let mut accounts2 = BTreeMap::new();
        accounts2.insert(
            addr,
            AccountDiff {
                change_type: AccountChangeType::Modified,
                balance: None,
                nonce: None,
                code_change: Some(ValueChange::new(Some(old_hash), Some(new_hash))),
                storage: BTreeMap::new(),
            },
        );
        tree.apply_diff(&StateDiff {
            accounts: accounts2,
        })
        .unwrap();

        let key = account_key(&addr);
        let val = tree.get(key).unwrap().unwrap();
        assert_eq!(decode_code_hash(&val), new_hash);
    }

    #[test]
    fn delete_then_recreate() {
        let mut tree = N42JmtTree::new();
        let addr = Address::repeat_byte(0x42);

        // Create.
        let d1 = simple_created_diff(addr, 500, 1);
        tree.apply_diff(&d1).unwrap();
        let key = account_key(&addr);
        assert!(tree.get(key).unwrap().is_some());

        // Destroy.
        let mut accounts = BTreeMap::new();
        accounts.insert(
            addr,
            AccountDiff {
                change_type: AccountChangeType::Destroyed,
                balance: Some(ValueChange::new(U256::from(500), U256::ZERO)),
                nonce: Some(ValueChange::new(1, 0)),
                code_change: None,
                storage: BTreeMap::new(),
            },
        );
        tree.apply_diff(&StateDiff { accounts }).unwrap();
        assert!(tree.get(key).unwrap().is_none());

        // Recreate with different balance.
        let d3 = simple_created_diff(addr, 999, 0);
        tree.apply_diff(&d3).unwrap();
        let val = tree.get(key).unwrap().unwrap();
        // Verify balance is 999, not 500.
        let balance_bytes = &val[0..32];
        let balance = alloy_primitives::U256::from_be_slice(balance_bytes);
        assert_eq!(balance, U256::from(999));
    }

    #[test]
    fn multi_block_incremental_updates() {
        let mut tree = N42JmtTree::new();

        // Block 1: create 100 accounts.
        let mut accounts = BTreeMap::new();
        for i in 0..100u8 {
            accounts.insert(
                Address::with_last_byte(i),
                AccountDiff {
                    change_type: AccountChangeType::Created,
                    balance: Some(ValueChange::new(U256::ZERO, U256::from(i as u64 * 100))),
                    nonce: Some(ValueChange::new(0, 0)),
                    code_change: None,
                    storage: BTreeMap::new(),
                },
            );
        }
        let (v1, r1) = tree.apply_diff(&StateDiff { accounts }).unwrap();
        assert_eq!(v1, 1);

        // Block 2: modify 50 accounts.
        let mut accounts2 = BTreeMap::new();
        for i in 0..50u8 {
            accounts2.insert(
                Address::with_last_byte(i),
                AccountDiff {
                    change_type: AccountChangeType::Modified,
                    balance: Some(ValueChange::new(
                        U256::from(i as u64 * 100),
                        U256::from(i as u64 * 200),
                    )),
                    nonce: Some(ValueChange::new(0, 1)),
                    code_change: None,
                    storage: BTreeMap::new(),
                },
            );
        }
        let (v2, r2) = tree
            .apply_diff(&StateDiff {
                accounts: accounts2,
            })
            .unwrap();
        assert_eq!(v2, 2);
        assert_ne!(r1, r2, "modified accounts should change root");

        // Block 3: destroy 10 accounts.
        let mut accounts3 = BTreeMap::new();
        for i in 90..100u8 {
            accounts3.insert(
                Address::with_last_byte(i),
                AccountDiff {
                    change_type: AccountChangeType::Destroyed,
                    balance: Some(ValueChange::new(U256::from(i as u64 * 100), U256::ZERO)),
                    nonce: Some(ValueChange::new(0, 0)),
                    code_change: None,
                    storage: BTreeMap::new(),
                },
            );
        }
        let (v3, r3) = tree
            .apply_diff(&StateDiff {
                accounts: accounts3,
            })
            .unwrap();
        assert_eq!(v3, 3);
        assert_ne!(r2, r3);

        // Verify destroyed accounts are gone.
        for i in 90..100u8 {
            let key = account_key(&Address::with_last_byte(i));
            assert!(
                tree.get(key).unwrap().is_none(),
                "account {i} should be deleted"
            );
        }
        // Verify surviving accounts still exist.
        for i in 0..50u8 {
            let key = account_key(&Address::with_last_byte(i));
            assert!(tree.get(key).unwrap().is_some(), "account {i} should exist");
        }
    }
}
