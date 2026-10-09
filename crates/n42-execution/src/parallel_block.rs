//! Complete transfer-block execution using the ordinary Ethereum block executor.
//!
//! System calls surround the parallel transaction phase. All worker databases must
//! refer to the exact same immutable parent as `db`; the pre-execution cache is
//! overlaid on each worker before it reads accounts. This API does not validate a
//! header, a signature, an H2 certificate, or commit the returned state.

use crate::{
    N42EvmConfig,
    parallel_transfer::{NotParallel, append_reverts, execute_for_build, graft_bundles},
};
use alloy_consensus::{Transaction, transaction::TxHashRef};
use alloy_evm::{Evm, ToTxEnv, block::BlockExecutor, eth::EthTxResult};
use reth_ethereum_primitives::{Block, Receipt};
use reth_evm::{
    ConfigureEvm,
    execute::{BlockExecutionError, BlockExecutionOutput},
};
use reth_primitives_traits::RecoveredBlock;
use revm::{
    Database, DatabaseCommit,
    context::result::ResultAndState,
    database::{State, states::bundle_state::BundleRetention},
    state::Account,
};

/// Try a complete block of qualified transfers. `None` means the caller must
/// execute the whole block serially on a fresh state. Errors after grafting are
/// fatal for this candidate; the partially merged local state is discarded.
pub fn try_execute_transfer_block<G>(
    config: &N42EvmConfig,
    db: G,
    block: &RecoveredBlock<Block>,
    open: &(dyn Fn() -> Option<G> + Sync),
) -> Result<Option<BlockExecutionOutput<Receipt>>, BlockExecutionError>
where
    G: Database + std::fmt::Debug + Send,
    G::Error: std::fmt::Display + Send + Sync + 'static,
{
    if block.body().transactions.is_empty() || block.header().block_access_list_hash.is_some() {
        return Ok(None);
    }
    let env = config
        .evm_env(block.header())
        .map_err(BlockExecutionError::other)?;
    let txs: Vec<_> = block
        .transactions_recovered()
        .map(|tx| tx.cloned())
        .collect();
    let mut state = State::builder()
        .with_database(db)
        .with_bundle_update()
        .build();
    let mut executor = config
        .executor_for_block(&mut state, block)
        .map_err(BlockExecutionError::other)?;
    executor.apply_pre_execution_changes()?;
    let Some(graft) = try_commit_import_batch(&mut executor, &env, &txs, open)? else {
        return Ok(None);
    };
    let (_, result) = executor.finish()?;
    state.merge_transitions(BundleRetention::Reverts);
    let mut bundle = state.take_bundle();
    append_reverts(&mut bundle, graft.reverts);
    Ok(Some(BlockExecutionOutput {
        state: bundle,
        result,
    }))
}

/// Commit a complete sealed block's transfer phase, or decline without mutating
/// the executor. Signature recovery belongs to the caller's existing import path.
/// Pre-execution system calls must already have run; post-execution changes and
/// graft revert finalization remain with that caller.
pub(crate) fn try_commit_import_batch<'a, DB, G>(
    executor: &mut reth_evm::BlockExecutorForEvm<'a, N42EvmConfig, DB>,
    env: &alloy_evm::EvmEnv,
    transactions: &[alloy_consensus::transaction::Recovered<
        reth_ethereum_primitives::TransactionSigned,
    >],
    open: &(dyn Fn() -> Option<G> + Sync),
) -> Result<Option<crate::parallel_transfer::Graft>, BlockExecutionError>
where
    DB: alloy_evm::Database + 'a,
    G: Database + std::fmt::Debug + Send,
    G::Error: std::fmt::Display + Send + Sync + 'static,
{
    use revm::primitives::hardfork::SpecId;
    if transactions.is_empty()
        || !executor.receipts().is_empty()
        || env.cfg_env.spec < SpecId::CANCUN
        || env.cfg_env.spec >= SpecId::AMSTERDAM
        // A multi-block BasicBlockExecutor can retain a previous block's bundle.
        // Direct graft bookkeeping currently requires a fresh per-block bundle.
        || !executor.evm_mut().db_mut().bundle_state.state.is_empty()
    {
        return Ok(None);
    }
    let mut keys = Vec::with_capacity(transactions.len());
    let mut remaining = env.block_env.gas_limit;
    for tx in transactions {
        let Some(to) = tx.to() else { return Ok(None) };
        if tx.gas_limit() > remaining {
            return Ok(None);
        }
        let Some(next) = remaining.checked_sub(21_000) else {
            return Ok(None);
        };
        remaining = next;
        keys.push((tx.signer(), to));
    }
    let prestate = executor.evm_mut().db_mut().cache.clone();
    let run = match execute_for_build(
        env,
        &keys,
        &|i| (&transactions[i], transactions[i].to_tx_env()),
        &|| {
            Some(
                State::builder()
                    .with_database(open()?)
                    .with_cached_prestate(prestate.clone())
                    .build(),
            )
        },
    ) {
        Ok(run) if run.skipped.is_empty() => run,
        Ok(_) | Err(NotParallel::NotATransfer(_) | NotParallel::TouchesBeneficiary(_)) => {
            return Ok(None);
        }
        Err(error) => return Err(BlockExecutionError::other(error)),
    };
    if run.executed.len() != transactions.len()
        || run.executed.iter().enumerate().any(|(i, tx)| tx.index != i)
    {
        return Err(BlockExecutionError::msg(
            "sealed transfer batch changed transaction order",
        ));
    }
    let (_, graft) = commit_transfer_batch(executor, run, env.block_env.beneficiary, |tx| {
        (*tx.tx_hash(), tx.tx_type())
    })?;
    Ok(Some(graft))
}

/// Commit a locally executed transfer prefix through the standard receipt and
/// tracking executor, after the caller has checked candidate gas/size limits.
/// All batches must originate from this executor's exact pre-transaction state.
/// Do not pass external execution results. On error, discard the candidate.
pub fn commit_transfer_batch<'a, DB, E, T>(
    executor: &mut crate::restored_slots::TrackingExecutor<E>,
    run: crate::parallel_transfer::BuildRun<T>,
    beneficiary: alloy_primitives::Address,
    metadata: impl Fn(&T) -> (alloy_primitives::B256, reth_ethereum_primitives::TxType),
) -> Result<(Vec<T>, crate::parallel_transfer::Graft), BlockExecutionError>
where
    DB: Database + 'a,
    E: BlockExecutor<
            Evm: Evm<DB = &'a mut State<DB>>,
            Transaction = reth_ethereum_primitives::TransactionSigned,
            Result = EthTxResult<
                revm::context::result::HaltReason,
                reth_ethereum_primitives::TxType,
            >,
        >,
{
    if !executor.receipts().is_empty() {
        return Err(BlockExecutionError::msg(
            "transfer batch must precede serial transactions",
        ));
    }
    let graft = graft_bundles(executor.evm_mut().db_mut(), run.bundles, beneficiary)
        .map_err(BlockExecutionError::other)?;
    if !graft.beneficiary_delta.is_zero() {
        let db = executor.evm_mut().db_mut();
        let mut info = db.basic(beneficiary).map_err(BlockExecutionError::other)?;
        if info.is_none()
            && let Some(parent_info) = graft.beneficiary_original.clone()
        {
            db.cache.accounts.insert(
                beneficiary,
                revm::database::states::CacheAccount::new_loaded(
                    parent_info.clone(),
                    Default::default(),
                ),
            );
            info = Some(parent_info);
        }
        let mut account = match info {
            Some(info) => Account::from(info),
            None => Account::new_not_existing(revm::state::TransactionId::ZERO),
        };
        account.info.balance = account
            .info
            .balance
            .checked_add(graft.beneficiary_delta)
            .ok_or_else(|| {
                BlockExecutionError::other(std::io::Error::other("transfer beneficiary overflow"))
            })?;
        account.mark_touch();
        db.commit([(beneficiary, account)].into_iter().collect());
    }
    // The standard receipt builder and gas accounting still own these outputs.
    // No storage was written by the qualified transfers, but their hashes must
    // remain in the restored-slot registry key in their original block order.
    let mut transactions = Vec::with_capacity(run.executed.len());
    for tx in run.executed {
        let (hash, tx_type) = metadata(&tx.tx);
        executor.commit_preexecuted_transfer(
            hash,
            EthTxResult {
                result: ResultAndState {
                    result: tx.result,
                    state: Default::default(),
                },
                blob_gas_used: 0,
                tx_type,
            },
        )?;
        transactions.push(tx.tx);
    }
    Ok((transactions, graft))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{N42EvmFactory, restored_slots_for, restored_slots_key};
    use alloy_consensus::{Header, SignableTransaction, TxLegacy};
    use alloy_eips::{eip2935, eip4788, eip4895::Withdrawal, eip7002, eip7251};
    use alloy_primitives::{Address, B256, Bytes, Signature, TxKind, U256, address};
    use reth_ethereum_primitives::{BlockBody, TransactionSigned};
    use reth_evm::execute::{BasicBlockExecutor, Executor};
    use revm::{
        bytecode::Bytecode,
        database::{CacheDB, EmptyDB},
        state::AccountInfo,
    };
    use std::sync::Arc;

    const A: Address = address!("1000000000000000000000000000000000000001");
    const B: Address = address!("2000000000000000000000000000000000000002");
    const C: Address = address!("3000000000000000000000000000000000000003");
    const FEE: Address = address!("4000000000000000000000000000000000000004");
    const NEW: Address = address!("5000000000000000000000000000000000000005");

    fn config() -> N42EvmConfig {
        let mut chain = n42_chainspec::n42_dev_chainspec();
        let chain_mut = Arc::make_mut(&mut chain);
        chain_mut.hardforks.insert(
            reth_chainspec::EthereumHardfork::Prague,
            reth_chainspec::ForkCondition::Timestamp(0),
        );
        chain_mut.genesis.config.prague_time = Some(0);
        N42EvmConfig::with_evm_factory(chain, N42EvmFactory::with_fast_transfers(false))
    }

    fn code(db: &mut CacheDB<EmptyDB>, address: Address, bytes: Bytes, balance: u64) {
        let code = Bytecode::new_raw(bytes);
        db.insert_account_info(
            address,
            AccountInfo {
                balance: U256::from(balance),
                nonce: 1,
                code_hash: code.hash_slow(),
                code: Some(code),
                ..Default::default()
            },
        );
    }

    fn database() -> CacheDB<EmptyDB> {
        let mut db = CacheDB::new(EmptyDB::default());
        for (address, balance) in [(A, 1_000_000_000), (B, 1_000_000_000), (C, 10), (FEE, 1)] {
            db.insert_account_info(
                address,
                AccountInfo {
                    balance: U256::from(balance),
                    ..Default::default()
                },
            );
        }
        for (address, bytes) in [
            (
                eip4788::BEACON_ROOTS_ADDRESS,
                eip4788::BEACON_ROOTS_CODE.clone(),
            ),
            (
                eip2935::HISTORY_STORAGE_ADDRESS,
                eip2935::HISTORY_STORAGE_CODE.clone(),
            ),
            (
                eip7002::WITHDRAWAL_REQUEST_PREDEPLOY_ADDRESS,
                eip7002::WITHDRAWAL_REQUEST_PREDEPLOY_CODE.clone(),
            ),
            (
                eip7251::CONSOLIDATION_REQUEST_PREDEPLOY_ADDRESS,
                eip7251::CONSOLIDATION_REQUEST_PREDEPLOY_CODE.clone(),
            ),
        ] {
            code(&mut db, address, bytes, 1);
        }
        db
    }

    fn block(config: &N42EvmConfig) -> RecoveredBlock<Block> {
        let mut txs: Vec<TransactionSigned> = Vec::new();
        let mut senders = Vec::new();
        for (from, to, nonce, value) in
            [(A, C, 0, 5), (B, C, 0, 7), (A, NEW, 1, 9), (B, NEW, 1, 11)]
        {
            // Deliberately supplied recovered senders: this test exercises execution,
            // not signature recovery or consensus validation.
            txs.push(
                TxLegacy {
                    chain_id: Some(config.chain_spec().chain.id()),
                    nonce,
                    gas_price: 9,
                    gas_limit: 21_000,
                    to: TxKind::Call(to),
                    value: U256::from(value),
                    input: Bytes::new(),
                }
                .into_signed(Signature::new(U256::from(1), U256::from(1), false))
                .into(),
            );
            senders.push(from);
        }
        RecoveredBlock::new_unhashed(
            Block {
                header: Header {
                    parent_hash: B256::repeat_byte(0x77),
                    number: 1,
                    timestamp: 1_700_000_000,
                    beneficiary: FEE,
                    gas_limit: 1_000_000,
                    gas_used: 84_000,
                    base_fee_per_gas: Some(7),
                    parent_beacon_block_root: Some(B256::repeat_byte(0x33)),
                    blob_gas_used: Some(0),
                    excess_blob_gas: Some(0),
                    ..Default::default()
                },
                body: BlockBody {
                    transactions: txs,
                    withdrawals: Some(
                        vec![
                            Withdrawal {
                                index: 0,
                                validator_index: 0,
                                address: C,
                                amount: 1,
                            },
                            Withdrawal {
                                index: 1,
                                validator_index: 1,
                                address: NEW,
                                amount: 1,
                            },
                        ]
                        .into(),
                    ),
                    ..Default::default()
                },
            },
            senders,
        )
    }

    fn compare(db: CacheDB<EmptyDB>) -> BlockExecutionOutput<Receipt> {
        let config = config();
        let block = block(&config);
        let mut parallel =
            try_execute_transfer_block(&config, db.clone(), &block, &|| Some(db.clone()))
                .unwrap()
                .expect("all transfers qualify");
        let key = restored_slots_key(
            block.header().parent_hash,
            block.body().transactions.iter().map(|tx| *tx.tx_hash()),
        );
        assert!(
            restored_slots_for(key).is_some(),
            "grafted tx hashes must be recorded"
        );
        let mut serial = BasicBlockExecutor::new(config, db).execute(&block).unwrap();
        for output in [&mut parallel, &mut serial] {
            for reverts in output.state.reverts.iter_mut() {
                reverts.sort_unstable_by_key(|(address, _)| *address);
            }
        }
        assert_eq!(
            parallel.result, serial.result,
            "receipts, gas, requests and blob gas"
        );
        assert_eq!(
            parallel.state, serial.state,
            "complete bundle, code and revert records"
        );
        parallel
    }

    #[test]
    fn block_matches_interpreter_with_all_system_contracts_and_withdrawals() {
        let output = compare(database());
        assert!(
            !output.state.state[&eip4788::BEACON_ROOTS_ADDRESS]
                .storage
                .is_empty()
        );
        assert!(
            !output.state.state[&eip2935::HISTORY_STORAGE_ADDRESS]
                .storage
                .is_empty()
        );
        assert_eq!(
            output.state.state[&C].info.as_ref().unwrap().balance,
            U256::from(1_000_000_022u64)
        );
        assert_eq!(
            output.state.state[&NEW].info.as_ref().unwrap().balance,
            U256::from(1_000_000_020u64)
        );
    }

    #[test]
    fn pre_system_credit_is_visible_to_workers_and_post_call_reads_merged_balances() {
        let mut db = database();
        db.insert_account_info(A, AccountInfo::default());
        // A test pre-call transfers its balance to A; a parent-only worker
        // would refuse A's transactions. This intentionally replaces fixture code.
        let mut pre = vec![0x73];
        pre.extend_from_slice(A.as_slice());
        pre.push(0xff); // SELFDESTRUCT(A), retaining old code under EIP-6780.
        code(
            &mut db,
            eip4788::BEACON_ROOTS_ADDRESS,
            pre.into(),
            1_000_000_000,
        );
        let mut post = vec![0x73];
        post.extend_from_slice(C.as_slice());
        post.extend_from_slice(&[0x31, 0x5f, 0x55, 0x5f, 0x5f, 0xf3]); // SSTORE(0, BALANCE(C)); return empty.
        code(
            &mut db,
            eip7002::WITHDRAWAL_REQUEST_PREDEPLOY_ADDRESS,
            post.into(),
            1,
        );
        let output = compare(db);
        assert_eq!(
            output.state.state[&eip7002::WITHDRAWAL_REQUEST_PREDEPLOY_ADDRESS].storage[&U256::ZERO]
                .present_value(),
            U256::from(22)
        );
    }

    #[test]
    fn sealed_batch_decline_keeps_executor_unchanged_for_ordered_fallback() {
        let config = config();
        let block = block(&config);
        for contract_recipient in [false, true] {
            let mut db = database();
            if contract_recipient {
                code(&mut db, C, Bytes::from_static(&[0x00]), 10);
            } else {
                db.insert_account_info(B, AccountInfo::default());
            }
            let mut state = State::builder()
                .with_database(db.clone())
                .with_bundle_update()
                .build();
            let mut executor = config.executor_for_block(&mut state, &block).unwrap();
            executor.apply_pre_execution_changes().unwrap();
            let before = executor.evm_mut().db_mut().cache.accounts.clone();
            let env = config.evm_env(block.header()).unwrap();
            let txs = block
                .transactions_recovered()
                .map(|tx| tx.cloned())
                .collect::<Vec<_>>();
            assert!(
                try_commit_import_batch(&mut executor, &env, &txs, &|| Some(db.clone()))
                    .unwrap()
                    .is_none()
            );
            assert_eq!(executor.evm_mut().db_mut().cache.accounts, before);
            assert!(executor.receipts().is_empty());
            assert!(executor.evm_mut().db_mut().bundle_state.state.is_empty());
            let serial = txs
                .iter()
                .try_for_each(|tx| executor.execute_transaction(tx).map(|_| ()));
            assert_eq!(serial.is_ok(), contract_recipient);
        }
    }

    #[test]
    fn worker_open_failure_is_fatal_and_block_gas_overflow_declines() {
        let config = config();
        let db = database();
        let block = block(&config);
        assert!(try_execute_transfer_block(&config, db.clone(), &block, &|| None).is_err());
        let (mut unsealed, senders) = block.split();
        unsealed.header.gas_limit = 42_000;
        let block = RecoveredBlock::new_unhashed(unsealed, senders);
        assert!(
            try_execute_transfer_block(&config, db.clone(), &block, &|| Some(db.clone()))
                .unwrap()
                .is_none()
        );
    }
}
