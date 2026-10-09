//! Canonical execution plus QMDB durable commit, with an independent transfer oracle.
//! This is a local storage/execution measurement, not Engine API or fleet TPS.

// The production Unix node enables jemalloc by default. This cfg(test) module
// makes the entire library-test binary use the same allocator, rather than
// comparing a System-allocated harness to a jemalloc-allocated live node.
#[cfg(unix)]
#[global_allocator]
static NODE_TEST_ALLOCATOR: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

use super::*;
use alloy_consensus::{Header, TxLegacy};
use alloy_primitives::{Bytes, TxKind};
use n42_execution::{N42EvmConfig, restored_slots_for};
use n42_twig_core::{
    qmdb_compat::{QmdbOperation, encode_gov5_account_value, gov5_account_key},
    qmdb_leaf_tree::QmdbLeafTree,
};
use reth_ethereum_primitives::{Block, BlockBody, Transaction};
use reth_evm::execute::{BasicBlockExecutor, Executor};
use reth_primitives_traits::{
    RecoveredBlock, SignedTransaction, crypto::secp256k1::public_key_to_address,
};
use reth_provider::test_utils::MockEthProvider;
use reth_storage_api::{StateProvider, StateProviderFactory};
use reth_testing_utils::generators::sign_tx_with_key_pair;
use secp256k1::{Keypair, Secp256k1, SecretKey};
use std::{collections::HashSet, time::Instant};

fn address(domain: u8, n: u64) -> Address {
    let mut bytes = [0u8; 20];
    bytes[0] = domain;
    bytes[12..].copy_from_slice(&n.to_be_bytes());
    Address::from(bytes)
}

fn account(address: Address, nonce: u64, balance: U256) -> QmdbOperation {
    QmdbOperation {
        key: gov5_account_key(address.as_ref()),
        value: Some(encode_gov5_account_value(
            nonce,
            &balance.to_be_bytes(),
            &B256::ZERO.0,
        )),
    }
}

fn run(accounts: u64, transactions: u64, blocks: u64) {
    run_case(accounts, transactions, blocks, None);
}

// Some selects the Prague transfer A/B; None retains the original Cancun baseline.
fn run_case(accounts: u64, transactions: u64, blocks: u64, fast: Option<bool>) -> Vec<B256> {
    run_strategy(accounts, transactions, blocks, fast, false)
}

fn run_strategy(
    accounts: u64,
    transactions: u64,
    blocks: u64,
    fast: Option<bool>,
    parallel: bool,
) -> Vec<B256> {
    run_fork(
        accounts,
        transactions,
        blocks,
        fast.unwrap_or(false),
        parallel,
        fast.is_some(),
    )
}

fn run_fork(
    accounts: u64,
    transactions: u64,
    blocks: u64,
    fast: bool,
    parallel: bool,
    prague: bool,
) -> Vec<B256> {
    assert!(transactions > 0 && blocks > 0 && accounts > 2 * transactions);
    let mut chain_spec = n42_chainspec::n42_dev_chainspec();
    if prague {
        let chain = Arc::make_mut(&mut chain_spec);
        chain.hardforks.insert(
            reth_chainspec::EthereumHardfork::Prague,
            reth_chainspec::ForkCondition::Timestamp(0),
        );
        chain.genesis.config.prague_time = Some(0);
    }
    let config = N42EvmConfig::with_evm_factory(
        chain_spec.clone(),
        n42_execution::N42EvmFactory::with_fast_transfers(fast),
    );
    let mut roots = Vec::new();
    let chain_id = chain_spec.chain.id();
    let funding = U256::from(1_000_000_000_000_000_000u64);
    let gas_per_tx = 21_000u64;
    let gas_price = 9u128;
    let base_fee = 7u64;
    let beneficiary = address(0xc0, 0);
    let secp = Secp256k1::new();
    let keys: Vec<_> = (1..=transactions)
        .map(|n| {
            let mut secret = [0u8; 32];
            secret[24..].copy_from_slice(&n.to_be_bytes());
            let key = Keypair::from_secret_key(&secp, &SecretKey::from_slice(&secret).unwrap());
            let sender = public_key_to_address(key.public_key());
            (key, sender)
        })
        .collect();
    let senders: HashSet<_> = keys.iter().map(|(_, sender)| *sender).collect();
    assert_eq!(senders.len(), transactions as usize);
    assert!(!senders.contains(&beneficiary));
    let mut initial = Vec::with_capacity(accounts as usize);
    for (n, (_, sender)) in keys.iter().enumerate() {
        let recipient = address(0xa0, n as u64);
        assert!(!senders.contains(&recipient));
        initial.push(account(*sender, 0, funding));
        initial.push(account(recipient, 0, U256::from(1)));
    }
    initial.push(account(beneficiary, 0, U256::from(1)));
    for n in 0..accounts - 2 * transactions - 1 {
        let unused = address(0xb0, n);
        assert!(!senders.contains(&unused));
        initial.push(account(unused, 0, U256::from(1)));
    }
    let mut oracle = QmdbLeafTree::new();
    let base_root = B256::from(oracle.apply_sorted_ops(initial).unwrap());
    let base_tree = oracle.clone();
    let base_hash = B256::repeat_byte(0x71);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("canonical.bin");
    let store = Arc::new(
        Gov5QmdbStateRootStore::persistent_from_leaf_tree(
            base_hash,
            base_root,
            base_tree.clone(),
            1000,
            path.clone(),
        )
        .unwrap(),
    );
    assert!(store.wal_enabled());
    let adapter = QmdbStateAdapter::new(store.clone(), QmdbReadsMode::Only).unwrap();
    let original = store.prepare_read_view(base_hash).unwrap().unwrap();
    // No account or storage exists in the fallback. A successful funded
    // transaction must obtain its state through the actual QMDB-only adapter.
    let fallback = MockEthProvider::default();
    assert!(
        fallback
            .latest()
            .unwrap()
            .basic_account(&keys[0].1)
            .unwrap()
            .is_none()
    );
    println!(
        "canonical_qmdb_config {}",
        serde_json::json!({
            "accounts": accounts, "transactions": transactions, "blocks": blocks,
            "chainId": chain_id, "fork": if prague { "prague" } else { "cancun" },
            "strategy": if parallel { "parallel_transfer_block" } else if fast { "canonical_fast_transfer" } else { "canonical_sequential" },
        "reads": "only", "allocator": if cfg!(unix) { "jemalloc" } else { "system" }, "distinctSenders": senders.len(),
            "distinctRecipients": transactions, "disjointAddresses": true,
            "gasPrice": gas_price, "baseFee": base_fee,
            "signatureRecoveryIncluded": false, "scope": "execution_delta_qmdb_durable_commit"
        })
    );
    let mut parent = base_hash;
    let mut last_view = original.clone();
    for number in 1..=blocks {
        // The independent oracle derives every changed balance from transfer
        // arithmetic, rather than using the executor's state bundle.
        let mut expected = Vec::with_capacity(2 * transactions as usize + 1);
        for (n, (_, sender)) in keys.iter().enumerate() {
            expected.push(account(
                *sender,
                number,
                funding - U256::from(number) * U256::from(gas_price * u128::from(gas_per_tx) + 1),
            ));
            expected.push(account(address(0xa0, n as u64), 0, U256::from(number + 1)));
        }
        expected.push(account(
            beneficiary,
            0,
            U256::from(1)
                + U256::from(number)
                    * U256::from(transactions)
                    * U256::from(gas_per_tx)
                    * U256::from(gas_price - u128::from(base_fee)),
        ));
        let expected_root = B256::from(oracle.apply_sorted_ops(expected).unwrap());
        let signed: Vec<_> = keys
            .iter()
            .enumerate()
            .map(|(n, (key, _))| {
                sign_tx_with_key_pair(
                    *key,
                    Transaction::Legacy(TxLegacy {
                        chain_id: Some(chain_id),
                        nonce: number - 1,
                        gas_price,
                        gas_limit: gas_per_tx,
                        to: TxKind::Call(address(0xa0, n as u64)),
                        value: U256::from(1),
                        input: Bytes::new(),
                    }),
                )
            })
            .collect();
        let recovery_started = Instant::now();
        let recovered: Vec<_> = signed
            .iter()
            .zip(&keys)
            .map(|(tx, (_, expected))| {
                let sender = tx.try_recover().unwrap();
                assert_eq!(sender, *expected);
                sender
            })
            .collect();
        let recovery_ms = recovery_started.elapsed().as_secs_f64() * 1000.0;
        let gas_used = gas_per_tx.checked_mul(transactions).unwrap();
        let block = RecoveredBlock::new_unhashed(
            Block {
                header: Header {
                    parent_hash: parent,
                    number,
                    state_root: expected_root,
                    timestamp: 1_700_000_000 + number,
                    beneficiary,
                    gas_limit: gas_used.checked_mul(2).unwrap(),
                    gas_used,
                    base_fee_per_gas: Some(base_fee),
                    parent_beacon_block_root: Some(B256::ZERO),
                    blob_gas_used: Some(0),
                    excess_blob_gas: Some(0),
                    ..Header::default()
                },
                body: BlockBody {
                    transactions: signed,
                    withdrawals: Some(Default::default()),
                    ..Default::default()
                },
            },
            recovered,
        );
        let hash = block.hash();
        let before = adapter.counters.snapshot();
        let started = Instant::now();
        let provider = adapter.wrap(parent, fallback.latest().unwrap()).unwrap();
        let database =
            reth_revm::database::StateProviderDatabase::new(provider.into_evm_state_provider());
        let output = if parallel {
            n42_execution::parallel_block::try_execute_transfer_block(
                &config,
                database,
                &block,
                &|| {
                    let provider = adapter.wrap(parent, fallback.latest().ok()?).ok()?;
                    Some(reth_revm::database::StateProviderDatabase::new(
                        provider.into_evm_state_provider(),
                    ))
                },
            )
            .expect("parallel complete block execution")
            .expect("qualified transfer block")
        } else {
            BasicBlockExecutor::new(config.clone(), database)
                .execute(&block)
                .expect("canonical block execution")
        };
        let execution_ms = started.elapsed().as_secs_f64() * 1000.0;
        let delta_started = Instant::now();
        let restored = restored_slots_for(crate::qmdb_state::gov5_restored_slots_key(&block))
            .expect("canonical tracking executor recorded this block");
        let mut operations =
            crate::qmdb_state::gov5_qmdb_operations_with_restored(&output.state, &restored);
        operations.sort_unstable_by_key(|operation| operation.key);
        let operation_count = operations.len();
        let delta_ms = delta_started.elapsed().as_secs_f64() * 1000.0;
        let commit_started = Instant::now();
        let root = store
            .compute_and_commit(parent, hash, expected_root, operations)
            .unwrap();
        let commit_ms = commit_started.elapsed().as_secs_f64() * 1000.0;
        let total_ms = started.elapsed().as_secs_f64() * 1000.0;
        // All checks below are outside the timed region.
        let after = adapter.counters.snapshot();
        assert_eq!(root, expected_root);
        roots.push(root);
        assert_eq!(operation_count as u64, 2 * transactions + 1);
        assert_eq!(output.receipts.len(), transactions as usize);
        for (n, receipt) in output.receipts.iter().enumerate() {
            assert!(receipt.success && receipt.logs.is_empty());
            assert_eq!(receipt.cumulative_gas_used, gas_per_tx * (n as u64 + 1));
        }
        assert!(after.account_reads - before.account_reads > 2 * transactions);
        assert_eq!(
            after.read_errors
                + after.provider_errors
                + after.mismatches
                + after.unavailable_providers,
            0
        );
        assert_eq!(after.account_comparisons + after.storage_comparisons, 0);
        let view = store.read_view_for(hash).unwrap().unwrap();
        for (n, (_, sender)) in keys.iter().enumerate() {
            let account = view.account(sender).unwrap().unwrap();
            assert_eq!(account.nonce, number);
            assert_eq!(
                account.balance,
                funding - U256::from(number) * U256::from(gas_price * u128::from(gas_per_tx) + 1)
            );
            assert_eq!(
                last_view.account(sender).unwrap().unwrap().nonce,
                number - 1
            );
            assert_eq!(
                view.account(&address(0xa0, n as u64))
                    .unwrap()
                    .unwrap()
                    .balance,
                U256::from(number + 1)
            );
        }
        assert_eq!(original.account(&keys[0].1).unwrap().unwrap().nonce, 0);
        println!(
            "canonical_qmdb_sample {}",
            serde_json::json!({
                "number": number, "transactions": transactions, "successful": output.receipts.len(),
                "operations": operation_count, "accountReads": after.account_reads - before.account_reads,
                "storageReads": after.storage_reads - before.storage_reads,
                "executionMs": execution_ms, "deltaMs": delta_ms, "commitMs": commit_ms,
                "totalMs": total_ms, "recoveryMsExcluded": recovery_ms, "root": root, "blockHash": hash,
                "walBytes": std::fs::metadata(path.with_extension("wal")).unwrap().len()
            })
        );
        parent = hash;
        last_view = view;
    }
    drop(adapter);
    drop(store);
    let reopened = Gov5QmdbStateRootStore::persistent_from_leaf_tree(
        base_hash, base_root, base_tree, 1000, path,
    )
    .unwrap();
    assert_eq!(
        reopened.root_for(parent).unwrap(),
        Some(B256::from(oracle.root()))
    );
    let recovered = reopened.prepare_read_view(parent).unwrap().unwrap();
    for (n, (_, sender)) in keys.iter().enumerate() {
        assert_eq!(
            recovered.account(sender).unwrap(),
            last_view.account(sender).unwrap()
        );
        let recipient = address(0xa0, n as u64);
        assert_eq!(
            recovered.account(&recipient).unwrap(),
            last_view.account(&recipient).unwrap()
        );
    }
    assert_eq!(
        recovered.account(&beneficiary).unwrap(),
        last_view.account(&beneficiary).unwrap()
    );
    println!("canonical_qmdb_restart_verified=true");
    roots
}

#[test]
fn canonical_qmdb_execution_commits_consecutive_blocks() {
    run(160, 32, 3);
}

#[test]
#[ignore = "release measurement with a durable store on TMPDIR"]
fn bench_canonical_qmdb_execution_and_commit() {
    let number = |name: &str, default| {
        std::env::var(name)
            .ok()
            .map(|value| value.parse().expect("positive integer benchmark setting"))
            .unwrap_or(default)
    };
    run(
        number("N42_CANON_BENCH_ACCOUNTS", 200_000),
        number("N42_CANON_BENCH_TXS", 50_000),
        number("N42_CANON_BENCH_BLOCKS", 3),
    );
}

#[test]
fn canonical_qmdb_fast_transfer_matches_prague_interpreter_and_restart() {
    let slow = run_case(160, 32, 3, Some(false));
    let before = n42_execution::fast_transfer::hits();
    let fast = run_case(160, 32, 3, Some(true));
    assert!(n42_execution::fast_transfer::hits() - before >= 96);
    assert_eq!(fast, slow, "all consecutive durable QMDB roots must match");
}

#[test]
fn parallel_qmdb_transfer_blocks_match_interpreter_and_restart() {
    let slow = run_case(8_200, 4_096, 3, Some(false));
    let parallel = run_strategy(8_200, 4_096, 3, Some(true), true);
    assert_eq!(
        parallel, slow,
        "parallel batches must preserve every durable QMDB root"
    );
}

#[test]
fn default_cancun_qmdb_fast_and_parallel_match_interpreter_and_restart() {
    let serial = run_fork(8_200, 4_096, 3, false, false, false);
    let before = n42_execution::fast_transfer::hits();
    let fast = run_fork(8_200, 4_096, 3, true, false, false);
    assert!(n42_execution::fast_transfer::hits() - before >= 12_288);
    let parallel = run_fork(8_200, 4_096, 3, true, true, false);
    assert_eq!(serial, fast);
    assert_eq!(serial, parallel);
}
