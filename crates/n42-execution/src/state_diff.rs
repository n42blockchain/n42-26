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

            // Extract storage changes
            let mut storage = BTreeMap::new();
            for (slot, slot_value) in &bundle_account.storage {
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
