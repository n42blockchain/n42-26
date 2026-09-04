//! A storage slot changed by one transaction and restored by a later one in
//! the same block: revm's bundle forgets it, gov5 writes it, and the QMDB
//! root job must add the rewrite from what the executor recorded.

use alloy_consensus::{Header, TxLegacy};
use alloy_primitives::{Address, B256, Bytes, TxKind, U256, keccak256};
use n42_execution::{N42EvmConfig, restored_slots_for};
use n42_node::qmdb_state::{
    gov5_qmdb_operations, gov5_qmdb_operations_with_restored, gov5_restored_slots_key,
};
use n42_twig_core::qmdb_compat::gov5_storage_key;
use reth_chainspec::{ChainSpecBuilder, MAINNET};
use reth_ethereum_primitives::{Block, BlockBody, Transaction, TransactionSigned};
use reth_evm::execute::{BasicBlockExecutor, Executor};
use reth_primitives_traits::{RecoveredBlock, crypto::secp256k1::public_key_to_address};
use reth_testing_utils::generators::sign_tx_with_key_pair;
use revm::{bytecode::Bytecode, database::CacheDB, state::AccountInfo};
use secp256k1::{Keypair, Secp256k1, SecretKey};
use std::sync::Arc;

const CONTRACT: Address = Address::with_last_byte(0xC0);
/// The slot under test.
const SLOT: U256 = U256::ZERO;
/// A second slot every call moves forward, so the contract stays in the
/// bundle: revm drops an account whose info is unchanged and whose storage
/// nets to nothing, and gov5's rewrite rule (as n42-rs checked it on chain
/// 94) applies to accounts the bundle still holds.
const OTHER_SLOT: U256 = U256::from_limbs([1, 0, 0, 0]);

/// `SSTORE(0, calldata[0..32]); SSTORE(1, calldata[32..64])`.
fn store_calldata_bytecode() -> Bytes {
    Bytes::from(vec![
        0x60, 0x00, 0x35, 0x60, 0x00, 0x55, 0x60, 0x20, 0x35, 0x60, 0x01, 0x55, 0x00,
    ])
}

fn sender(seed: u8) -> (Keypair, Address) {
    let secret_key = SecretKey::from_slice(&[seed; 32]).expect("valid test key");
    let key_pair = Keypair::from_secret_key(&Secp256k1::new(), &secret_key);
    (key_pair, public_key_to_address(key_pair.public_key()))
}

fn store_call(key_pair: Keypair, chain_id: u64, nonce: u64, value: U256) -> TransactionSigned {
    let mut input = value.to_be_bytes::<32>().to_vec();
    // The other slot takes the call's nonce plus one: never restored.
    input.extend_from_slice(&U256::from(nonce + 1).to_be_bytes::<32>());
    sign_tx_with_key_pair(
        key_pair,
        Transaction::Legacy(TxLegacy {
            chain_id: Some(chain_id),
            nonce,
            gas_price: 7,
            gas_limit: 100_000,
            to: TxKind::Call(CONTRACT),
            value: U256::ZERO,
            input: Bytes::from(input),
        }),
    )
}

/// Executes one block of `values` written in turn to the contract's slot 0,
/// which starts at `initial`, and returns the block and its bundle.
fn execute(
    initial: U256,
    values: &[U256],
    seed: u8,
) -> (RecoveredBlock<Block>, revm::database::states::BundleState) {
    let chain_spec = Arc::new(
        ChainSpecBuilder::from(&*MAINNET)
            .shanghai_activated()
            .build(),
    );
    let (key_pair, sender) = sender(seed);
    let mut db = CacheDB::new(revm::database::EmptyDB::default());
    db.insert_account_info(
        sender,
        AccountInfo {
            balance: U256::from(10u64).pow(U256::from(18)),
            ..Default::default()
        },
    );
    let code = store_calldata_bytecode();
    db.insert_account_info(
        CONTRACT,
        AccountInfo {
            nonce: 1,
            code_hash: keccak256(&code),
            code: Some(Bytecode::new_raw(code)),
            ..Default::default()
        },
    );
    db.insert_account_storage(CONTRACT, SLOT, initial)
        .expect("storage insert");

    let transactions: Vec<_> = values
        .iter()
        .enumerate()
        .map(|(nonce, value)| store_call(key_pair, chain_spec.chain.id(), nonce as u64, *value))
        .collect();
    let header = Header {
        number: 1,
        parent_hash: B256::repeat_byte(0x11),
        timestamp: 1_700_000_000,
        gas_limit: 30_000_000,
        beneficiary: Address::with_last_byte(0xFF),
        base_fee_per_gas: Some(7),
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
    let output = BasicBlockExecutor::new(N42EvmConfig::new(chain_spec), db)
        .execute(&block)
        .expect("block executes");
    for receipt in &output.receipts {
        assert!(receipt.success, "every store call succeeds");
    }
    (block, output.state)
}

#[test]
fn a_slot_changed_and_restored_within_the_block_is_rewritten() {
    let initial = U256::from(5);
    let (block, bundle) = execute(initial, &[U256::from(1), initial], 0x21);

    // revm dropped the slot: its final value equals its original value.
    let account = bundle.state.get(&CONTRACT).expect("contract touched");
    assert!(account.info.is_some());
    assert!(
        !account.storage.contains_key(&SLOT),
        "revm's bundle forgets a slot restored to its original value"
    );
    assert!(account.storage.contains_key(&OTHER_SLOT));

    // The executor filed it under the key the root job derives from the block.
    let restored =
        restored_slots_for(gov5_restored_slots_key(&block)).expect("the block was executed here");
    assert_eq!(
        restored.as_slice(),
        &[
            (CONTRACT, SLOT, initial),
            (CONTRACT, OTHER_SLOT, U256::ZERO)
        ]
    );

    let plain = gov5_qmdb_operations(&bundle);
    let with = gov5_qmdb_operations_with_restored(&bundle, &restored);
    let key = gov5_storage_key(&CONTRACT.into_array(), &SLOT.to_be_bytes());
    assert!(!plain.iter().any(|op| op.key == key));
    let rewrite = with
        .iter()
        .find(|op| op.key == key)
        .expect("the restored slot is rewritten");
    assert_eq!(
        rewrite.value.as_deref(),
        Some(initial.to_be_bytes::<32>().as_slice())
    );
    assert_eq!(with.len(), plain.len() + 1);
}

#[test]
fn a_slot_that_stays_changed_is_written_once() {
    let initial = U256::from(5);
    let (block, bundle) = execute(initial, &[U256::from(1), U256::from(2)], 0x22);

    let account = bundle.state.get(&CONTRACT).expect("contract touched");
    assert_eq!(
        account.storage.get(&SLOT).map(|slot| slot.present_value()),
        Some(U256::from(2))
    );

    // Recorded with its value at the block's start, as every changed slot is.
    let restored =
        restored_slots_for(gov5_restored_slots_key(&block)).expect("the block was executed here");
    assert_eq!(
        restored.as_slice(),
        &[
            (CONTRACT, SLOT, initial),
            (CONTRACT, OTHER_SLOT, U256::ZERO)
        ]
    );

    // But the bundle already writes it, so nothing is added.
    let plain = gov5_qmdb_operations(&bundle);
    let with = gov5_qmdb_operations_with_restored(&bundle, &restored);
    assert_eq!(with, plain);
    let key = gov5_storage_key(&CONTRACT.into_array(), &SLOT.to_be_bytes());
    assert_eq!(with.iter().filter(|op| op.key == key).count(), 1);
    assert_eq!(
        with.iter()
            .find(|op| op.key == key)
            .unwrap()
            .value
            .as_deref(),
        Some(U256::from(2).to_be_bytes::<32>().as_slice())
    );
}

#[test]
fn a_write_of_the_value_a_slot_already_holds_is_not_a_change() {
    let (block, _) = execute(U256::from(5), &[U256::from(5)], 0x23);
    // The slot under test is written with the value it already has: revm
    // reports no change for it, so only the other slot is recorded.
    let restored =
        restored_slots_for(gov5_restored_slots_key(&block)).expect("the block was executed here");
    assert_eq!(restored.as_slice(), &[(CONTRACT, OTHER_SLOT, U256::ZERO)]);
}
