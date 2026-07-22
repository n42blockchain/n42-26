//! Conversion from reth execution changes to gov5 replay-v2 QMDB mutations.

use n42_twig_core::qmdb_compat::{
    QmdbOperation, encode_gov5_account_value, gov5_account_key, gov5_storage_key,
};
use reth_ethereum_primitives::Receipt;
use reth_execution_types::BlockExecutionOutput;
use revm::database::states::BundleState;

/// Convert one reth execution bundle to the account/storage leaf mutations gov5 applies before
/// computing its replay-v2 QMDB root.
///
/// A destroyed account has complete storage in the revm bundle, so every known slot is emitted:
/// this preserves the storage wipe even when a slot's `present_value` still equals its original
/// value. Other accounts emit only changed slots. Ordering is deliberately left to
/// `QmdbCompatTree::apply_sorted_ops`, which rejects duplicates and sorts by hashed key exactly
/// once immediately before mutation.
pub fn gov5_qmdb_operations(state: &BundleState) -> Vec<QmdbOperation> {
    let mut operations = Vec::with_capacity(state.state_size);
    for (address, account) in &state.state {
        let address = address.into_array();
        if account.is_info_changed() {
            operations.push(QmdbOperation {
                key: gov5_account_key(&address),
                value: account.info.as_ref().map(|info| {
                    encode_gov5_account_value(
                        info.nonce,
                        &info.balance.to_be_bytes(),
                        &info.code_hash.0,
                    )
                }),
            });
        }

        for (slot, value) in &account.storage {
            if !account.was_destroyed() && !value.is_changed() {
                continue;
            }
            let present = value.present_value();
            operations.push(QmdbOperation {
                key: gov5_storage_key(&address, &slot.to_be_bytes()),
                value: (!account.was_destroyed() || account.info.is_some())
                    .then_some(present)
                    .filter(|present| !present.is_zero())
                    .map(|present| present.to_be_bytes::<32>().to_vec()),
            });
        }
    }
    operations
}

/// Convenience boundary used by a state-root job, keeping the receipt type localized to the
/// concrete Ethereum execution node.
pub fn gov5_qmdb_operations_from_output(
    output: &BlockExecutionOutput<Receipt>,
) -> Vec<QmdbOperation> {
    gov5_qmdb_operations(&output.state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, B256, U256};
    use n42_twig_core::qmdb_compat::QmdbCompatTree;
    use revm::{
        database::states::{AccountStatus, BundleAccount, StorageSlot},
        state::AccountInfo,
    };

    #[test]
    fn converts_changed_account_and_storage_to_gov5_leaf_format() {
        let address = Address::repeat_byte(0x11);
        let slot = U256::from(7);
        let info = AccountInfo {
            nonce: 42,
            balance: U256::from(5_000_000_u64),
            code_hash: B256::repeat_byte(0x33),
            ..Default::default()
        };
        let account = BundleAccount::new(
            None,
            Some(info.clone()),
            [(slot, StorageSlot::new_changed(U256::ZERO, U256::from(9)))]
                .into_iter()
                .collect(),
            AccountStatus::Changed,
        );
        let mut state = BundleState::default();
        state.state.insert(address, account);
        state.state_size = 2;

        let operations = gov5_qmdb_operations(&state);
        assert_eq!(operations.len(), 2);
        let account_key = gov5_account_key(&address.into_array());
        assert!(operations.contains(&QmdbOperation {
            key: account_key,
            value: Some(encode_gov5_account_value(
                info.nonce,
                &info.balance.to_be_bytes(),
                &info.code_hash.0,
            )),
        }));
        assert!(operations.contains(&QmdbOperation {
            key: gov5_storage_key(&address.into_array(), &slot.to_be_bytes()),
            value: Some(U256::from(9).to_be_bytes::<32>().to_vec()),
        }));

        let mut tree = QmdbCompatTree::new();
        assert_ne!(tree.apply_sorted_ops(operations).unwrap(), B256::ZERO.0);
    }

    #[test]
    fn destroyed_account_emits_account_and_complete_storage_deletes() {
        let address = Address::repeat_byte(0x44);
        let slot = U256::from(3);
        let original = AccountInfo {
            balance: U256::from(1),
            ..Default::default()
        };
        let account = BundleAccount::new(
            Some(original),
            None,
            [(slot, StorageSlot::new(U256::from(8)))]
                .into_iter()
                .collect(),
            AccountStatus::Destroyed,
        );
        let mut state = BundleState::default();
        state.state.insert(address, account);

        let operations = gov5_qmdb_operations(&state);
        assert_eq!(operations.len(), 2);
        assert!(operations.iter().all(|operation| operation.value.is_none()));
        assert!(
            operations
                .iter()
                .any(|operation| { operation.key == gov5_account_key(&address.into_array()) })
        );
        assert!(operations.iter().any(|operation| {
            operation.key == gov5_storage_key(&address.into_array(), &slot.to_be_bytes())
        }));
    }

    #[test]
    fn ignores_loaded_unchanged_state() {
        let address = Address::repeat_byte(0x55);
        let info = AccountInfo::default();
        let account = BundleAccount::new(
            Some(info.clone()),
            Some(info),
            [(U256::from(1), StorageSlot::new(U256::from(2)))]
                .into_iter()
                .collect(),
            AccountStatus::Loaded,
        );
        let mut state = BundleState::default();
        state.state.insert(address, account);
        assert!(gov5_qmdb_operations(&state).is_empty());
    }
}
