//! Conversion from reth execution changes to gov5 replay-v2 QMDB mutations.

use alloy_eips::{
    eip2935::{HISTORY_STORAGE_ADDRESS, HISTORY_STORAGE_CODE},
    eip4788::{BEACON_ROOTS_ADDRESS, BEACON_ROOTS_CODE},
    eip7002::{WITHDRAWAL_REQUEST_PREDEPLOY_ADDRESS, WITHDRAWAL_REQUEST_PREDEPLOY_CODE},
    eip7251::CONSOLIDATION_REQUEST_PREDEPLOY_ADDRESS,
};
use alloy_genesis::Genesis;
use alloy_primitives::{B256, U256, address, keccak256};
use n42_execution::{RestoredSlot, restored_slots_key};
use n42_twig_core::qmdb_compat::{
    QmdbCompatTree, QmdbOperation, QmdbOperationError, encode_gov5_account_value, gov5_account_key,
    gov5_storage_key,
};
use reth_ethereum_primitives::{Block, Receipt};
use reth_execution_types::BlockExecutionOutput;
use reth_primitives_traits::SealedBlock;
use revm::database::states::BundleState;
use std::collections::HashSet;

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

/// The key under which the executor filed a block's restored slots: keccak of the parent hash
/// and the transaction hashes in order (`n42_execution::restored_slots_key`). Derived from the
/// block itself, so the root job and the payload builder find what the executor recorded
/// without a block hash, which the builder does not have before sealing.
pub fn gov5_restored_slots_key(block: &SealedBlock<Block>) -> B256 {
    restored_slots_key(
        block.header().parent_hash,
        block.body().transactions().map(|tx| *tx.tx_hash()),
    )
}

/// `SYSTEM_ADDRESS` (EIP-4788), the caller of every system call.
pub const GOV5_PRAGUE_SYSTEM_CALLER: alloy_primitives::Address =
    address!("fffffffffffffffffffffffffffffffffffffffe");

/// The leaf a Prague block writes that revm's bundle cannot show: the system
/// caller of the EIP-7002/7251 end-of-block calls.
///
/// gov5 executes those calls as messages from `SYSTEM_ADDRESS`
/// (`SysCallContract`); Erigon's EVM loads the caller as a state object, which
/// lands it in the journal's dirty set, and gov5's root computer writes every
/// dirty account — so every Prague block writes `0xffff…fffe` as a live, empty
/// account: nonce 0, balance 0, no code, the one-byte leaf `[0x00]`. reth's
/// `SystemCaller` removes the system address from the state after each call,
/// so it never reaches the bundle. Measured on chain 94 block 13,560,376: gov5's
/// 29 appended slots were the bundle's 28 plus this leaf, in key order. EIP-4788
/// and EIP-2935 do not contribute it: gov5 writes those slots directly.
pub fn with_gov5_prague_system_caller(operations: &mut Vec<QmdbOperation>) {
    let key = gov5_account_key(&GOV5_PRAGUE_SYSTEM_CALLER.into_array());
    if operations.iter().any(|operation| operation.key == key) {
        return;
    }
    operations.push(QmdbOperation {
        key,
        value: Some(encode_gov5_account_value(
            0,
            &[0u8; 32],
            &alloy_primitives::KECCAK256_EMPTY.0,
        )),
    });
}

/// [`gov5_qmdb_operations`] plus the rewrites gov5 makes for the slots the block changed and
/// restored to their value at the block's start, which revm drops from the bundle (see
/// `n42_execution::restored_slots`). A rewrite is a leaf with the value the slot already has;
/// QMDB records it as a fresh slot, and the root moves exactly as gov5's does.
///
/// Only for accounts the bundle leaves alive, only for slots the bundle does not already write,
/// and never for a zero value: that would be the deletion of a leaf that is not there, which
/// neither side records. Keys never repeat: the bundle's own storage writes are excluded and
/// the restored list is deduplicated.
pub fn gov5_qmdb_operations_with_restored(
    state: &BundleState,
    restored: &[RestoredSlot],
) -> Vec<QmdbOperation> {
    let mut operations = gov5_qmdb_operations(state);
    let mut seen = HashSet::with_capacity(restored.len());
    for (address, slot, original) in restored {
        let Some(account) = state.state.get(address) else {
            continue;
        };
        if account.was_destroyed()
            || account.info.is_none()
            || account.storage.contains_key(slot)
            || original.is_zero()
            || !seen.insert((*address, *slot))
        {
            continue;
        }
        tracing::debug!(
            target: "n42::qmdb::state",
            %address,
            slot = %B256::from(slot.to_be_bytes::<32>()),
            value = %original,
            "leaf: slot rewritten (changed and restored within the block, as gov5 writes it)"
        );
        operations.push(QmdbOperation {
            key: gov5_storage_key(&address.into_array(), &slot.to_be_bytes()),
            value: Some(original.to_be_bytes::<32>().to_vec()),
        });
    }
    operations
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
    fn a_prague_block_writes_the_system_caller_as_an_empty_live_leaf() {
        let mut operations = Vec::new();
        with_gov5_prague_system_caller(&mut operations);
        assert_eq!(operations.len(), 1);
        assert_eq!(
            operations[0].key,
            gov5_account_key(&GOV5_PRAGUE_SYSTEM_CALLER.into_array())
        );
        // gov5's `MarshalV2` of an initialised, empty account: an empty bitmap.
        assert_eq!(operations[0].value.as_deref(), Some(&[0u8][..]));
        // The leaf gov5 appended for it at chain 94 slot 63,349,385.
        assert_eq!(
            hex::encode(n42_twig_core::hash_leaf(&operations[0].key, &[0u8])),
            "47ba50c315e1a3640fc08968acbd9f53395331daaa10d803e32aa00fa00c966d"
        );
        // Never twice.
        with_gov5_prague_system_caller(&mut operations);
        assert_eq!(operations.len(), 1);
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
    #[test]
    fn restored_slots_are_rewritten_unless_the_bundle_already_writes_them() {
        let address = Address::repeat_byte(0x88);
        let written = U256::from(1);
        let restored_slot = U256::from(2);
        let zero_slot = U256::from(3);
        let info = AccountInfo {
            nonce: 1,
            balance: U256::from(5),
            ..Default::default()
        };
        let account = BundleAccount::new(
            Some(info.clone()),
            Some(info),
            [(
                written,
                StorageSlot::new_changed(U256::from(4), U256::from(9)),
            )]
            .into_iter()
            .collect(),
            AccountStatus::Changed,
        );
        let mut state = BundleState::default();
        state.state.insert(address, account);

        let restored = vec![
            // Already written by the bundle with another value: not duplicated.
            (address, written, U256::from(4)),
            // Changed and restored: rewritten with its original value.
            (address, restored_slot, U256::from(7)),
            // The same slot twice: written once.
            (address, restored_slot, U256::from(7)),
            // Restored to zero: a deletion of nothing, not written.
            (address, zero_slot, U256::ZERO),
            // An account the block did not touch: nothing to rewrite.
            (Address::repeat_byte(0x99), restored_slot, U256::from(7)),
        ];
        let plain = gov5_qmdb_operations(&state);
        let with = gov5_qmdb_operations_with_restored(&state, &restored);
        assert_eq!(with.len(), plain.len() + 1);
        assert!(with.starts_with(&plain));
        assert_eq!(
            with.last().unwrap(),
            &QmdbOperation {
                key: gov5_storage_key(&address.into_array(), &restored_slot.to_be_bytes()),
                value: Some(U256::from(7).to_be_bytes::<32>().to_vec()),
            }
        );
        let keys: HashSet<_> = with.iter().map(|op| op.key).collect();
        assert_eq!(keys.len(), with.len(), "no key repeats");

        // A destroyed account gets no rewrite: only its own deletion is emitted.
        let mut destroyed = BundleState::default();
        destroyed.state.insert(
            address,
            BundleAccount::new(None, None, Default::default(), AccountStatus::Destroyed),
        );
        let plain = gov5_qmdb_operations(&destroyed);
        assert_eq!(plain.len(), 1);
        assert_eq!(
            gov5_qmdb_operations_with_restored(&destroyed, &restored),
            plain
        );
    }
}
