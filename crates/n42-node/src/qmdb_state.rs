//! Conversion from reth execution changes to gov5 replay-v2 QMDB mutations.

use alloy_eips::{
    eip2935::{HISTORY_STORAGE_ADDRESS, HISTORY_STORAGE_CODE},
    eip4788::{BEACON_ROOTS_ADDRESS, BEACON_ROOTS_CODE},
    eip7002::{WITHDRAWAL_REQUEST_PREDEPLOY_ADDRESS, WITHDRAWAL_REQUEST_PREDEPLOY_CODE},
    eip7251::CONSOLIDATION_REQUEST_PREDEPLOY_ADDRESS,
};
use alloy_genesis::Genesis;
use alloy_primitives::{B256, U256, address, keccak256};
use n42_twig_core::qmdb_compat::{
    QmdbCompatTree, QmdbOperation, QmdbOperationError, encode_gov5_account_value, gov5_account_key,
    gov5_storage_key,
};
use reth_ethereum_primitives::Receipt;
use reth_execution_types::BlockExecutionOutput;
use revm::database::states::BundleState;

/// Apply the additional state that Gov5 replay-v2 inserts after sealing the source block-zero
/// header but before executing block one. This state is intentionally not part of the genesis
/// hash; it is, however, part of replay-v2's QMDB positional history and execution PlainState.
pub fn gov5_replay_execution_genesis(genesis: &Genesis) -> Genesis {
    let mut replay = genesis.clone();
    let hardfork_address = address!("4f88c44eeb74fecf4ad37b95a6d81bcae0f3f091");
    let hardfork_amount = U256::from_str_radix("9B18AB5DF7180B6B8000000", 16)
        .expect("fixed hardfork allocation is valid");
    let hardfork_account = replay.alloc.entry(hardfork_address).or_default();
    hardfork_account.balance = hardfork_account
        .balance
        .checked_add(hardfork_amount)
        .expect("fixed hardfork allocation cannot overflow");

    // Gov5 replay-v2 deliberately uses the EEST consolidation bytecode without Alloy's two
    // trailing STOP bytes. Keep this literal profile-specific: the code hash is QMDB consensus
    // data and even semantically inert suffix bytes change the authenticated account leaf.
    let gov5_consolidation_code = alloy_primitives::bytes!(
        "3373fffffffffffffffffffffffffffffffffffffffe1460d35760115f54807fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff1461019a57600182026001905f5b5f82111560685781019083028483029004916001019190604d565b9093900492505050366060146088573661019a573461019a575f5260205ff35b341061019a57600154600101600155600354806004026004013381556001015f358155600101602035815560010160403590553360601b5f5260605f60143760745fa0600101600355005b6003546002548082038060021160e7575060025b5f5b8181146101295782810160040260040181607402815460601b815260140181600101548152602001816002015481526020019060030154905260010160e9565b910180921461013b5790600255610146565b90505f6002555f6003555b5f54807fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff141561017357505f5b6001546001828201116101885750505f61018e565b01600190035b5f555f6001556074025ff35b5f5ffd"
    );

    for (address, code) in [
        (HISTORY_STORAGE_ADDRESS, HISTORY_STORAGE_CODE.clone()),
        (
            WITHDRAWAL_REQUEST_PREDEPLOY_ADDRESS,
            WITHDRAWAL_REQUEST_PREDEPLOY_CODE.clone(),
        ),
        (
            CONSOLIDATION_REQUEST_PREDEPLOY_ADDRESS,
            gov5_consolidation_code,
        ),
        (BEACON_ROOTS_ADDRESS, BEACON_ROOTS_CODE.clone()),
    ] {
        let account = replay.alloc.entry(address).or_default();
        account.nonce = Some(1);
        account.code = Some(code);
    }
    replay
}

/// Build Gov5's deterministic block-zero QMDB state from the canonical genesis allocation.
/// Accounts and non-zero storage slots enter one common key-sorted append batch, matching
/// `QMDBRootComputer.ComputeRoot` during Gov5 genesis seeding.
pub fn gov5_qmdb_genesis_tree(genesis: &Genesis) -> Result<QmdbCompatTree, QmdbOperationError> {
    let mut operations = Vec::new();
    for (address, account) in &genesis.alloc {
        let nonce = account.nonce.unwrap_or_default();
        let code_hash = account
            .code
            .as_ref()
            .map(|code| keccak256(code.as_ref()))
            .unwrap_or(B256::ZERO);
        let account_is_empty = nonce == 0
            && account.balance.is_zero()
            && account.code.as_ref().is_none_or(|code| code.is_empty());
        if !account_is_empty {
            operations.push(QmdbOperation {
                key: gov5_account_key(address.as_ref()),
                value: Some(encode_gov5_account_value(
                    nonce,
                    &account.balance.to_be_bytes(),
                    &code_hash.0,
                )),
            });
        }
        if let Some(storage) = &account.storage {
            for (slot, value) in storage {
                if *value == B256::ZERO {
                    continue;
                }
                operations.push(QmdbOperation {
                    key: gov5_storage_key(address.as_ref(), slot.as_ref()),
                    value: Some(value.to_vec()),
                });
            }
        }
    }
    let mut tree = QmdbCompatTree::new();
    tree.apply_sorted_ops(operations)?;
    Ok(tree)
}

/// Convert one reth execution bundle to the account/storage leaf mutations gov5 applies before
/// computing its replay-v2 QMDB root.
///
/// Gov5 appends every account marked dirty by execution, including an account whose final
/// `AccountInfo` equals its original value. QMDB is positional, so omitting such a no-op account
/// write changes every later slot and the root. Reth preserves the same distinction in
/// `BundleAccount::status`: loaded accounts are reads, while every other status is dirty.
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
        if !account.status.is_not_modified() {
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
    use alloy_genesis::GenesisAccount;
    use alloy_primitives::{Address, U256};
    use revm::{
        database::states::{AccountStatus, BundleAccount, StorageSlot},
        state::AccountInfo,
    };
    use std::collections::BTreeMap;

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

    #[test]
    fn emits_dirty_account_even_when_final_info_is_unchanged() {
        let address = Address::repeat_byte(0x66);
        let info = AccountInfo {
            nonce: 7,
            balance: U256::from(11),
            code_hash: B256::repeat_byte(0x77),
            ..Default::default()
        };
        let account = BundleAccount::new(
            Some(info.clone()),
            Some(info.clone()),
            Default::default(),
            AccountStatus::Changed,
        );
        let mut state = BundleState::default();
        state.state.insert(address, account);

        assert_eq!(
            gov5_qmdb_operations(&state),
            vec![QmdbOperation {
                key: gov5_account_key(&address.into_array()),
                value: Some(encode_gov5_account_value(
                    info.nonce,
                    &info.balance.to_be_bytes(),
                    &info.code_hash.0,
                )),
            }]
        );
    }

    #[test]
    fn runtime_02_genesis_alloc_matches_gov5_qmdb_root() {
        let balance = U256::from_str_radix("1000000000000000000000000000", 10).unwrap();
        let genesis = Genesis {
            alloc: BTreeMap::from([
                (
                    "0x81d4c1f92ddb837cb46f82280d9b491b101fa582"
                        .parse()
                        .unwrap(),
                    GenesisAccount {
                        balance,
                        ..Default::default()
                    },
                ),
                (
                    "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"
                        .parse()
                        .unwrap(),
                    GenesisAccount {
                        balance,
                        ..Default::default()
                    },
                ),
            ]),
            ..Default::default()
        };
        let tree = gov5_qmdb_genesis_tree(&genesis).unwrap();
        assert_eq!(
            B256::from(tree.root()),
            "0x91a450c13f9deab2c9edf5832c96008862e7cc1169599f68461c3ec947099941"
                .parse::<B256>()
                .unwrap()
        );
        assert_eq!(tree.snapshot().entries.len(), 2);

        let replay_tree = gov5_qmdb_genesis_tree(&gov5_replay_execution_genesis(&genesis)).unwrap();
        assert_eq!(replay_tree.snapshot().entries.len(), 7);
    }
}
