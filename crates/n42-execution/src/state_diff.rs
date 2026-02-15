use alloy_primitives::{Address, U256, B256};
use revm::database::BundleState;
use std::collections::BTreeMap;

/// State diff for a single block: all account and storage changes.
///
/// Extracted from revm's `BundleState` after block execution. This provides
/// a compact representation of what changed, useful for:
/// - Light node incremental sync
/// - Mobile device state updates
/// - Audit/debugging of state transitions
#[derive(Debug, Clone, Default)]
pub struct StateDiff {
    /// Per-account changes in this block.
    pub accounts: BTreeMap<Address, AccountDiff>,
}

/// Changes to a single account in a block.
#[derive(Debug, Clone)]
pub struct AccountDiff {
    /// The type of change to this account.
    pub change_type: AccountChangeType,
    /// Balance before and after (if account existed).
    pub balance: Option<ValueChange<U256>>,
    /// Nonce before and after (if account existed).
    pub nonce: Option<ValueChange<u64>>,
    /// Code change: None if unchanged, Some with old/new bytecode hashes.
    pub code_change: Option<ValueChange<Option<B256>>>,
    /// Storage slot changes for this account.
    pub storage: BTreeMap<U256, ValueChange<U256>>,
}

/// Type of account-level change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountChangeType {
    /// Account was created in this block.
    Created,
    /// Account was modified (balance, nonce, code, or storage changed).
    Modified,
    /// Account was destroyed (via SELFDESTRUCT or equivalent).
    Destroyed,
}

/// A before/after pair for tracking a value change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValueChange<T> {
    /// Value before the block execution.
    pub from: T,
    /// Value after the block execution.
    pub to: T,
}

impl<T> ValueChange<T> {
    /// Creates a new value change.
    pub fn new(from: T, to: T) -> Self {
        Self { from, to }
    }
}

impl StateDiff {
    /// Returns the number of changed accounts.
    pub fn len(&self) -> usize {
        self.accounts.len()
    }

    /// Returns true if no accounts changed.
    pub fn is_empty(&self) -> bool {
        self.accounts.is_empty()
    }

    /// Extracts a state diff from a revm `BundleState`.
    ///
    /// The `BundleState` tracks all account and storage changes with their
    /// original values, which allows us to construct a complete before/after diff.
    pub fn from_bundle_state(bundle: &BundleState) -> Self {
        let mut accounts = BTreeMap::new();

        for (address, bundle_account) in &bundle.state {
            // Determine account change type
            let was_destroyed = bundle_account.was_destroyed();
            let original_info = bundle_account.original_info.as_ref();
            let current_info = bundle_account.info.as_ref();

            // Validate: destroyed accounts should not have current info
            debug_assert!(
                !(was_destroyed && current_info.is_some()),
                "account {address:?} marked destroyed but still has current info"
            );

            let change_type = if was_destroyed {
                AccountChangeType::Destroyed
            } else if original_info.is_none() && current_info.is_some() {
                AccountChangeType::Created
            } else {
                AccountChangeType::Modified
            };

            // Extract balance change
            let balance = match (original_info, current_info) {
                (Some(orig), Some(curr)) => Some(ValueChange::new(orig.balance, curr.balance)),
                (None, Some(curr)) => Some(ValueChange::new(U256::ZERO, curr.balance)),
                (Some(orig), None) => Some(ValueChange::new(orig.balance, U256::ZERO)),
                (None, None) => None,
            };

            // Extract nonce change
            let nonce = match (original_info, current_info) {
                (Some(orig), Some(curr)) => Some(ValueChange::new(orig.nonce, curr.nonce)),
                (None, Some(curr)) => Some(ValueChange::new(0, curr.nonce)),
                (Some(orig), None) => Some(ValueChange::new(orig.nonce, 0)),
                (None, None) => None,
            };

            // Extract code change (track by code hash)
            let code_change = match (original_info, current_info) {
                (Some(orig), Some(curr)) if orig.code_hash != curr.code_hash => {
                    Some(ValueChange::new(
                        Some(orig.code_hash),
                        Some(curr.code_hash),
                    ))
                }
                (None, Some(curr))
                    if curr.code_hash != revm::primitives::KECCAK_EMPTY =>
                {
                    Some(ValueChange::new(None, Some(curr.code_hash)))
                }
                (Some(orig), None)
                    if orig.code_hash != revm::primitives::KECCAK_EMPTY =>
                {
                    Some(ValueChange::new(Some(orig.code_hash), None))
                }
                _ => None,
            };

            // Extract storage changes (skip unchanged slots)
            let mut storage = BTreeMap::new();
            for (slot, slot_value) in &bundle_account.storage {
                if slot_value.previous_or_original_value == slot_value.present_value {
                    continue;
                }
                storage.insert(
                    *slot,
                    ValueChange::new(
                        slot_value.previous_or_original_value,
                        slot_value.present_value,
                    ),
                );
            }

            // Only include accounts that actually changed
            let has_changes = balance
                .as_ref()
                .is_some_and(|v| v.from != v.to)
                || nonce.as_ref().is_some_and(|v| v.from != v.to)
                || code_change.is_some()
                || !storage.is_empty()
                || was_destroyed;

            if has_changes {
                accounts.insert(
                    *address,
                    AccountDiff {
                        change_type,
                        balance,
                        nonce,
                        code_change,
                        storage,
                    },
                );
            }
        }

        Self { accounts }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, B256, U256};
    use revm::database::{AccountStatus, BundleAccount, BundleState, StorageWithOriginalValues};
    use revm::database::states::StorageSlot;
    use revm::state::AccountInfo;

    /// Helper: create an AccountInfo with the given balance and nonce.
    fn make_account_info(balance: u64, nonce: u64) -> AccountInfo {
        AccountInfo {
            balance: U256::from(balance),
            nonce,
            code_hash: revm::primitives::KECCAK_EMPTY,
            code: None,
            account_id: None,
        }
    }

    /// Helper: create a BundleAccount for a newly created account.
    fn created_account(balance: u64, nonce: u64) -> BundleAccount {
        BundleAccount {
            info: Some(make_account_info(balance, nonce)),
            original_info: None,
            storage: StorageWithOriginalValues::default(),
            status: AccountStatus::InMemoryChange,
        }
    }

    /// Helper: create a BundleAccount for a modified account.
    fn modified_account(
        orig_balance: u64,
        orig_nonce: u64,
        new_balance: u64,
        new_nonce: u64,
    ) -> BundleAccount {
        BundleAccount {
            info: Some(make_account_info(new_balance, new_nonce)),
            original_info: Some(make_account_info(orig_balance, orig_nonce)),
            storage: StorageWithOriginalValues::default(),
            status: AccountStatus::Changed,
        }
    }

    /// Helper: create a BundleAccount for a destroyed account.
    fn destroyed_account(orig_balance: u64, orig_nonce: u64) -> BundleAccount {
        BundleAccount {
            info: None,
            original_info: Some(make_account_info(orig_balance, orig_nonce)),
            storage: StorageWithOriginalValues::default(),
            status: AccountStatus::Destroyed,
        }
    }

    /// Helper: create a storage slot change.
    fn storage_slot(from: u64, to: u64) -> StorageSlot {
        StorageSlot::new_changed(U256::from(from), U256::from(to))
    }

    #[test]
    fn test_value_change_new() {
        let vc = ValueChange::new(10u64, 20u64);
        assert_eq!(vc.from, 10);
        assert_eq!(vc.to, 20);
    }

    #[test]
    fn test_value_change_equality() {
        let vc1 = ValueChange::new(U256::from(5), U256::from(10));
        let vc2 = ValueChange::new(U256::from(5), U256::from(10));
        let vc3 = ValueChange::new(U256::from(5), U256::from(20));

        assert_eq!(vc1, vc2, "same from/to should be equal");
        assert_ne!(vc1, vc3, "different 'to' should not be equal");
    }

    #[test]
    fn test_account_change_type_equality() {
        assert_eq!(AccountChangeType::Created, AccountChangeType::Created);
        assert_eq!(AccountChangeType::Modified, AccountChangeType::Modified);
        assert_eq!(AccountChangeType::Destroyed, AccountChangeType::Destroyed);
        assert_ne!(AccountChangeType::Created, AccountChangeType::Modified);
        assert_ne!(AccountChangeType::Modified, AccountChangeType::Destroyed);
    }

    #[test]
    fn test_state_diff_default_empty() {
        let diff = StateDiff::default();
        assert!(diff.is_empty(), "default state diff should be empty");
        assert_eq!(diff.len(), 0);
    }

    #[test]
    fn test_state_diff_from_empty_bundle() {
        let bundle = BundleState::default();
        let diff = StateDiff::from_bundle_state(&bundle);
        assert!(diff.is_empty(), "empty bundle should produce empty diff");
    }

    #[test]
    fn test_state_diff_created_account() {
        let addr = Address::with_last_byte(1);
        let mut bundle = BundleState::default();
        bundle.state.insert(addr, created_account(1000, 0));

        let diff = StateDiff::from_bundle_state(&bundle);
        assert_eq!(diff.len(), 1, "should have one changed account");

        let account_diff = diff.accounts.get(&addr).expect("addr should be in diff");
        assert_eq!(account_diff.change_type, AccountChangeType::Created);

        // Balance: 0 → 1000
        let bal = account_diff.balance.as_ref().expect("should have balance change");
        assert_eq!(bal.from, U256::ZERO);
        assert_eq!(bal.to, U256::from(1000));

        // Nonce: 0 → 0 (no change, but still recorded)
        let nc = account_diff.nonce.as_ref().expect("should have nonce");
        assert_eq!(nc.from, 0);
        assert_eq!(nc.to, 0);
    }

    #[test]
    fn test_state_diff_modified_account() {
        let addr = Address::with_last_byte(2);
        let mut bundle = BundleState::default();
        bundle
            .state
            .insert(addr, modified_account(500, 1, 1000, 2));

        let diff = StateDiff::from_bundle_state(&bundle);
        assert_eq!(diff.len(), 1);

        let account_diff = diff.accounts.get(&addr).unwrap();
        assert_eq!(account_diff.change_type, AccountChangeType::Modified);

        let bal = account_diff.balance.as_ref().unwrap();
        assert_eq!(bal.from, U256::from(500));
        assert_eq!(bal.to, U256::from(1000));

        let nc = account_diff.nonce.as_ref().unwrap();
        assert_eq!(nc.from, 1);
        assert_eq!(nc.to, 2);
    }

    #[test]
    fn test_state_diff_destroyed_account() {
        let addr = Address::with_last_byte(3);
        let mut bundle = BundleState::default();
        bundle.state.insert(addr, destroyed_account(2000, 5));

        let diff = StateDiff::from_bundle_state(&bundle);
        assert_eq!(diff.len(), 1);

        let account_diff = diff.accounts.get(&addr).unwrap();
        assert_eq!(account_diff.change_type, AccountChangeType::Destroyed);

        let bal = account_diff.balance.as_ref().unwrap();
        assert_eq!(bal.from, U256::from(2000));
        assert_eq!(bal.to, U256::ZERO);
    }

    #[test]
    fn test_state_diff_storage_changes() {
        let addr = Address::with_last_byte(4);
        let mut account = modified_account(100, 1, 200, 2);
        account
            .storage
            .insert(U256::from(0), storage_slot(42, 99));
        account
            .storage
            .insert(U256::from(1), storage_slot(0, 100));

        let mut bundle = BundleState::default();
        bundle.state.insert(addr, account);

        let diff = StateDiff::from_bundle_state(&bundle);
        let account_diff = diff.accounts.get(&addr).unwrap();

        assert_eq!(
            account_diff.storage.len(),
            2,
            "should have 2 storage slot changes"
        );

        let slot0 = account_diff.storage.get(&U256::from(0)).unwrap();
        assert_eq!(slot0.from, U256::from(42));
        assert_eq!(slot0.to, U256::from(99));

        let slot1 = account_diff.storage.get(&U256::from(1)).unwrap();
        assert_eq!(slot1.from, U256::ZERO);
        assert_eq!(slot1.to, U256::from(100));
    }

    #[test]
    fn test_state_diff_no_actual_change_filtered() {
        // An account where nothing actually changed (balance, nonce, code all same)
        // should be filtered out.
        let addr = Address::with_last_byte(5);
        let account = modified_account(100, 1, 100, 1); // same values

        let mut bundle = BundleState::default();
        bundle.state.insert(addr, account);

        let diff = StateDiff::from_bundle_state(&bundle);
        assert!(
            diff.is_empty(),
            "account with no actual changes should be filtered out"
        );
    }

    #[test]
    fn test_state_diff_code_change() {
        let addr = Address::with_last_byte(6);
        let old_hash = B256::repeat_byte(0xAA);
        let new_hash = B256::repeat_byte(0xBB);

        let account = BundleAccount {
            info: Some(AccountInfo {
                balance: U256::from(100),
                nonce: 1,
                code_hash: new_hash,
                code: None,
                account_id: None,
            }),
            original_info: Some(AccountInfo {
                balance: U256::from(100),
                nonce: 1,
                code_hash: old_hash,
                code: None,
                account_id: None,
            }),
            storage: StorageWithOriginalValues::default(),
            status: AccountStatus::Changed,
        };

        let mut bundle = BundleState::default();
        bundle.state.insert(addr, account);

        let diff = StateDiff::from_bundle_state(&bundle);
        assert_eq!(diff.len(), 1);

        let account_diff = diff.accounts.get(&addr).unwrap();
        let code_change = account_diff
            .code_change
            .as_ref()
            .expect("should have code change");
        assert_eq!(code_change.from, Some(old_hash));
        assert_eq!(code_change.to, Some(new_hash));
    }

    #[test]
    fn test_state_diff_multiple_accounts() {
        let addr1 = Address::with_last_byte(10);
        let addr2 = Address::with_last_byte(20);
        let addr3 = Address::with_last_byte(30);

        let mut bundle = BundleState::default();
        bundle.state.insert(addr1, created_account(100, 0));
        bundle
            .state
            .insert(addr2, modified_account(200, 1, 300, 2));
        bundle.state.insert(addr3, destroyed_account(400, 3));

        let diff = StateDiff::from_bundle_state(&bundle);
        assert_eq!(diff.len(), 3, "should have 3 changed accounts");

        assert_eq!(
            diff.accounts[&addr1].change_type,
            AccountChangeType::Created
        );
        assert_eq!(
            diff.accounts[&addr2].change_type,
            AccountChangeType::Modified
        );
        assert_eq!(
            diff.accounts[&addr3].change_type,
            AccountChangeType::Destroyed
        );
    }
}
