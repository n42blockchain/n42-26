//! Whole blocks executed through [`N42EvmConfig`]: the restored-slot record,
//! the touched and unchanged accounts of the bundle, and the randomness
//! precompile as a block sees them.

use crate::{
    N42EvmConfig, restored_slots_env_key, restored_slots_execution_key, restored_slots_for,
    restored_slots_key,
};
use alloy_consensus::{BlockBody, Header, TxLegacy, transaction::TxHashRef};
use alloy_primitives::{Address, B256, Bytes, Signature, TxKind, U256, bytes, keccak256};
use evm2::{
    bytecode::Bytecode,
    evm::{AccountInfo, CacheDB, EmptyDB},
};
use n42_tx_types::{Block, N42TxEnvelope};
use reth_chainspec::{ChainSpecBuilder, MAINNET};
use reth_ethereum_primitives::{Transaction, TransactionSigned};
use reth_evm::{
    ConfigureEvm,
    execute::{BasicBlockExecutor, Executor},
};
use reth_primitives_traits::RecoveredBlock;
use reth_trie_common::bundle_state::BundleState;
use std::sync::Arc;

const CONTRACT: Address = Address::with_last_byte(0xC0);
/// The slot under test.
const SLOT: U256 = U256::ZERO;
/// A second slot every call moves forward, so every call changes the
/// contract whatever happens to the slot under test.
const OTHER_SLOT: U256 = U256::from_limbs([1, 0, 0, 0]);

/// `SSTORE(0, calldata[0..32]); SSTORE(1, calldata[32..64])`.
fn store_calldata_bytecode() -> Bytes {
    Bytes::from(vec![
        0x60, 0x00, 0x35, 0x60, 0x00, 0x55, 0x60, 0x20, 0x35, 0x60, 0x01, 0x55, 0x00,
    ])
}

fn store_call(chain_id: u64, nonce: u64, value: U256) -> N42TxEnvelope {
    let mut input = value.to_be_bytes::<32>().to_vec();
    // The other slot takes the call's nonce plus one: never restored.
    input.extend_from_slice(&U256::from(nonce + 1).to_be_bytes::<32>());
    // The sender is given to the executor with the block, so the signature
    // is never recovered.
    TransactionSigned::new_unhashed(
        Transaction::Legacy(TxLegacy {
            chain_id: Some(chain_id),
            nonce,
            gas_price: 7,
            gas_limit: 100_000,
            to: TxKind::Call(CONTRACT),
            value: U256::ZERO,
            input: Bytes::from(input),
        }),
        Signature::test_signature(),
    )
    .into()
}

/// Executes one block of `values` written in turn to the contract's slot 0,
/// which starts at `initial`, and returns the bundle and the key the block's
/// restored slots are filed under.
fn execute(initial: U256, values: &[U256], seed: u8) -> (BundleState, B256) {
    execute_code(initial, values, seed, store_calldata_bytecode())
}

fn execute_code(initial: U256, values: &[U256], seed: u8, code: Bytes) -> (BundleState, B256) {
    let chain_spec = Arc::new(
        ChainSpecBuilder::from(&*MAINNET)
            .shanghai_activated()
            .build(),
    );
    let sender = Address::repeat_byte(seed);
    let mut db = CacheDB::new(EmptyDB::default());
    db.insert_account_info(
        &sender,
        AccountInfo::empty().with_balance(U256::from(10u64).pow(U256::from(18))),
    );
    db.insert_account_info(
        &CONTRACT,
        AccountInfo::new(
            U256::ZERO,
            1,
            keccak256(&code),
            Some(Bytecode::new_raw(code)),
        ),
    );
    db.insert_account_storage(&CONTRACT, &SLOT, &initial);

    let transactions: Vec<_> = values
        .iter()
        .enumerate()
        .map(|(nonce, value)| store_call(chain_spec.chain.id(), nonce as u64, *value))
        .collect();
    let header = Header {
        number: 1,
        // Distinct per test: the registry of restored slots is process-wide.
        parent_hash: B256::repeat_byte(seed),
        timestamp: 1_700_000_000,
        gas_limit: 30_000_000,
        beneficiary: Address::with_last_byte(0xFF),
        base_fee_per_gas: Some(7),
        mix_hash: B256::repeat_byte(0x5A),
        ..Header::default()
    };
    let senders = vec![sender; transactions.len()];
    let block = RecoveredBlock::new_unhashed(
        Block {
            header,
            body: BlockBody {
                transactions,
                ..Default::default()
            },
        },
        senders,
    );
    let config = N42EvmConfig::new(chain_spec);
    let env_key = restored_slots_env_key(&config.evm_env(block.header()).unwrap().block);
    let output = BasicBlockExecutor::new(config, db)
        .execute(&block)
        .expect("block executes");
    for receipt in &output.receipts {
        assert!(receipt.success, "every call succeeds");
    }
    let context = restored_slots_execution_key(
        block.header().parent_hash,
        env_key,
        block.header().parent_beacon_block_root,
        block.body().withdrawals.as_ref().map(|w| w.as_slice()),
        [],
    );
    let key = restored_slots_key(
        context,
        block.body().transactions.iter().map(|tx| *tx.tx_hash()),
    );
    (output.state, key)
}

#[test]
fn a_slot_changed_and_restored_within_the_block_is_recorded() {
    let initial = U256::from(5);
    let (bundle, key) = execute(initial, &[U256::from(1), initial], 0x21);

    // The bundle dropped the slot: its final value equals its original value.
    let account = bundle.state.get(&CONTRACT).expect("contract touched");
    assert!(account.info.is_some());
    assert!(
        !account.storage.contains_key(&SLOT),
        "the bundle forgets a slot restored to its original value"
    );
    assert!(account.storage.contains_key(&OTHER_SLOT));

    let restored = restored_slots_for(key).expect("the block was executed here");
    assert_eq!(
        restored.as_slice(),
        &[
            (CONTRACT, SLOT, initial),
            (CONTRACT, OTHER_SLOT, U256::ZERO)
        ]
    );
}

#[test]
fn a_slot_that_stays_changed_is_recorded_with_its_value_at_block_start() {
    let initial = U256::from(5);
    let (bundle, key) = execute(initial, &[U256::from(1), U256::from(2)], 0x22);

    let account = bundle.state.get(&CONTRACT).expect("contract touched");
    assert_eq!(
        account.storage.get(&SLOT).map(|slot| slot.present_value()),
        Some(U256::from(2))
    );
    let restored = restored_slots_for(key).expect("the block was executed here");
    assert_eq!(
        restored.as_slice(),
        &[
            (CONTRACT, SLOT, initial),
            (CONTRACT, OTHER_SLOT, U256::ZERO)
        ]
    );
}

#[test]
fn a_write_of_the_value_a_slot_already_holds_is_not_a_change() {
    let (_, key) = execute(U256::from(5), &[U256::from(5)], 0x23);
    let restored = restored_slots_for(key).expect("the block was executed here");
    assert_eq!(restored.as_slice(), &[(CONTRACT, OTHER_SLOT, U256::ZERO)]);
}

#[test]
fn a_slot_changed_and_restored_within_one_transaction_is_recorded() {
    // SSTORE(0, 7); SSTORE(0, 5); SSTORE(1, 1)
    let code = bytes!("60076000556005600055600160015500");
    let (bundle, key) = execute_code(U256::from(5), &[U256::ZERO], 0x24, code);
    assert!(
        !bundle
            .state
            .get(&CONTRACT)
            .unwrap()
            .storage
            .contains_key(&SLOT)
    );
    let restored = restored_slots_for(key).unwrap();
    assert_eq!(
        restored.as_slice(),
        &[
            (CONTRACT, SLOT, U256::from(5)),
            (CONTRACT, OTHER_SLOT, U256::ZERO)
        ]
    );
}

#[test]
fn an_unchanged_sstore_is_not_a_dirty_rewrite() {
    // SSTORE(0, 5); SSTORE(1, 1)
    let code = bytes!("6005600055600160015500");
    let (_, key) = execute_code(U256::from(5), &[U256::ZERO], 0x25, code);
    let restored = restored_slots_for(key).unwrap();
    assert!(!restored.iter().any(|(_, slot, _)| *slot == SLOT));
}

#[test]
fn a_write_reverted_with_its_frame_is_still_recorded() {
    // SSTORE(0, 7); REVERT(0, 0)
    let code = bytes!("600760005560006000fd");
    let chain_spec = Arc::new(
        ChainSpecBuilder::from(&*MAINNET)
            .shanghai_activated()
            .build(),
    );
    let sender = Address::repeat_byte(0x26);
    let mut db = CacheDB::new(EmptyDB::default());
    db.insert_account_info(
        &sender,
        AccountInfo::empty().with_balance(U256::from(10u64).pow(U256::from(18))),
    );
    db.insert_account_info(
        &CONTRACT,
        AccountInfo::new(
            U256::ZERO,
            1,
            keccak256(&code),
            Some(Bytecode::new_raw(code)),
        ),
    );
    db.insert_account_storage(&CONTRACT, &SLOT, &U256::from(5));
    let tx = store_call(chain_spec.chain.id(), 0, U256::ZERO);
    let hash = *tx.tx_hash();
    let header = Header {
        number: 1,
        parent_hash: B256::repeat_byte(0x26),
        timestamp: 1_700_000_000,
        gas_limit: 30_000_000,
        base_fee_per_gas: Some(7),
        ..Header::default()
    };
    let block = RecoveredBlock::new_unhashed(
        Block {
            header,
            body: BlockBody {
                transactions: vec![tx],
                ..Default::default()
            },
        },
        vec![sender],
    );
    let config = N42EvmConfig::new(chain_spec);
    let env_key = restored_slots_env_key(&config.evm_env(block.header()).unwrap().block);
    let output = BasicBlockExecutor::new(config, db)
        .execute(&block)
        .expect("block executes");
    assert!(!output.receipts[0].success);
    let context = restored_slots_execution_key(block.header().parent_hash, env_key, None, None, []);
    // The SSTORE itself succeeded, and the slot is left loaded with its
    // original value: recorded like a slot restored within one transaction.
    let restored = restored_slots_for(restored_slots_key(context, [hash])).unwrap();
    assert_eq!(restored.as_slice(), &[(CONTRACT, SLOT, U256::from(5))]);
}

#[test]
fn the_randomness_precompile_reads_the_block_prevrandao() {
    // STATICCALL(gas, 0x0302, 0, 1, 0, 32) with selector 0x00; SSTORE(0, MLOAD(0))
    let code = bytes!("60206000600160006103025afa5060005160005500");
    let (bundle, _) = execute_code(U256::ZERO, &[U256::ZERO], 0x27, code);
    let slot = bundle
        .state
        .get(&CONTRACT)
        .unwrap()
        .storage
        .get(&SLOT)
        .unwrap();
    assert_eq!(
        slot.present_value(),
        U256::from_be_bytes(keccak256(B256::repeat_byte(0x5A)).0)
    );
}

const BENEFICIARY: Address = Address::with_last_byte(0xFE);
const RICH: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);

fn contract(code: Bytes) -> AccountInfo {
    AccountInfo::new(
        U256::ZERO,
        1,
        keccak256(&code),
        Some(Bytecode::new_raw(code)),
    )
}

/// Executes one block of zero-value calls to `targets` from a funded sender,
/// over `accounts`, at `gas_price` against a base fee of 7. Returns the
/// bundle, each call's success and the block's restored-slots key.
fn execute_calls(
    seed: u8,
    accounts: Vec<(Address, AccountInfo)>,
    targets: &[Address],
    gas_price: u128,
) -> (BundleState, Vec<bool>, B256) {
    let chain_spec = Arc::new(
        ChainSpecBuilder::from(&*MAINNET)
            .shanghai_activated()
            .build(),
    );
    let sender = Address::repeat_byte(seed);
    let mut db = CacheDB::new(EmptyDB::default());
    db.insert_account_info(&sender, AccountInfo::empty().with_balance(RICH));
    for (address, info) in accounts {
        db.insert_account_info(&address, info);
    }
    db.insert_account_storage(&CONTRACT, &SLOT, &U256::from(5));
    let transactions: Vec<N42TxEnvelope> = targets
        .iter()
        .enumerate()
        .map(|(nonce, target)| {
            TransactionSigned::new_unhashed(
                Transaction::Legacy(TxLegacy {
                    chain_id: Some(chain_spec.chain.id()),
                    nonce: nonce as u64,
                    gas_price,
                    gas_limit: 100_000,
                    to: TxKind::Call(*target),
                    value: U256::ZERO,
                    input: Bytes::new(),
                }),
                Signature::test_signature(),
            )
            .into()
        })
        .collect();
    let header = Header {
        number: 1,
        parent_hash: B256::repeat_byte(seed),
        timestamp: 1_700_000_000,
        gas_limit: 30_000_000,
        beneficiary: BENEFICIARY,
        base_fee_per_gas: Some(7),
        ..Header::default()
    };
    let hashes: Vec<B256> = transactions.iter().map(|tx| *tx.tx_hash()).collect();
    let senders = vec![sender; transactions.len()];
    let block = RecoveredBlock::new_unhashed(
        Block {
            header,
            body: BlockBody {
                transactions,
                ..Default::default()
            },
        },
        senders,
    );
    let config = N42EvmConfig::new(chain_spec);
    let env_key = restored_slots_env_key(&config.evm_env(block.header()).unwrap().block);
    let output = BasicBlockExecutor::new(config, db)
        .execute(&block)
        .expect("block executes");
    let context = restored_slots_execution_key(block.header().parent_hash, env_key, None, None, []);
    let success = output.receipts.iter().map(|receipt| receipt.success).collect();
    (output.state, success, restored_slots_key(context, hashes))
}

/// The account is in the bundle as revm left a touched and unchanged one:
/// dirty, with the info it had.
#[track_caller]
fn assert_touched_unchanged(bundle: &BundleState, address: Address, info: &AccountInfo) {
    let account = bundle
        .state
        .get(&address)
        .expect("a touched account is in the bundle");
    assert!(!account.status.is_not_modified());
    assert!(!account.status.was_destroyed());
    assert!(account.storage.is_empty());
    assert_eq!(account.info, account.original_info);
    let kept = account.info.as_ref().unwrap();
    assert_eq!(
        (kept.balance, kept.nonce, kept.code_hash),
        (info.balance, info.nonce, info.code_hash)
    );
}

#[test]
fn the_existing_recipient_of_a_zero_value_transfer_is_in_the_bundle() {
    let recipient = Address::with_last_byte(0xA1);
    let info = AccountInfo::empty().with_balance(U256::from(3));
    let (bundle, success, _) = execute_calls(0x31, vec![(recipient, info.clone())], &[recipient], 8);
    assert_eq!(success, [true]);
    assert_touched_unchanged(&bundle, recipient, &info);
}

#[test]
fn a_zero_value_transfer_does_not_create_its_recipient() {
    // revm made no transition for a touched account that did not exist, and
    // state clearing leaves no empty account to find.
    let recipient = Address::with_last_byte(0xA2);
    let (bundle, success, _) = execute_calls(0x32, Vec::new(), &[recipient], 8);
    assert_eq!(success, [true]);
    assert!(!bundle.state.contains_key(&recipient));
}

#[test]
fn a_beneficiary_credited_a_zero_tip_is_in_the_bundle_if_it_exists() {
    let recipient = Address::with_last_byte(0xA3);
    let info = AccountInfo::empty().with_balance(U256::from(9));
    // The gas price equals the base fee: the tip is zero.
    let (bundle, _, _) = execute_calls(0x33, vec![(BENEFICIARY, info.clone())], &[recipient], 7);
    assert_touched_unchanged(&bundle, BENEFICIARY, &info);

    let (bundle, _, _) = execute_calls(0x34, Vec::new(), &[recipient], 7);
    assert!(!bundle.state.contains_key(&BENEFICIARY));
}

#[test]
fn a_contract_called_to_no_effect_is_in_the_bundle() {
    let inner = Address::with_last_byte(0xC1);
    let inner_info = contract(bytes!("00"));
    // CALL(gas, inner, 0, 0, 0, 0, 0)
    let mut code = bytes!("5f5f5f5f5f73").to_vec();
    code.extend_from_slice(inner.as_slice());
    code.extend_from_slice(&bytes!("5af15000"));
    let outer_info = contract(code.into());
    let (bundle, success, _) = execute_calls(
        0x35,
        vec![(CONTRACT, outer_info.clone()), (inner, inner_info.clone())],
        &[CONTRACT],
        8,
    );
    assert_eq!(success, [true]);
    assert_touched_unchanged(&bundle, CONTRACT, &outer_info);
    assert_touched_unchanged(&bundle, inner, &inner_info);
}

#[test]
fn a_contract_touched_in_a_reverted_transaction_is_not_in_the_bundle() {
    // REVERT(0, 0)
    let info = contract(bytes!("5f5ffd"));
    let (bundle, success, _) = execute_calls(0x36, vec![(CONTRACT, info)], &[CONTRACT], 8);
    assert_eq!(success, [false]);
    assert!(!bundle.state.contains_key(&CONTRACT));
}

#[test]
fn a_contract_that_only_changed_and_restored_a_slot_is_in_the_bundle() {
    // SSTORE(0, 7); SSTORE(0, 5)
    let info = contract(bytes!("600760005560056000550000"));
    let (bundle, success, key) =
        execute_calls(0x37, vec![(CONTRACT, info.clone())], &[CONTRACT], 8);
    assert_eq!(success, [true]);
    assert_touched_unchanged(&bundle, CONTRACT, &info);
    let restored = restored_slots_for(key).unwrap();
    assert_eq!(restored.as_slice(), &[(CONTRACT, SLOT, U256::from(5))]);
}

#[test]
fn an_account_touched_by_two_transactions_is_entered_once() {
    let recipient = Address::with_last_byte(0xA4);
    let info = AccountInfo::empty().with_balance(U256::from(3));
    let (bundle, _, _) = execute_calls(0x38, vec![(recipient, info.clone())], &[recipient; 2], 7);
    assert_touched_unchanged(&bundle, recipient, &info);
    assert_eq!(bundle.state.len(), 2, "the sender and the recipient");
}
