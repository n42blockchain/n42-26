//! The parallel transfer prefix in the payload lifecycle.
//!
//! The serial part follows `native_payload::default_n42_payload` (reth v2.11.0
//! `default_ethereum_payload`); keep its limits and finalization in step with
//! it. Before the serial loop the pool's leading plain transfers are executed
//! in sender batches (`n42_execution::parallel_build`) and handed to the
//! block's executor, which folds their state changes into the block's bundle
//! when it finishes.

use crate::engine_types::N42BuiltPayload;
use crate::native_payload::{BestTransactionsIter, default_n42_payload};
use alloy_consensus::transaction::TxHashRef as _;
use alloy_consensus::{Transaction, TransactionEnvelope as _};
use alloy_primitives::{Address, B256, Bloom, U256};
use alloy_rlp::Encodable;
use alloy_rpc_types_engine::PayloadAttributes as EthPayloadAttributes;
use n42_execution::{
    N42EvmConfig,
    direct_transfer::DirectTransfers,
    parallel_build::{self, CacheScope},
};
use n42_tx_types::{N42Primitives, N42TxEnvelope};
use reth_basic_payload_builder::{BuildArguments, BuildOutcome, PayloadConfig, is_better_payload};
use reth_chainspec::{ChainSpecProvider, EthChainSpec, EthereumHardforks};
use reth_consensus_common::validation::MAX_RLP_BLOCK_SIZE;
use reth_errors::ConsensusError;
use reth_ethereum_payload_builder::EthereumBuilderConfig;
use reth_evm::{
    ConfigureEvm, EvmEnv, NextBlockEnvAttributes,
    database::StateProviderDatabase,
    execute::{
        BasicBlockBuilder, BlockAssemblerInput, BlockBuilder, BlockBuilderOutcome,
        BlockExecutionError, BlockExecutionOutput, BlockExecutor, BlockValidationError,
    },
};
use reth_execution_cache::{CachedStateMetrics, CachedStateMetricsSource, CachedStateProvider};
use reth_payload_builder::BlobSidecars;
use reth_payload_builder_primitives::PayloadBuilderError;
use reth_payload_primitives::PayloadAttributes as _;
use reth_primitives_traits::{
    Recovered, RecoveredBlock, transaction::error::InvalidTransactionError,
};
use reth_storage_api::{EvmStateProvider, StateProvider, StateProviderFactory};
use reth_transaction_pool::{
    BestTransactions, BestTransactionsAttributes, PoolTransaction, TransactionPool,
    ValidPoolTransaction,
    error::{Eip4844PoolTransactionError, InvalidPoolTransactionError},
};
use reth_trie_common::updates::TrieUpdates;
use std::sync::Arc;
use tracing::{debug, info, trace, warn};

type Factory = <N42EvmConfig as ConfigureEvm>::BlockExecutorFactory;

fn flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|value| value == "1")
}

/// The builder's parts, taken by value so that nothing of it outlives the
/// call.
/// Moves the executed candidates, `order` of `candidates`, into the block's
/// transactions in that order.
///
/// A full block's transfers are tens of megabytes of transactions. Moved one
/// after another into a vector that grows as it goes, they took as long as
/// the merge of the block's execution (18 ms of a 79 ms prefix); when every
/// candidate executed, which is the block the batches fill, they are moved
/// side by side into room made once.
fn move_executed<T: Send>(
    mut candidates: Vec<std::sync::OnceLock<T>>,
    order: &[usize],
    transactions: &mut Vec<T>,
) {
    use rayon::prelude::*;
    transactions.reserve(order.len());
    let every_candidate = order.len() == candidates.len()
        && order.iter().enumerate().all(|(at, &index)| at == index)
        && candidates.iter().all(|candidate| candidate.get().is_some());
    if every_candidate && order.len() >= 4_096 {
        transactions.par_extend(candidates.into_par_iter().map(|candidate| {
            candidate.into_inner().expect("every candidate was converted: checked above")
        }));
        return;
    }
    for &index in order {
        if let Some(tx) = candidates[index].take() {
            transactions.push(tx);
        }
    }
}

#[expect(clippy::type_complexity)]
fn into_parts<'a, F, E, A, N>(
    builder: BasicBlockBuilder<'a, F, E, A, N>,
) -> (
    E,
    F::EvmEnv,
    Vec<Recovered<reth_primitives_traits::TxTy<N>>>,
    F::ExecutionCtx<'a>,
    &'a reth_primitives_traits::SealedHeader<reth_primitives_traits::HeaderTy<N>>,
    &'a A,
)
where
    F: reth_evm::BlockExecutorFactory + 'a,
    N: reth_primitives_traits::NodePrimitives,
{
    let BasicBlockBuilder {
        executor,
        evm_env,
        transactions,
        ctx,
        parent,
        assembler,
    } = builder;
    (executor, evm_env, transactions, ctx, parent, assembler)
}

pub(crate) fn enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| flag("N42_PARALLEL_BUILD"))
}

/// `N42_GRAFT_STREAM=1` grafted each batch's bundle into the block's live
/// state as the batch ended. The batches' changes are now merged once and
/// folded into the finished bundle, so there is nothing to stream into: the
/// switch is reported once and changes nothing.
fn warn_graft_stream() {
    static REPORTED: std::sync::Once = std::sync::Once::new();
    REPORTED.call_once(|| {
        if flag("N42_GRAFT_STREAM") {
            warn!(
                target: "n42::payload",
                "N42_GRAFT_STREAM=1 has no effect: the transfer batches are merged once and folded into the block's bundle at its end"
            );
        }
    });
}

pub(crate) fn build_with_parallel<Client, Pool, F>(
    evm_config: N42EvmConfig,
    client: Client,
    pool: Pool,
    builder_config: EthereumBuilderConfig,
    args: BuildArguments<EthPayloadAttributes, N42BuiltPayload>,
    best_txs: F,
    parallel: bool,
) -> Result<BuildOutcome<N42BuiltPayload>, PayloadBuilderError>
where
    Client: StateProviderFactory + ChainSpecProvider<ChainSpec: EthereumHardforks>,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = N42TxEnvelope>>,
    F: FnOnce(BestTransactionsAttributes) -> BestTransactionsIter<Pool>,
{
    if !parallel {
        return default_n42_payload(evm_config, client, pool, builder_config, args, best_txs);
    }
    warn_graft_stream();
    let BuildArguments {
        mut cached_reads,
        execution_cache,
        state_root_handle,
        config,
        cancel,
        best_payload,
    } = args;
    let PayloadConfig {
        parent_header,
        attributes,
        payload_id,
        ..
    } = config;
    let skip_state_root = builder_config.skip_state_root;
    let setup_start = std::time::Instant::now();
    // The batches' changes reach the block's bundle at its end and are not
    // streamed to the incremental root job transaction by transaction. Drop
    // its hook and receiver and compute the complete root at finish.
    drop(state_root_handle);

    let state_provider = client.state_by_block_hash(parent_header.hash())?;
    let evm_state_provider = (&state_provider).into_evm_state_provider();
    let cached_state_provider = execution_cache.map(|execution_cache| {
        CachedStateProvider::new(
            &evm_state_provider,
            execution_cache.cache().clone(),
            // It's ok to recreate the cache every time, because it's cheap to do so for a vanilla
            // Ethereum builder every 12s.
            Some(CachedStateMetrics::zeroed(
                CachedStateMetricsSource::Builder,
            )),
        )
    });
    let state = StateProviderDatabase::new(
        cached_state_provider
            .as_ref()
            .map(|provider| provider as &dyn EvmStateProvider)
            .unwrap_or(&evm_state_provider),
    );
    let setup_provider_ms = setup_start.elapsed().as_millis() as u64;
    let chain_spec = client.chain_spec();
    let is_amsterdam = chain_spec.is_amsterdam_active_at_timestamp(attributes.timestamp());
    let gas_limit = builder_config
        .gas_limit_with_target(parent_header.gas_limit, attributes.target_gas_limit());
    let blob_params = chain_spec.blob_params_at_timestamp(attributes.timestamp());

    debug!(
        target: "payload_builder",
        id = %payload_id,
        parent_header = ?parent_header.hash(),
        parent_number = parent_header.number,
        "building payload"
    );

    let cached_db = cached_reads.as_db_mut(state);
    let evm_config = evm_config.with_jit_support();
    let next_block_env_attributes = NextBlockEnvAttributes {
        timestamp: attributes.timestamp(),
        suggested_fee_recipient: attributes.suggested_fee_recipient,
        prev_randao: attributes.prev_randao,
        gas_limit,
        parent_beacon_block_root: attributes.parent_beacon_block_root(),
        withdrawals: attributes.withdrawals.clone().map(Into::into),
        extra_data: builder_config.extra_data.clone(),
        slot_number: attributes.slot_number(),
    };
    let batch_env = evm_config
        .next_evm_env(&parent_header, &next_block_env_attributes)
        .map_err(PayloadBuilderError::other)?;
    let ctx = evm_config
        .context_for_next_block(&parent_header, next_block_env_attributes)
        .map_err(PayloadBuilderError::other)?;
    let evm = evm_config.evm_with_env(cached_db, batch_env.clone());
    // Built by hand rather than through `builder_for_next_block`: the prefix
    // and the H2-v4 finish below need the executor and the builder's parts.
    let mut builder = BasicBlockBuilder::<Factory, _, _, N42Primitives>::new(
        evm_config.block_executor_factory(),
        evm_config.block_assembler(),
        evm,
        batch_env.clone(),
        &parent_header,
        ctx,
    );
    if is_amsterdam {
        builder.executor_mut().enable_block_access_list_builder();
    }
    let base_fee = builder.evm_env().block_base_fee();
    let blob_fee = blob_params
        .as_ref()
        .map(|_| builder.evm_env().block_blob_base_fee());
    let separate_block_gas = builder.evm_env().uses_separate_block_gas();
    let regular_gas_limit_cap = builder.evm_env().regular_gas_limit_cap();

    let setup_env_ms = setup_start.elapsed().as_millis() as u64 - setup_provider_ms;
    let best_start = std::time::Instant::now();
    let mut best_txs = best_txs(BestTransactionsAttributes::new(base_fee, blob_fee));
    let setup_best_ms = best_start.elapsed().as_millis() as u64;

    let pre_execution_start = std::time::Instant::now();
    builder.apply_pre_execution_changes().map_err(|err| {
        warn!(target: "payload_builder", %err, "failed to apply pre-execution changes");
        PayloadBuilderError::Internal(err.into())
    })?;
    let setup_pre_execution_ms = pre_execution_start.elapsed().as_millis() as u64;

    let mut total_fees = U256::ZERO;
    let mut cumulative_tx_gas_used = 0u64;
    let mut block_regular_gas_used = 0u64;
    let mut block_state_gas_used = 0u64;
    // initialize empty blob sidecars at first. If cancun is active then this will be populated by
    // blob sidecars if any.
    let mut blob_sidecars = BlobSidecars::Empty;
    let mut block_blob_count = 0;
    let mut block_transactions_rlp_length = 0;
    let protocol_max_blob_count = blob_params
        .as_ref()
        .map(|params| params.max_blob_count)
        .unwrap_or_default();
    // Apply user-configured blob limit (EIP-7872)
    // Per EIP-7872: if the minimum is zero, set it to one
    let max_blob_count = builder_config
        .max_blobs_per_block
        .map(|user_limit| std::cmp::min(user_limit, protocol_max_blob_count).max(1))
        .unwrap_or(protocol_max_blob_count);
    let is_osaka = chain_spec.is_osaka_active_at_timestamp(attributes.timestamp());
    let withdrawals_rlp_length = attributes
        .withdrawals
        .as_ref()
        .map(|withdrawals| withdrawals.length())
        .unwrap_or(0);

    // N42: per-block transaction count limit to prevent oversized blocks.
    static MAX_TXS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let max_txs_per_block = *MAX_TXS.get_or_init(|| {
        std::env::var("N42_MAX_TXS_PER_BLOCK")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(80_000)
    });
    // N42: build time budget — stop packing when exceeded (checked every 256 txs).
    static BUILD_BUDGET_MS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    let build_time_budget = std::time::Duration::from_millis(*BUILD_BUDGET_MS.get_or_init(|| {
        std::env::var("N42_BUILD_TIME_BUDGET_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1000u64)
    }));
    let mut tx_count: usize = 0;

    let packing_start = std::time::Instant::now();
    let setup_ms = setup_start.elapsed().as_millis() as u64;
    let mut evm_exec_total = std::time::Duration::ZERO;
    let mut iter_total = std::time::Duration::ZERO;
    let mut consensus_total = std::time::Duration::ZERO;

    // Stop processing new arrivals during packing. The iterator already holds
    // a snapshot; new txs arriving mid-pack only add overhead via broadcast
    // channel polling.
    best_txs.no_updates();

    // False once a batch was handed over without its accounts in the
    // executor's cache: nothing may then read them from it.
    let mut serial_allowed = true;
    // Transactions root of a block that is exactly the parallel prefix, derived
    // on a helper thread while the prefix is committed: (transaction count, job).
    let mut early_transactions_root: Option<(usize, std::thread::JoinHandle<B256>)> = None;
    let mut replay = std::collections::VecDeque::new();
    // The batches reproduce Cancun to Osaka; Amsterdam's block access list
    // needs every transaction's own changes.
    if chain_spec.is_cancun_active_at_timestamp(attributes.timestamp()) && !is_amsterdam {
        let beneficiary = batch_env.block.beneficiary;
        let mut candidates = Vec::new();
        let mut keys = Vec::new();
        let mut lengths = Vec::new();
        let mut reserved_gas = 0u64;
        let mut reserved_rlp = 0usize;
        while candidates.len() < max_txs_per_block {
            if cancel.is_cancelled() {
                return Ok(BuildOutcome::Cancelled);
            }
            if packing_start.elapsed() >= build_time_budget {
                break;
            }
            let Some(pool_tx) = best_txs.next() else {
                break;
            };
            let Some(to) = pool_tx.to() else {
                replay.push_back(pool_tx);
                break;
            };
            // 0x50 executes as an EIP-1559 call from its key's address
            // (`n42_tx_types::execution`), so a plain 0x50 transfer is one too.
            let plain = (pool_tx.tx_type() <= 2
                || pool_tx.tx_type() == n42_tx_types::ALT_SIG_TX_TYPE_ID)
                && pool_tx.transaction.input().is_empty()
                && pool_tx
                    .transaction
                    .access_list()
                    .is_none_or(|list| list.is_empty())
                && pool_tx.sender() != to
                && pool_tx.sender() != beneficiary
                && to != beneficiary;
            let length = if is_osaka {
                pool_tx.transaction.consensus_ref().inner().length()
            } else {
                0
            };
            // Reserve declared gas, which bounds actual gas even when candidates
            // are refused. The ordinary loop can spend any unused capacity later.
            let fits = pool_tx.gas_limit() <= gas_limit.saturating_sub(reserved_gas)
                && reserved_rlp
                    .saturating_add(length)
                    .saturating_add(withdrawals_rlp_length)
                    .saturating_add(1024)
                    <= MAX_RLP_BLOCK_SIZE;
            if !plain || !fits {
                replay.push_back(pool_tx);
                break;
            }
            reserved_gas += pool_tx.gas_limit();
            reserved_rlp += length;
            keys.push((pool_tx.sender(), to));
            lengths.push(length);
            candidates.push(pool_tx);
        }
        if !candidates.is_empty() {
            let batch_started = std::time::Instant::now();
            // Each candidate as the block carries it, kept from its
            // conversion on the batch's thread.
            let consensus: Vec<std::sync::OnceLock<Recovered<N42TxEnvelope>>> =
                (0..candidates.len()).map(|_| std::sync::OnceLock::new()).collect();
            let convert = |i: usize| {
                let tx = consensus[i].get_or_init(|| candidates[i].to_consensus());
                Recovered::new_unchecked(n42_tx_types::execution::evm_tx_of(tx.inner()), tx.signer())
            };
            // Every batch reads the block's parent. A pre-execution system
            // call that changed a batch account is caught by the cache half
            // below, and the candidates then run serially.
            let open = || {
                let provider = client.state_by_block_hash(parent_header.hash()).ok()?;
                Some(StateProviderDatabase::new(provider.into_evm_state_provider()))
            };
            // N42_GRAFT_NO_CACHE=1: a batch that fills the block, in a block
            // without withdrawals, is not written into the executor's cache.
            // Nothing may read a batch account from the executor afterwards,
            // so the serial loop below does not run for such a block.
            static NO_CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            let keeps_cache = !(*NO_CACHE.get_or_init(|| flag("N42_GRAFT_NO_CACHE"))
                && candidates.len() >= max_txs_per_block
                && attributes
                    .withdrawals
                    .as_ref()
                    .is_none_or(|withdrawals| withdrawals.is_empty()));
            static ROOT_EARLY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            if !keeps_cache && *ROOT_EARLY.get_or_init(|| flag("N42_TX_ROOT_EARLY")) {
                // No serial transaction follows a prefix that drops the cache
                // copy, so the candidates are the block's whole transaction
                // list when all of them execute. Hash them beside the execution.
                let pooled = candidates.clone();
                let job = std::thread::Builder::new()
                    .name("tx-root".into())
                    .spawn(move || {
                        let started = std::time::Instant::now();
                        let txs: Vec<_> =
                            pooled.iter().map(|tx| tx.to_consensus().into_inner()).collect();
                        let root = alloy_consensus::proofs::calculate_transaction_root(&txs);
                        metrics::histogram!("n42_payload_builder_tx_root_ms")
                            .record(started.elapsed().as_secs_f64() * 1000.0);
                        debug!(target: "payload_builder", tx_root_ms = started.elapsed().as_millis() as u64, "early transactions root derived");
                        root
                    })
                    .map_err(PayloadBuilderError::other)?;
                early_transactions_root = Some((candidates.len(), job));
            }
            let batch_config = parallel_build::batch_config(&evm_config);
            let direct =
                DirectTransfers::for_env(&batch_env, n42_execution::direct_transfer::shape_of);
            let run = parallel_build::execute_batches(
                &batch_config,
                &batch_env,
                &keys,
                &convert,
                &open,
                direct.as_ref(),
            );
            if cancel.is_cancelled() {
                return Ok(BuildOutcome::Cancelled);
            }
            let run_ms = batch_started.elapsed().as_millis() as u64;
            let commit_started = std::time::Instant::now();
            // The run handed to the executor: the cache half, the receipts
            // and the merge its finish folds. A refusal that leaves the
            // executor as it was gives every candidate to the serial loop.
            let handed = run.and_then(|run| {
                let parallel_build::ParallelRun {
                    executed,
                    skipped,
                    merged,
                    touched,
                    phases,
                } = run;
                // Each transfer's hash, type and fee are read out of its
                // transaction side by side: one after another, a full block's
                // 95,000 scattered transactions took about 4 ms.
                let spent: Vec<(usize, u64)> = executed
                    .iter()
                    .map(|executed| (executed.index, executed.result.tx_gas_used()))
                    .collect();
                let entries = {
                    use rayon::prelude::*;
                    spent
                        .par_iter()
                        .with_min_len(2048)
                        .map(|&(index, gas_used)| {
                            let tx = consensus[index].get().ok_or_else(|| {
                                parallel_build::Refused::Commit(
                                    "an executed transfer was not converted".to_owned(),
                                )
                            })?;
                            let fee = tx.effective_tip_per_gas(base_fee).ok_or_else(|| {
                                parallel_build::Refused::Commit(
                                    "invalid executed transfer fee".to_owned(),
                                )
                            })?;
                            Ok::<_, parallel_build::Refused>((
                                *tx.tx_hash(),
                                tx.tx_type(),
                                U256::from(fee) * U256::from(gas_used),
                            ))
                        })
                        .collect::<Result<Vec<_>, parallel_build::Refused>>()?
                };
                let fees = entries
                    .iter()
                    .fold(U256::ZERO, |fees, (_, _, fee)| fees + *fee);
                let order: Vec<usize> = spent.iter().map(|(index, _)| *index).collect();
                let results = entries
                    .into_iter()
                    .zip(executed)
                    .map(|((hash, tx_type, _), executed)| (hash, tx_type, executed.result));
                let scope = if keeps_cache {
                    CacheScope::All
                } else {
                    CacheScope::Only(&[])
                };
                let committed =
                    parallel_build::commit_run(&mut builder.executor, results, merged, touched, scope)?;
                Ok((order, skipped, fees, committed, phases))
            });
            let commit_ms = commit_started.elapsed().as_millis() as u64;
            match handed {
                Ok((order, skipped, fees, committed, phases)) => {
                    let accepted = order.len();
                    // The job hashed every candidate; its root stands only if
                    // all of them executed (they keep candidate order in the
                    // block).
                    if accepted != candidates.len() {
                        early_transactions_root = None;
                    }
                    total_fees += fees;
                    cumulative_tx_gas_used += committed.gas_used;
                    block_regular_gas_used += committed.regular_gas_used;
                    block_state_gas_used += committed.state_gas_used;
                    block_transactions_rlp_length +=
                        order.iter().map(|&index| lengths[index]).sum::<usize>();
                    let moved_started = std::time::Instant::now();
                    move_executed(consensus, &order, &mut builder.transactions);
                    let moved_ms = moved_started.elapsed().as_millis() as u64;
                    serial_allowed = keeps_cache;
                    tx_count = accepted;
                    let tail = std::mem::take(&mut replay);
                    replay.extend(skipped.iter().map(|&i| candidates[i].clone()));
                    replay.extend(tail);
                    evm_exec_total += batch_started.elapsed();
                    metrics::counter!("n42_parallel_payload_executed_total")
                        .increment(accepted as u64);
                    metrics::histogram!("n42_parallel_payload_batch_ms")
                        .record(batch_started.elapsed().as_secs_f64() * 1_000.0);
                    info!(target: "n42::payload", accepted, skipped = replay.len(),
                        batches = phases.batches,
                        run_ms,
                        commit_ms,
                        keeps_cache,
                        total_ms = batch_started.elapsed().as_millis() as u64,
                        partition_ms = phases.partition_ms,
                        groups_ms = phases.groups_ms,
                        open_ms_sum = phases.open_ms_sum,
                        run_ms_sum = phases.run_ms_sum,
                        batch_merge_ms_sum = phases.batch_merge_ms_sum,
                        batch_ms_max = phases.batch_ms_max,
                        collect_ms = phases.collect_ms,
                        merge_ms = phases.merge_ms,
                        moved_ms,
                        direct_hits = phases.direct_hits,
                        direct_delegated = phases.direct_delegated,
                        "parallel transfer prefix executed; not a consensus commit");
                }
                Err(refused) if refused.executor_untouched() => {
                    // Nothing of the run was kept: the serial loop takes the
                    // candidates in their order.
                    early_transactions_root = None;
                    metrics::counter!("n42_parallel_payload_refused_total", "reason" => refused.reason())
                        .increment(1);
                    warn!(target: "n42::payload", %refused, candidates = candidates.len(),
                        "parallel transfer prefix refused; its candidates are built serially");
                    let tail = std::mem::take(&mut replay);
                    replay.extend(candidates);
                    replay.extend(tail);
                }
                Err(refused) => return Err(PayloadBuilderError::other(refused)),
            }
        }
    }
    let mut best_txs = ReplayTransactions {
        front: replay,
        inner: best_txs,
    };

    while let Some(pool_tx) = {
        let iter_start = std::time::Instant::now();
        let tx = best_txs.next();
        iter_total += iter_start.elapsed();
        tx
    } {
        if tx_count >= max_txs_per_block || !serial_allowed {
            break;
        }
        // Check build time budget every 256 transactions to avoid per-tx syscall overhead.
        if tx_count & 0xFF == 0 && tx_count > 0 && packing_start.elapsed() >= build_time_budget {
            debug!(target: "payload_builder", tx_count, "build time budget exceeded, stopping packing");
            break;
        }
        // check if the job was cancelled, if so we can exit early
        if cancel.is_cancelled() {
            return Ok(BuildOutcome::Cancelled);
        }

        // ensure we still have capacity for this transaction
        let exceeds_gas_limit = if separate_block_gas {
            let regular_available = gas_limit.saturating_sub(block_regular_gas_used);
            let state_available = gas_limit.saturating_sub(block_state_gas_used);
            let regular_limit = pool_tx.gas_limit().min(regular_gas_limit_cap);
            if regular_limit > regular_available {
                Some((regular_limit, regular_available))
            } else if pool_tx.gas_limit() > state_available {
                Some((pool_tx.gas_limit(), state_available))
            } else {
                None
            }
        } else {
            let available = gas_limit.saturating_sub(cumulative_tx_gas_used);
            (pool_tx.gas_limit() > available).then_some((pool_tx.gas_limit(), available))
        };
        if let Some((transaction_gas_limit, block_available_gas)) = exceeds_gas_limit {
            // we can't fit this transaction into the block, so we need to mark it as invalid
            // which also removes all dependent transaction from the iterator before we can
            // continue
            best_txs.mark_invalid(
                &pool_tx,
                InvalidPoolTransactionError::ExceedsGasLimit(
                    transaction_gas_limit,
                    block_available_gas,
                ),
            );
            continue;
        }

        // Converted only for transactions that pass the gas check above; the
        // cost is tracked on its own.
        let consensus_start = std::time::Instant::now();
        let tx = pool_tx.to_consensus();
        consensus_total += consensus_start.elapsed();

        // MAX_RLP_BLOCK_SIZE applies from Osaka only: the RLP length is not
        // computed before it (about 2 us a transaction).
        let tx_rlp_len = if is_osaka {
            let len = tx.inner().length();
            let estimated_block_size_with_tx =
                block_transactions_rlp_length + len + withdrawals_rlp_length + 1024; // 1Kb of overhead for the block header
            if estimated_block_size_with_tx > MAX_RLP_BLOCK_SIZE {
                best_txs.mark_invalid(
                    &pool_tx,
                    InvalidPoolTransactionError::OversizedData {
                        size: estimated_block_size_with_tx,
                        limit: MAX_RLP_BLOCK_SIZE,
                    },
                );
                continue;
            }
            len
        } else {
            0
        };

        // There's only limited amount of blob space available per block, so we need to check if
        // the EIP-4844 can still fit in the block
        let mut blob_tx_sidecar = None;
        let tx_blob_count = tx.blob_count();
        if let Some(tx_blob_count) = tx_blob_count {
            if block_blob_count + tx_blob_count > max_blob_count {
                // we can't fit this _blob_ transaction into the block, so we mark it as
                // invalid, which removes its dependent transactions from
                // the iterator. This is similar to the gas limit condition
                // for regular transactions above.
                trace!(
                    target: "payload_builder",
                    tx = ?tx.hash(),
                    ?block_blob_count,
                    "skipping blob transaction because it would exceed the max blob count per block"
                );
                best_txs.mark_invalid(
                    &pool_tx,
                    InvalidPoolTransactionError::Eip4844(
                        Eip4844PoolTransactionError::TooManyEip4844Blobs {
                            have: block_blob_count + tx_blob_count,
                            permitted: max_blob_count,
                        },
                    ),
                );
                continue;
            }

            let blob_sidecar_result = 'sidecar: {
                let Some(sidecar) = pool
                    .get_blob(*tx.hash())
                    .map_err(PayloadBuilderError::other)?
                else {
                    break 'sidecar Err(Eip4844PoolTransactionError::MissingEip4844BlobSidecar);
                };

                if is_osaka {
                    if sidecar.is_eip7594() {
                        Ok(sidecar)
                    } else {
                        Err(Eip4844PoolTransactionError::UnexpectedEip4844SidecarAfterOsaka)
                    }
                } else if sidecar.is_eip4844() {
                    Ok(sidecar)
                } else {
                    Err(Eip4844PoolTransactionError::UnexpectedEip7594SidecarBeforeOsaka)
                }
            };

            blob_tx_sidecar = match blob_sidecar_result {
                Ok(sidecar) => Some(sidecar),
                Err(error) => {
                    best_txs.mark_invalid(&pool_tx, InvalidPoolTransactionError::Eip4844(error));
                    continue;
                }
            };
        }

        let miner_fee = tx.effective_tip_per_gas(base_fee);
        let tx_hash = *tx.tx_hash();
        let evm_start = std::time::Instant::now();
        let gas_output = match builder.execute_transaction(tx) {
            Ok(gas_output) => gas_output,
            Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                error, ..
            })) => {
                if error.is_nonce_too_low() {
                    // if the nonce is too low, we can skip this transaction
                    trace!(target: "payload_builder", %error, ?tx_hash, "skipping nonce too low transaction");
                } else {
                    // if the transaction is invalid, we can skip it and all of its
                    // descendants
                    trace!(target: "payload_builder", %error, ?tx_hash, "skipping invalid transaction and its descendants");
                    best_txs.mark_invalid(
                        &pool_tx,
                        InvalidPoolTransactionError::Consensus(
                            InvalidTransactionError::TxTypeNotSupported,
                        ),
                    );
                }
                continue;
            }
            // The executor is the source of truth for block gas availability. Keep this
            // non-fatal in case local builder accounting diverges from executor rules.
            Err(BlockExecutionError::Validation(
                BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas {
                    transaction_gas_limit,
                    block_available_gas,
                },
            )) => {
                trace!(target: "payload_builder", %transaction_gas_limit, %block_available_gas, ?tx_hash, "skipping transaction exceeding block gas limit");
                best_txs.mark_invalid(
                    &pool_tx,
                    InvalidPoolTransactionError::ExceedsGasLimit(
                        transaction_gas_limit,
                        block_available_gas,
                    ),
                );
                continue;
            }
            // this is an error that we should treat as fatal for this attempt
            Err(err) => return Err(PayloadBuilderError::evm(err)),
        };

        evm_exec_total += evm_start.elapsed();

        // add to the total blob gas used if the transaction successfully executed
        if let Some(blob_count) = tx_blob_count {
            block_blob_count += blob_count;

            // if we've reached the max blob count, we can skip blob txs entirely
            if block_blob_count == max_blob_count {
                best_txs.skip_blobs();
            }
        }

        block_transactions_rlp_length += tx_rlp_len;

        // update and add to total fees
        let gas_used = gas_output.tx_gas_used();
        let miner_fee = miner_fee.expect("fee is always valid; execution succeeded");
        total_fees += U256::from(miner_fee) * U256::from(gas_used);
        cumulative_tx_gas_used += gas_used;
        block_regular_gas_used += gas_output.regular_gas_used();
        block_state_gas_used += gas_output.state_gas_used();
        tx_count += 1;

        // Add blob tx sidecar to the payload.
        if let Some(sidecar) = blob_tx_sidecar {
            blob_sidecars.push_sidecar_variant(sidecar.as_ref().clone());
        }
    }


    let packing_elapsed = packing_start.elapsed();
    let other_ms = packing_elapsed
        .saturating_sub(evm_exec_total)
        .saturating_sub(iter_total)
        .saturating_sub(consensus_total)
        .as_millis() as u64;
    info!(
        target: "payload_builder",
        id = %payload_id,
        tx_count,
        cumulative_tx_gas_used,
        packing_ms = packing_elapsed.as_millis() as u64,
        evm_exec_ms = evm_exec_total.as_millis() as u64,
        pool_overhead_ms = packing_elapsed.saturating_sub(evm_exec_total).as_millis() as u64,
        iter_ms = iter_total.as_millis() as u64,
        consensus_ms = consensus_total.as_millis() as u64,
        other_ms,
        setup_ms,
        setup_provider_ms,
        setup_env_ms,
        setup_best_ms,
        setup_pre_execution_ms,
        "N42_PAYLOAD_PACK: tx packing complete"
    );

    if cancel.is_cancelled() {
        return Ok(BuildOutcome::Cancelled);
    }

    // check if we have a better block
    if !is_better_payload(best_payload.as_ref(), total_fees) {
        // Release db
        drop(builder);
        // can skip building the block
        return Ok(BuildOutcome::Aborted {
            fees: total_fees,
            cached_reads,
        });
    }

    let finish_start = std::time::Instant::now();
    let mut built_gov5_roots = None;
    let BlockBuilderOutcome {
        execution_result,
        execution_state,
        block,
        block_access_list,
        ..
    } = if skip_state_root && !is_amsterdam && crate::payload::builder_state_root_replaced() {
        // H2-v4 normalization overwrites both the state root (QMDB root) and the
        // receipts root (Gov5 flat receipts hash) of every built block, and only
        // the normalized block is proposed. The upstream finish would still hash
        // the post state and build the MPT receipts trie; skip both. This mirrors
        // `BasicBlockBuilder::finish_with_state_root` without those two steps;
        // Amsterdam (block access lists) is excluded above and takes the
        // upstream path.
        let (executor, evm_env, transactions, ctx, parent, assembler) = into_parts(builder);
        let BlockExecutionOutput {
            state: execution_state,
            result: execution_result,
        } = executor.finish()?;
        let executor_finish_ms = finish_start.elapsed().as_millis() as u64;
        let receipts_started = std::time::Instant::now();
        let receipts_root = n42_network::gov5_native_receipts_root(&execution_result.receipts);
        let receipts_root_ms = receipts_started.elapsed().as_millis() as u64;
        // The QMDB root that replaces this block's state root depends only on
        // the state changes, which are final here. Derive it beside the block
        // assembly instead of after the payload has been handed over.
        let qmdb_root_job = crate::payload::builder_qmdb_store()
            .filter(|_| !n42_consensus_service::orchestrator::shared_execution_enabled())
            .map(|store| {
                let context = n42_execution::restored_slots_execution_key(
                    ctx.parent_hash,
                    n42_execution::restored_slots_env_key(&evm_env.block),
                    ctx.parent_beacon_block_root,
                    ctx.withdrawals.as_deref(),
                    [],
                );
                let key = n42_execution::restored_slots_key(
                    context,
                    transactions.iter().map(|tx| *tx.tx_hash()),
                );
                (store, key, ctx.parent_hash)
            });
        let prague = chain_spec.is_prague_active_at_timestamp(attributes.timestamp());
        let bundle_state = &execution_state;
        let (qmdb_root, block) = std::thread::scope(|scope| {
            let qmdb_root = qmdb_root_job.map(|(store, key, parent_hash)| {
                scope.spawn(move || {
                    let started = std::time::Instant::now();
                    let restored = n42_execution::restored_slots_for(key).unwrap_or_default();
                    let operations =
                        crate::exec_cache::gov5_sorted_operations(bundle_state, &restored, prague);
                    let root = store.prepare_bound_candidate(parent_hash, operations, key);
                    metrics::histogram!("n42_payload_builder_qmdb_root_ms")
                        .record(started.elapsed().as_secs_f64() * 1000.0);
                    root
                })
            });
            let assemble_started = std::time::Instant::now();
            // Used only when the block holds exactly the transactions the job
            // hashed; a panicked job falls back to the assembler's own root.
            let transactions_root = early_transactions_root
                .take()
                .filter(|(count, _)| *count == transactions.len())
                .and_then(|(_, job)| job.join().ok());
            let root_join_ms = assemble_started.elapsed().as_millis() as u64;
            let mut logs_bloom = Bloom::ZERO;
            for log in execution_result.receipts.iter().flat_map(|r| &r.logs) {
                logs_bloom.accrue_log(log);
            }
            let (transactions, senders): (Vec<_>, Vec<Address>) =
                transactions.into_iter().map(|tx| tx.into_parts()).unzip();
            let block = assembler
                .assemble_block(
                    BlockAssemblerInput::<Factory>::new(
                        evm_env,
                        ctx,
                        parent,
                        transactions,
                        &execution_result,
                        bundle_state,
                        state_provider.as_ref(),
                        parent_header.state_root,
                        None,
                    ),
                    transactions_root,
                    Some(receipts_root),
                    Some(logs_bloom),
                )
                .map(|block| RecoveredBlock::new_unhashed(block, senders));
            let assemble_ms = assemble_started.elapsed().as_millis() as u64 - root_join_ms;
            let qmdb_root = qmdb_root.map(|job| job.join());
            info!(
                target: "payload_builder",
                executor_finish_ms,
                receipts_root_ms,
                early_tx_root = transactions_root.is_some(),
                root_join_ms,
                assemble_ms,
                root_wait_ms = assemble_started.elapsed().as_millis() as u64 - assemble_ms,
                "N42_PAYLOAD_FINISH: block finish stages"
            );
            (qmdb_root, block)
        });
        let block = block?;
        match qmdb_root {
            Some(Ok(Ok(state_root))) => built_gov5_roots = Some((state_root, receipts_root)),
            // Normalization derives the root again and reports the failure.
            Some(Ok(Err(error))) => {
                warn!(target: "payload_builder", id = %payload_id, %error, "builder could not derive the QMDB root")
            }
            Some(Err(_)) => {
                warn!(target: "payload_builder", id = %payload_id, "QMDB root worker panicked in the builder")
            }
            None => {}
        }
        BlockBuilderOutcome {
            execution_result,
            execution_state,
            hashed_state: Default::default(),
            trie_updates: Default::default(),
            block,
            block_access_list: None,
        }
    } else if skip_state_root {
        debug!(
            target: "payload_builder",
            id = %payload_id,
            state_root = ?parent_header.state_root,
            "skipping payload state-root computation"
        );
        builder.finish(
            state_provider.as_ref(),
            Some((parent_header.state_root, TrieUpdates::default())),
        )?
    } else {
        builder.finish(state_provider.as_ref(), None)?
    };

    let finish_ms = finish_start.elapsed().as_secs_f64() * 1000.0;
    metrics::histogram!("n42_payload_builder_finish_ms").record(finish_ms);

    let requests = chain_spec
        .is_prague_active_at_timestamp(attributes.timestamp())
        .then(|| execution_result.requests.clone());

    if cancel.is_cancelled() {
        return Ok(BuildOutcome::Cancelled);
    }
    if is_osaka && block.rlp_length() > MAX_RLP_BLOCK_SIZE {
        return Err(PayloadBuilderError::other(ConsensusError::BlockTooLarge {
            rlp_length: block.rlp_length(),
            max_rlp_length: MAX_RLP_BLOCK_SIZE,
        }));
    }

    // Retain the transaction root and execution output so this node's import
    // and compact followers can reuse work already authenticated by the
    // leader's sealed block hash.
    let cache_start = std::time::Instant::now();
    let block_hash = block.hash();
    let transactions_root = block.header().transactions_root;
    let senders = block.senders().to_vec();
    let execution_output = BlockExecutionOutput {
        state: execution_state,
        result: execution_result,
    };
    reth_evm::payload_cache::store_payload_transactions_root(block_hash, transactions_root);
    if let Some(roots) = built_gov5_roots {
        crate::exec_cache::store_built_gov5_roots(block_hash, roots);
    }
    // The second copy exists to be sent to peers. A block whose roots the
    // H2-v4 normalization replaces is sent without its output unless shared
    // execution is on; the normalization then borrows the copy kept for this
    // node's own import.
    if !crate::payload::builder_state_root_replaced()
        || n42_consensus_service::orchestrator::shared_execution_enabled()
    {
        reth_evm::payload_cache::store_broadcast_execution(
            block_hash,
            (execution_output.clone(), senders.clone()),
        );
    }
    reth_evm::payload_cache::store_payload_execution(block_hash, (execution_output, senders));
    let cache_ms = cache_start.elapsed().as_secs_f64() * 1000.0;
    metrics::histogram!("n42_payload_builder_cache_ms").record(cache_ms);
    info!(
        target: "payload_builder",
        %block_hash,
        finish_ms,
        cache_ms,
        "N42_PAYLOAD_CACHE: cached execution output for new_payload + broadcast"
    );

    debug!(
        target: "payload_builder",
        id = %payload_id,
        sealed_block_header = ?block.sealed_header(),
        "sealed built block"
    );

    let block_access_list = block_access_list.map(|bal| bal.split().1);
    let payload = N42BuiltPayload::new(Arc::new(block), total_fees, requests, block_access_list)
        // add blob sidecars from the executed txs
        .with_sidecars(blob_sidecars);

    Ok(BuildOutcome::Better {
        payload,
        cached_reads,
    })
}

/// Candidates already pulled from the pool iterator that still need serial execution.
/// Invalidating one must also suppress any buffered descendants of that sender.
struct ReplayTransactions<T: PoolTransaction> {
    front: std::collections::VecDeque<Arc<ValidPoolTransaction<T>>>,
    inner: Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<T>>>>,
}

impl<T: PoolTransaction> Iterator for ReplayTransactions<T> {
    type Item = Arc<ValidPoolTransaction<T>>;
    fn next(&mut self) -> Option<Self::Item> {
        self.front.pop_front().or_else(|| self.inner.next())
    }
}

impl<T: PoolTransaction> BestTransactions for ReplayTransactions<T> {
    fn mark_invalid(&mut self, transaction: &Self::Item, kind: InvalidPoolTransactionError) {
        self.front
            .retain(|tx| tx.sender() != transaction.sender() || tx.nonce() < transaction.nonce());
        self.inner.mark_invalid(transaction, kind);
    }
    fn no_updates(&mut self) {
        self.inner.no_updates();
    }
    fn set_skip_blobs(&mut self, skip_blobs: bool) {
        if skip_blobs {
            let mut blocked = std::collections::HashMap::<alloy_primitives::Address, u64>::new();
            for tx in &self.front {
                if tx.transaction.is_eip4844() {
                    blocked
                        .entry(tx.sender())
                        .and_modify(|nonce| *nonce = (*nonce).min(tx.nonce()))
                        .or_insert(tx.nonce());
                }
            }
            self.front.retain(|tx| {
                blocked
                    .get(&tx.sender())
                    .is_none_or(|nonce| tx.nonce() < *nonce)
            });
        }
        self.inner.set_skip_blobs(skip_blobs);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_pool::N42PooledTransaction;

    fn candidates_of(values: &[Option<u64>]) -> Vec<std::sync::OnceLock<[u64; 40]>> {
        values
            .iter()
            .map(|value| {
                let cell = std::sync::OnceLock::new();
                if let Some(value) = value {
                    let _ = cell.set([*value; 40]);
                }
                cell
            })
            .collect()
    }

    /// The executed candidates reach the block in `order`, after what the
    /// block already holds, whether all of them executed or some did not.
    #[test]
    fn executed_candidates_move_into_the_block_in_order() {
        let serial = |candidates: &[Option<u64>], order: &[usize]| {
            let mut cells = candidates_of(candidates);
            let mut out = vec![[7u64; 40]];
            for &index in order {
                if let Some(tx) = cells[index].take() {
                    out.push(tx);
                }
            }
            out
        };
        let moved = |candidates: &[Option<u64>], order: &[usize]| {
            let mut out = vec![[7u64; 40]];
            move_executed(candidates_of(candidates), order, &mut out);
            out
        };
        // A full run, large enough for the side-by-side move, and a small one.
        for n in [10_000usize, 5] {
            let all: Vec<_> = (0..n as u64).map(Some).collect();
            let order: Vec<usize> = (0..n).collect();
            assert_eq!(moved(&all, &order), serial(&all, &order));
            // Some candidates were skipped.
            let some: Vec<usize> = (0..n).filter(|index| index % 3 != 1).collect();
            assert_eq!(moved(&all, &some), serial(&all, &some));
            // A candidate that was never converted is left out, as before.
            let mut holed = all.clone();
            holed[n / 2] = None;
            assert_eq!(moved(&holed, &order), serial(&holed, &order));
        }
        assert_eq!(moved(&[], &[]), serial(&[], &[]));
    }

    /// `cargo test --release -p n42-node --lib moving_a_full_block -- --ignored --nocapture`
    #[test]
    #[ignore = "timing"]
    fn moving_a_full_block_of_executed_candidates() {
        type Tx = Recovered<N42TxEnvelope>;
        let n = 95_238usize;
        let one = transaction(A, TxKind::Call(B), 0, 1, Bytes::new(), 21_000).to_consensus();
        let fresh = || -> Vec<std::sync::OnceLock<Tx>> {
            (0..n)
                .map(|_| {
                    let cell = std::sync::OnceLock::new();
                    let _ = cell.set(one.clone());
                    cell
                })
                .collect()
        };
        let order: Vec<usize> = (0..n).collect();
        let median = |mut v: Vec<f64>| {
            v.sort_by(f64::total_cmp);
            v[v.len() / 2]
        };
        let (mut before, mut after) = (Vec::new(), Vec::new());
        for _ in 0..11 {
            let mut cells = fresh();
            let mut out: Vec<Tx> = Vec::new();
            let at = std::time::Instant::now();
            for &index in &order {
                if let Some(tx) = cells[index].take() {
                    out.push(tx);
                }
            }
            before.push(at.elapsed().as_secs_f64() * 1e3);
            assert_eq!(out.len(), n);
            let cells = fresh();
            let mut out: Vec<Tx> = Vec::new();
            let at = std::time::Instant::now();
            move_executed(cells, &order, &mut out);
            after.push(at.elapsed().as_secs_f64() * 1e3);
            assert_eq!(out.len(), n);
        }
        println!(
            "MOVE_EXECUTED txs={n} bytes_each={} one_by_one_ms={:.1} side_by_side_ms={:.1}",
            std::mem::size_of::<Tx>(),
            median(before),
            median(after)
        );
    }
    use alloy_consensus::{Header, SignableTransaction, TxLegacy};
    use alloy_primitives::{Bytes, Signature, TxKind, address};
    use n42_tx_types::Receipt;
    use reth_ethereum_primitives::TransactionSigned;
    use reth_evm::execute::{BasicBlockExecutor, Executor};
    use reth_primitives_traits::SealedHeader;
    use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};
    use reth_transaction_pool::{
        TransactionOrigin,
        identifier::{SenderId, TransactionId},
        noop::NoopTransactionPool,
    };

    const A: Address = address!("1000000000000000000000000000000000000001");
    const B: Address = address!("2000000000000000000000000000000000000002");
    const C: Address = address!("3000000000000000000000000000000000000003");
    const FEE: Address = address!("4000000000000000000000000000000000000004");
    const CONTRACT: Address = address!("6000000000000000000000000000000000000006");
    type Item = Arc<ValidPoolTransaction<N42PooledTransaction>>;
    type Output = (BlockExecutionOutput<Receipt>, Vec<Address>);

    fn transaction(
        from: Address,
        to: TxKind,
        nonce: u64,
        value: u64,
        input: Bytes,
        gas: u64,
    ) -> Item {
        let signed: TransactionSigned = TxLegacy {
            chain_id: Some(n42_chainspec::N42_CHAIN_ID),
            nonce,
            gas_price: 9,
            gas_limit: gas,
            to,
            value: U256::from(value),
            input,
        }
        .into_signed(Signature::new(U256::from(1), U256::from(1), false))
        .into();
        Arc::new(ValidPoolTransaction {
            transaction: N42PooledTransaction::try_from_consensus(Recovered::new_unchecked(
                N42TxEnvelope::from(signed),
                from,
            ))
            .unwrap(),
            transaction_id: TransactionId::new(
                SenderId::from(if from == A { 0 } else { 1 }),
                nonce,
            ),
            propagate: false,
            timestamp: std::time::Instant::now(),
            origin: TransactionOrigin::External,
            authority_ids: None,
        })
    }

    type MockProvider = MockEthProvider<N42Primitives>;

    fn provider() -> MockProvider {
        let mut chain = n42_chainspec::n42_dev_chainspec();
        let chain_mut = Arc::make_mut(&mut chain);
        chain_mut.hardforks.insert(
            reth_chainspec::EthereumHardfork::Prague,
            reth_chainspec::ForkCondition::Timestamp(0),
        );
        chain_mut.genesis.config.prague_time = Some(0);
        let provider = MockProvider::new().with_chain_spec((*chain).clone());
        for (address, balance) in [
            (A, 100_000_000_000u64),
            (B, 100_000_000_000),
            (C, 10),
            (FEE, 1),
        ] {
            provider.add_account(address, ExtendedAccount::new(0, U256::from(balance)));
        }
        let mut code = vec![0x73];
        code.extend_from_slice(C.as_slice());
        code.extend_from_slice(&[0x31, 0x5f, 0x55, 0x00]); // SSTORE(0, BALANCE(C))
        provider.add_account(
            CONTRACT,
            ExtendedAccount::new(1, U256::ZERO).with_bytecode(code.into()),
        );
        provider
    }

    fn build(
        provider: MockProvider,
        txs: &[Item],
        parallel: bool,
        gas_limit: u64,
    ) -> (N42BuiltPayload, Output) {
        let config = N42EvmConfig::with_evm_factory(
            provider.chain_spec(),
            n42_execution::N42EvmFactory::with_fast_transfers(false),
        );
        let parent = Arc::new(SealedHeader::new_unhashed(Header {
            number: 10,
            gas_limit,
            gas_used: gas_limit / 2,
            base_fee_per_gas: Some(7),
            timestamp: 1_700_000_000,
            blob_gas_used: Some(0),
            excess_blob_gas: Some(0),
            ..Default::default()
        }));
        let attributes = EthPayloadAttributes {
            timestamp: 1_700_000_001,
            prev_randao: B256::repeat_byte(1),
            suggested_fee_recipient: FEE,
            parent_beacon_block_root: Some(B256::ZERO),
            withdrawals: Some(Default::default()),
            ..Default::default()
        };
        let args = BuildArguments::new(
            Default::default(),
            Default::default(),
            None,
            PayloadConfig::new(parent, attributes, Default::default()),
            Default::default(),
            None,
        );
        let pool = NoopTransactionPool::<N42PooledTransaction>::new();
        // Mock root is only a sentinel proving the synchronous provider call;
        // state correctness is checked through the complete published bundle below.
        let sentinel = B256::repeat_byte(0xa7);
        provider.state_roots.lock().push(sentinel);
        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let payload = metrics::with_local_recorder(&recorder, || {
            build_with_parallel(
                config,
                provider.clone(),
                pool.clone(),
                EthereumBuilderConfig::new().with_gas_limit(gas_limit),
                args,
                |_| {
                    Box::new(ReplayTransactions {
                        front: txs.iter().cloned().collect(),
                        inner: pool.best_transactions(),
                    })
                },
                parallel,
            )
            .unwrap()
            .into_payload()
            .unwrap()
        });
        assert!(
            provider.state_roots.lock().is_empty(),
            "complete synchronous root calculation called"
        );
        assert_eq!(payload.block().header().state_root, sentinel);
        if parallel {
            assert!(recorder.snapshotter().snapshot().into_vec().iter().any(|(key, _, _, value)|
                key.key().name() == "n42_parallel_payload_executed_total"
                && matches!(value, metrics_util::debugging::DebugValue::Counter(count) if *count > 0)),
                "the builder must execute a parallel prefix, not silently run all serially");
        }
        let mut output =
            reth_evm::payload_cache::take_payload_execution::<Output>(&payload.block().hash())
                .expect("execution cache must be published after graft revert finalization");
        let interpreter = N42EvmConfig::with_evm_factory(
            provider.chain_spec(),
            n42_execution::N42EvmFactory::with_fast_transfers(false),
        );
        let database = StateProviderDatabase::new(
            provider
                .state_by_block_hash(payload.block().header().parent_hash)
                .unwrap()
                .into_evm_state_provider(),
        );
        let mut reference = BasicBlockExecutor::new(interpreter, database)
            .execute(payload.recovered_block())
            .unwrap();
        for state in [&mut output.0.state, &mut reference.state] {
            for reverts in state.reverts.iter_mut() {
                reverts.sort_unstable_by_key(|(address, _)| *address);
            }
        }
        assert_eq!(
            output.0.result, reference.result,
            "published result must match independent execution of the actual built order"
        );
        assert_eq!(
            output.0.state, reference.state,
            "published bundle must match independent execution of the actual built order"
        );
        (payload, output)
    }

    fn compare_provider(provider: MockProvider, txs: &[Item], gas_limit: u64) {
        let (serial, (mut serial_output, serial_senders)) =
            build(provider.clone(), txs, false, gas_limit);
        let (parallel, (mut parallel_output, parallel_senders)) =
            build(provider, txs, true, gas_limit);
        assert_eq!(
            parallel.block(),
            serial.block(),
            "full block and transaction/receipt commitments"
        );
        assert_eq!(parallel_senders, serial_senders);
        assert_eq!(parallel_output.result, serial_output.result);
        for output in [&mut serial_output, &mut parallel_output] {
            for revert in output.state.reverts.iter_mut() {
                revert.sort_unstable_by_key(|(address, _)| *address);
            }
        }
        assert_eq!(
            parallel_output.state, serial_output.state,
            "published bundle including originals, code, storage and reverts"
        );
    }

    fn compare(txs: &[Item], gas_limit: u64) {
        compare_provider(provider(), txs, gas_limit);
    }

    #[test]
    fn online_builder_parallel_prefix_matches_serial_payload_and_published_execution() {
        let cancun = provider().with_chain_spec((*n42_chainspec::n42_dev_chainspec()).clone());
        let cancun_transfers = vec![
            transaction(A, TxKind::Call(C), 0, 5, Bytes::new(), 21_000),
            transaction(B, TxKind::Call(C), 0, 7, Bytes::new(), 21_000),
            transaction(
                A,
                TxKind::Call(CONTRACT),
                1,
                0,
                Bytes::from_static(&[0]),
                100_000,
            ),
        ];
        compare_provider(cancun, &cancun_transfers, 1_000_000);
        let mut txs = Vec::new();
        for nonce in 0..2_100 {
            for from in [A, B] {
                txs.push(transaction(
                    from,
                    TxKind::Call(C),
                    nonce,
                    1,
                    Bytes::new(),
                    21_000,
                ));
            }
        }
        compare(&txs, 200_000_000);
        // Mixed serial tail observes the grafted C balance, then spends from a
        // sender already touched by the parallel prefix.
        let mixed = vec![
            transaction(A, TxKind::Call(C), 0, 5, Bytes::new(), 21_000),
            transaction(
                B,
                TxKind::Call(CONTRACT),
                0,
                0,
                Bytes::from_static(&[0]),
                100_000,
            ),
            transaction(A, TxKind::Call(C), 1, 7, Bytes::new(), 21_000),
        ];
        compare(&mixed, 1_000_000);
        // A transfer funds the address created by the serial tail. Its original
        // revert must coexist with the CREATE storage revert.
        let created = B.create(0);
        let create = vec![
            transaction(A, TxKind::Call(created), 0, 1, Bytes::new(), 21_000),
            transaction(
                B,
                TxKind::Create,
                0,
                0,
                Bytes::from_static(&[0x60, 0x01, 0x5f, 0x55, 0x5f, 0x5f, 0xf3]),
                100_000,
            ),
        ];
        compare(&create, 1_000_000);
        compare(&txs[..6], 42_000);
        let invalid = vec![
            transaction(A, TxKind::Call(C), 0, 1, Bytes::new(), 21_000),
            transaction(A, TxKind::Call(C), 2, 1, Bytes::new(), 21_000),
            transaction(A, TxKind::Call(C), 3, 1, Bytes::new(), 21_000),
            transaction(B, TxKind::Call(C), 0, 1, Bytes::new(), 21_000),
        ];
        compare(&invalid, 1_000_000);
        // Structural transfer detection cannot see recipient bytecode. A refused
        // call is retried in the serial tail; validate its resulting new block order.
        let reordered = vec![
            transaction(A, TxKind::Call(C), 0, 5, Bytes::new(), 21_000),
            transaction(B, TxKind::Call(CONTRACT), 0, 0, Bytes::new(), 100_000),
            transaction(A, TxKind::Call(C), 1, 7, Bytes::new(), 21_000),
        ];
        let (payload, _) = build(provider(), &reordered, true, 1_000_000);
        assert_eq!(payload.block().body().transactions[1].nonce(), 1);
        assert_eq!(payload.block().body().transactions[2].to(), Some(CONTRACT));
        let funded = provider();
        funded.add_account(B, ExtendedAccount::new(0, U256::ZERO));
        let credits = vec![
            transaction(A, TxKind::Call(B), 0, 1, Bytes::new(), 21_000),
            transaction(B, TxKind::Call(C), 0, 100, Bytes::new(), 21_000),
            transaction(A, TxKind::Call(B), 1, 2_000_000, Bytes::new(), 21_000),
        ];
        let (payload, _) = build(funded, &credits, true, 1_000_000);
        assert_eq!(payload.recovered_block().senders(), &[A, A, B]);
        let mut osaka = provider();
        let chain = Arc::make_mut(&mut osaka.chain_spec);
        chain.hardforks.insert(
            reth_chainspec::EthereumHardfork::Osaka,
            reth_chainspec::ForkCondition::Timestamp(0),
        );
        chain.genesis.config.osaka_time = Some(0);
        let oversize = vec![
            transaction(A, TxKind::Call(C), 0, 1, Bytes::new(), 21_000),
            transaction(
                B,
                TxKind::Call(CONTRACT),
                0,
                0,
                vec![1; MAX_RLP_BLOCK_SIZE].into(),
                10_000_000,
            ),
            transaction(B, TxKind::Call(C), 1, 1, Bytes::new(), 21_000),
        ];
        compare_provider(osaka, &oversize, 200_000_000);
    }
}
