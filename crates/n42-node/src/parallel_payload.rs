//! N42 parallel prefix in the existing Ethereum payload lifecycle.
//! Based on pinned Reth 23316e3ff8adca8c3bd5085ff0565fcae019202a payload builder
//! plus this workspace's Reth patches. Keep the serial limits/finalization aligned.

use crate::engine_types::N42BuiltPayload;
use crate::native_payload::default_n42_payload;
use alloy_consensus::TransactionEnvelope as _;
use alloy_consensus::transaction::TxHashRef as _;
use alloy_consensus::{BlockHeader, Transaction};
use alloy_evm::{ToTxEnv, block::BlockExecutor as _};
use alloy_primitives::{Bloom, Bytes, U256};
use alloy_rlp::Encodable;
use alloy_rpc_types_engine::PayloadAttributes as EthPayloadAttributes;
use n42_execution::N42EvmConfig;
use n42_tx_types::{N42Primitives, N42TxEnvelope};
use reth_basic_payload_builder::{BuildArguments, BuildOutcome, PayloadConfig, is_better_payload};
use reth_chainspec::{ChainSpecProvider, EthChainSpec, EthereumHardforks};
use reth_consensus_common::validation::MAX_RLP_BLOCK_SIZE;
use reth_errors::{BlockExecutionError, BlockValidationError, ConsensusError};
use reth_ethereum_payload_builder::EthereumBuilderConfig;
use reth_evm::{
    ConfigureEvm, Evm, NextBlockEnvAttributes,
    block::TxResult,
    execute::{
        BasicBlockBuilder, BlockAssemblerInput, BlockBuilder, BlockBuilderOutcome,
        BlockExecutionOutput,
    },
};
use reth_execution_cache::{CachedStateMetrics, CachedStateMetricsSource, CachedStateProvider};
use reth_payload_builder::BlobSidecars;
use reth_payload_builder_primitives::PayloadBuilderError;
use reth_payload_primitives::PayloadAttributes;
use reth_primitives_traits::RecoveredBlock;
use reth_primitives_traits::transaction::error::InvalidTransactionError;
use reth_revm::{
    database::StateProviderDatabase,
    db::{State, states::bundle_state::BundleRetention},
};
use reth_storage_api::{EvmStateProvider, StateProvider, StateProviderFactory};
use reth_transaction_pool::{
    BestTransactions, BestTransactionsAttributes, PoolTransaction, TransactionPool,
    ValidPoolTransaction,
    error::{Eip4844PoolTransactionError, InvalidPoolTransactionError},
};
use revm::context_interface::{Block as _, Cfg as _};
use std::sync::Arc;
use tracing::{debug, info, trace, warn};

type BestTransactionsIter<Pool> = Box<
    dyn BestTransactions<Item = Arc<ValidPoolTransaction<<Pool as TransactionPool>::Transaction>>>,
>;

pub(crate) fn enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("N42_PARALLEL_BUILD").is_ok_and(|value| value == "1"))
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
    let BuildArguments {
        mut cached_reads,
        execution_cache,
        mut state_root_handle,
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
    // Direct bundle graft does not stream per-tx changes to the incremental root
    // job. Drop its hook/receiver and compute the complete root at finish.
    drop(state_root_handle.take());

    let state_provider = client.state_by_block_hash(parent_header.hash())?;
    // Reth 2.7 separates the EVM read provider from the full provider used by
    // block finalization. Open the same immutable parent twice so each API
    // retains its required owned provider type.
    let evm_state_provider = client
        .state_by_block_hash(parent_header.hash())?
        .into_evm_state_provider();
    let evm_state_provider: Box<dyn EvmStateProvider + Send> =
        if let Some(execution_cache) = execution_cache {
            Box::new(CachedStateProvider::new(
                evm_state_provider,
                execution_cache.cache().clone(),
                // It's ok to recreate the cache every time, because it's cheap to do so for a vanilla
                // Ethereum builder every 12s.
                Some(CachedStateMetrics::zeroed(
                    CachedStateMetricsSource::Builder,
                )),
            ))
        } else {
            Box::new(evm_state_provider)
        };
    let state = StateProviderDatabase::new(evm_state_provider);
    let chain_spec = client.chain_spec();
    let is_amsterdam = chain_spec.is_amsterdam_active_at_timestamp(attributes.timestamp());
    let mut db = State::builder()
        .with_database(cached_reads.as_db_mut(state))
        .with_bundle_update()
        .with_bal_builder_if(is_amsterdam)
        .build();

    let evm_config = evm_config.with_jit_support();
    let next_attributes = NextBlockEnvAttributes {
        timestamp: attributes.timestamp(),
        suggested_fee_recipient: attributes.suggested_fee_recipient,
        prev_randao: attributes.prev_randao,
        gas_limit: builder_config
            .gas_limit_with_target(parent_header.gas_limit, attributes.target_gas_limit()),
        parent_beacon_block_root: attributes.parent_beacon_block_root(),
        withdrawals: attributes.withdrawals.clone().map(Into::into),
        extra_data: builder_config.extra_data.clone(),
        slot_number: attributes.slot_number(),
    };
    let batch_env = evm_config
        .next_evm_env(&parent_header, &next_attributes)
        .map_err(PayloadBuilderError::other)?;
    let ctx = evm_config
        .context_for_next_block(&parent_header, next_attributes)
        .map_err(PayloadBuilderError::other)?;
    let evm = evm_config.evm_with_env(&mut db, batch_env.clone());
    let mut builder = BasicBlockBuilder::<
        <N42EvmConfig as ConfigureEvm>::BlockExecutorFactory,
        _,
        _,
        N42Primitives,
    > {
        executor: evm_config.create_executor(evm, ctx.clone()),
        ctx,
        parent: &parent_header,
        assembler: evm_config.block_assembler(),
        transactions: Vec::new(),
    };

    debug!(target: "payload_builder", id=%payload_id, parent_header = ?parent_header.hash(), parent_number = parent_header.number, "building new payload");
    let mut cumulative_tx_gas_used = 0;
    let mut block_regular_gas_used = 0;
    let mut block_state_gas_used = 0;
    let block_gas_limit: u64 = builder.evm_mut().block().gas_limit();
    let tx_gas_limit_cap = builder.evm_mut().cfg_env().tx_gas_limit_cap();
    let base_fee = builder.evm_mut().block().basefee();

    let mut best_txs = best_txs(BestTransactionsAttributes::new(
        base_fee,
        builder
            .evm_mut()
            .block()
            .blob_gasprice()
            .map(|gasprice| gasprice as u64),
    ));
    let mut total_fees = U256::ZERO;

    // If we have a state-root task, wire a state hook that streams per-tx state diffs.
    if let Some(task) = state_root_handle.as_mut() {
        builder
            .evm_mut()
            .db_mut()
            .set_state_hook(Some(Box::new(task.take_state_hook())));
    }

    builder.apply_pre_execution_changes().map_err(|err| {
        warn!(target: "payload_builder", %err, "failed to apply pre-execution changes");
        PayloadBuilderError::Internal(err.into())
    })?;

    // initialize empty blob sidecars at first. If cancun is active then this will be populated by
    // blob sidecars if any.
    let mut blob_sidecars = BlobSidecars::Empty;

    let mut block_blob_count = 0;
    let mut block_transactions_rlp_length = 0;

    let blob_params = chain_spec.blob_params_at_timestamp(attributes.timestamp);
    let protocol_max_blob_count = blob_params
        .as_ref()
        .map(|params| params.max_blob_count)
        .unwrap_or_else(Default::default);

    // Apply user-configured blob limit (EIP-7872)
    // Per EIP-7872: if the minimum is zero, set it to one
    let max_blob_count = builder_config
        .max_blobs_per_block
        .map(|user_limit| std::cmp::min(user_limit, protocol_max_blob_count).max(1))
        .unwrap_or(protocol_max_blob_count);

    let is_osaka = chain_spec.is_osaka_active_at_timestamp(attributes.timestamp);

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
    let mut evm_exec_total = std::time::Duration::ZERO;
    let mut iter_total = std::time::Duration::ZERO;
    let mut consensus_total = std::time::Duration::ZERO;

    // Optimization 1: Stop processing new arrivals during packing.
    // The iterator already holds a snapshot; new txs arriving mid-pack only add
    // overhead via broadcast channel polling (48K × try_recv calls).
    best_txs.no_updates();

    let mut graft_reverts = Vec::new();
    // False once a batch was grafted without the state's cache entries.
    let mut serial_allowed = true;
    // Transactions root of a block that is exactly the parallel prefix, derived
    // on a helper thread while the prefix is committed: (transaction count, job).
    let mut early_transactions_root: Option<(usize, std::thread::JoinHandle<alloy_primitives::B256>)> = None;
    let mut replay = std::collections::VecDeque::new();
    if chain_spec.is_cancun_active_at_timestamp(attributes.timestamp()) && !is_amsterdam {
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
                && pool_tx.sender() != batch_env.block_env.beneficiary
                && to != batch_env.block_env.beneficiary;
            let length = if is_osaka {
                pool_tx.transaction.consensus_ref().inner().length()
            } else {
                0
            };
            // Reserve declared gas, which bounds actual gas even when candidates
            // are refused. The ordinary loop can spend any unused capacity later.
            let fits = pool_tx.gas_limit() <= block_gas_limit.saturating_sub(reserved_gas)
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
            let prestate = builder.evm_mut().db_mut().cache.clone();
            let batch_started = std::time::Instant::now();
            let convert = |i: usize| {
                let tx = candidates[i].to_consensus();
                let env = tx.to_tx_env();
                (tx, env)
            };
            let open = || {
                let provider = client.state_by_block_hash(parent_header.hash()).ok()?;
                Some(
                    State::builder()
                        .with_database(StateProviderDatabase::new(
                            provider.into_evm_state_provider(),
                        ))
                        .with_cached_prestate(prestate.clone())
                        .build(),
                )
            };
            let beneficiary = batch_env.block_env.beneficiary;
            // N42_GRAFT_NO_CACHE=1: a batch that fills the block, in a block
            // without withdrawals, is grafted into the bundle only. Nothing
            // may read a grafted account from the state afterwards, so the
            // serial loop below does not run for such a block.
            static NO_CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            let graft_keeps_cache = !(*NO_CACHE.get_or_init(|| {
                std::env::var("N42_GRAFT_NO_CACHE").is_ok_and(|value| value == "1")
            }) && candidates.len() >= max_txs_per_block
                && attributes
                    .withdrawals
                    .as_ref()
                    .is_none_or(|withdrawals| withdrawals.is_empty()));
            // N42_GRAFT_STREAM=1: each batch's bundle is grafted on this
            // thread as the batch ends, while the later batches still run,
            // instead of all of them after the last one.
            static STREAM: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            let stream = *STREAM.get_or_init(|| {
                std::env::var("N42_GRAFT_STREAM").is_ok_and(|value| value == "1")
            });
            static ROOT_EARLY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            if !graft_keeps_cache
                && *ROOT_EARLY
                    .get_or_init(|| std::env::var("N42_TX_ROOT_EARLY").is_ok_and(|v| v == "1"))
            {
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
            let mut graft_ms = 0u64;
            let (run, streamed_graft) = if stream {
                let (bundle_tx, bundle_rx) = std::sync::mpsc::channel();
                let state: &mut State<_> = builder.evm_mut().db_mut();
                let want = keys.len() * 2;
                state.bundle_state.state.reserve(want);
                if graft_keeps_cache {
                    state.cache.accounts.reserve(want);
                }
                let mut graft = n42_execution::parallel_transfer::Graft::default();
                let mut graft_error = None;
                let mut graft_us = 0u128;
                let joined = std::thread::scope(|scope| {
                    let worker = scope.spawn(|| {
                        let sink = move |bundle| {
                            let _ = bundle_tx.send(bundle);
                        };
                        n42_execution::parallel_transfer::execute_for_build_streamed(
                            &batch_env, &keys, &convert, &open, &sink,
                        )
                    });
                    for bundle in bundle_rx.iter() {
                        if graft_error.is_some() {
                            continue;
                        }
                        let at = std::time::Instant::now();
                        if let Err(error) = n42_execution::parallel_transfer::graft_bundles_with(
                            state,
                            vec![bundle],
                            beneficiary,
                            graft_keeps_cache,
                        )
                        .and_then(|part| graft.absorb(part, beneficiary))
                        {
                            graft_error = Some(error.to_string());
                        }
                        // The first bundle may have become the block's bundle
                        // as it was, without the room reserved above.
                        let have = state.bundle_state.state.len();
                        if state.bundle_state.state.capacity() < want && have < want {
                            state.bundle_state.state.reserve(want - have);
                        }
                        graft_us += at.elapsed().as_micros();
                    }
                    worker.join()
                });
                let run = joined
                    .map_err(|_| {
                        PayloadBuilderError::other(std::io::Error::other(
                            "parallel transfer executor panicked",
                        ))
                    })?
                    .map_err(PayloadBuilderError::other)?;
                if let Some(error) = graft_error {
                    return Err(PayloadBuilderError::other(std::io::Error::other(error)));
                }
                graft_ms = (graft_us / 1000) as u64;
                (run, Some(graft))
            } else {
                let run = n42_execution::parallel_transfer::execute_for_build(
                    &batch_env, &keys, &convert, &open,
                )
                .map_err(PayloadBuilderError::other)?;
                (run, None)
            };
            if cancel.is_cancelled() {
                return Ok(BuildOutcome::Cancelled);
            }
            let accepted = run.executed.len();
            let batches = run.phases.batches;
            let phases = run.phases;
            let run_ms = batch_started.elapsed().as_millis() as u64;
            let skipped: Vec<_> = run.skipped.iter().map(|&i| candidates[i].clone()).collect();
            for executed in &run.executed {
                let fee = executed.tx.effective_tip_per_gas(base_fee).ok_or_else(|| {
                    PayloadBuilderError::other(std::io::Error::other(
                        "invalid executed transfer fee",
                    ))
                })?;
                total_fees += U256::from(fee) * U256::from(executed.gas_used);
                cumulative_tx_gas_used += executed.gas_used;
                block_regular_gas_used += executed.result.gas().block_regular_gas_used();
                block_state_gas_used += executed.result.gas().block_state_gas_used();
                block_transactions_rlp_length += lengths[executed.index];
            }
            let fee_ms = batch_started.elapsed().as_millis() as u64 - run_ms;
            let commit_started = std::time::Instant::now();
            // The job hashed every candidate; its root stands only if all
            // of them executed (they keep candidate order in the block).
            if accepted != candidates.len() {
                early_transactions_root = None;
            }
            let (transactions, graft) = match streamed_graft {
                Some(graft) => n42_execution::parallel_block::finish_transfer_batch(
                    &mut builder.executor,
                    run.executed,
                    graft,
                    beneficiary,
                    |tx| (*tx.tx_hash(), tx.tx_type()),
                ),
                None => n42_execution::parallel_block::commit_transfer_batch(
                    &mut builder.executor,
                    run,
                    beneficiary,
                    |tx| (*tx.tx_hash(), tx.tx_type()),
                    graft_keeps_cache,
                ),
            }
            .map_err(PayloadBuilderError::other)?;
            serial_allowed = graft_keeps_cache;
            let commit_ms = commit_started.elapsed().as_millis() as u64;
            builder.transactions.extend(transactions);
            graft_reverts = graft.reverts;
            tx_count = accepted;
            let tail = std::mem::take(&mut replay);
            replay.extend(skipped);
            replay.extend(tail);
            evm_exec_total += batch_started.elapsed();
            metrics::counter!("n42_parallel_payload_executed_total").increment(accepted as u64);
            metrics::histogram!("n42_parallel_payload_batch_ms")
                .record(batch_started.elapsed().as_secs_f64() * 1_000.0);
            info!(target: "n42::payload", accepted, skipped = replay.len(), batches,
                run_ms,
                fee_ms,
                commit_ms,
                streamed_graft_ms = graft_ms,
                keeps_cache = graft_keeps_cache,
                total_ms = batch_started.elapsed().as_millis() as u64,
                partition_ms = phases.partition_ms,
                groups_ms = phases.groups_ms,
                open_ms_sum = phases.open_ms_sum,
                run_ms_sum = phases.run_ms_sum,
                batch_merge_ms_sum = phases.batch_merge_ms_sum,
                batch_ms_max = phases.batch_ms_max,
                collect_ms = phases.collect_ms,
                "parallel transfer prefix executed; not a consensus commit");
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
        // ensure we still have capacity for this transaction
        let exceeds_gas_limit = if is_amsterdam {
            let regular_available_gas = block_gas_limit.saturating_sub(block_regular_gas_used);
            let state_available_gas = block_gas_limit.saturating_sub(block_state_gas_used);
            let regular_tx_gas_limit = pool_tx.gas_limit().min(tx_gas_limit_cap);

            if regular_tx_gas_limit > regular_available_gas {
                Some((regular_tx_gas_limit, regular_available_gas))
            } else if pool_tx.gas_limit() > state_available_gas {
                Some((pool_tx.gas_limit(), state_available_gas))
            } else {
                None
            }
        } else {
            let block_available_gas = block_gas_limit.saturating_sub(cumulative_tx_gas_used);
            (pool_tx.gas_limit() > block_available_gas)
                .then_some((pool_tx.gas_limit(), block_available_gas))
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

        // check if the job was cancelled, if so we can exit early
        if cancel.is_cancelled() {
            return Ok(BuildOutcome::Cancelled);
        }

        // Optimization 3: Defer to_consensus() conversion — only needed for txs
        // that pass the gas check above. Also track its cost separately.
        let consensus_start = std::time::Instant::now();
        let tx = pool_tx.to_consensus();
        consensus_total += consensus_start.elapsed();

        // Optimization 2: Skip RLP length computation and block size check
        // for non-Osaka chains. The MAX_RLP_BLOCK_SIZE limit only applies to Osaka.
        // This saves ~2µs/tx of RLP encoding traversal.
        let tx_rlp_len = if is_osaka {
            let len = tx.inner().length();
            let estimated_block_size_with_tx =
                block_transactions_rlp_length + len + withdrawals_rlp_length + 1024;

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
                trace!(target: "payload_builder", tx=?tx.hash(), ?block_blob_count, "skipping blob transaction because it would exceed the max blob count per block");
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

        let mut tx_regular_gas_used = 0;
        let evm_start = std::time::Instant::now();
        let gas_output = match builder.execute_transaction_with_result_closure(tx, |result| {
            tx_regular_gas_used = result.result().result.gas().block_regular_gas_used();
        }) {
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
        block_regular_gas_used += tx_regular_gas_used;
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
        block,
        block_access_list,
        ..
    } = if skip_state_root && !is_amsterdam && crate::payload::builder_state_root_replaced() {
        // H2-v4 normalization overwrites both the state root (QMDB root) and the
        // receipts root (Gov5 flat receipts hash) of every built block, and only
        // the normalized block is proposed. The upstream finish would still hash
        // the post state and build the MPT receipts trie; skip both. This mirrors `BasicBlockBuilder::finish` without those two steps; Amsterdam
        // (block access lists) is excluded above and takes the upstream path.
        let BasicBlockBuilder {
            executor,
            transactions,
            ctx,
            parent,
            assembler,
        } = builder;
        let (evm, execution_result) = executor.finish()?;
        let (finished_db, evm_env) = evm.finish();
        let executor_finish_ms = finish_start.elapsed().as_millis() as u64;
        finished_db.merge_transitions(BundleRetention::Reverts);
        let merge_ms = finish_start.elapsed().as_millis() as u64 - executor_finish_ms;
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
                    n42_execution::restored_slots_env_key(&evm_env.block_env),
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
        let prague = chain_spec.is_prague_active_at_timestamp(attributes.timestamp);
        let bundle_state = &finished_db.bundle_state;
        let (qmdb_root, block) = std::thread::scope(|scope| {
            let qmdb_root = qmdb_root_job.map(|(store, key, parent_hash)| {
                scope.spawn(move || {
                    let started = std::time::Instant::now();
                    let restored = n42_execution::restored_slots_for(key).unwrap_or_default();
                    let operations =
                        crate::exec_cache::gov5_sorted_operations(bundle_state, &restored, prague);
                    let root = store.prepare_candidate(parent_hash, operations);
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
            let (transactions, senders): (Vec<_>, Vec<_>) =
                transactions.into_iter().map(|tx| tx.into_parts()).unzip();
            let block = assembler
                .assemble_block(
                    BlockAssemblerInput::<<N42EvmConfig as ConfigureEvm>::BlockExecutorFactory>::new(
                        evm_env,
                        ctx,
                        parent,
                        transactions,
                        &execution_result,
                        bundle_state,
                        state_provider.as_ref(),
                        parent_header.state_root(),
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
                merge_ms,
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
            hashed_state: Default::default(),
            trie_updates: Default::default(),
            block,
            block_access_list: None,
        }
    } else if skip_state_root {
        debug!(
            target: "payload_builder",
            id = %payload_id,
            state_root = ?parent_header.state_root(),
            "skipping payload state-root computation"
        );
        builder.finish(
            state_provider.as_ref(),
            Some((parent_header.state_root(), Default::default())),
        )?
    } else if let Some(mut task) = state_root_handle {
        // Drop the state hook, which signals the state-root task to finalize.
        builder.evm_mut().db_mut().set_state_hook(None);

        // The state-root task has been computing incrementally alongside tx execution.
        // This recv() waits for the final root hash — most work is already done.
        // Fall back to sync state root if the trie pipeline fails.
        match task.state_root() {
            Ok(outcome) => {
                debug!(target: "payload_builder", id=%payload_id, state_root=?outcome.state_root, job = task.name(), "received state root from state-root job");
                builder.finish(
                    state_provider.as_ref(),
                    Some((
                        outcome.state_root,
                        Arc::unwrap_or_clone(outcome.trie_updates),
                    )),
                )?
            }
            Err(err) => {
                warn!(target: "payload_builder", id=%payload_id, %err, "state-root job failed, falling back to sync state root");
                builder.finish(state_provider.as_ref(), None)?
            }
        }
    } else {
        builder.finish(state_provider.as_ref(), None)?
    };

    let finish_ms = finish_start.elapsed().as_secs_f64() * 1000.0;
    metrics::histogram!("n42_payload_builder_finish_ms").record(finish_ms);

    // Extract requests before moving execution_result into cache.
    let requests = chain_spec
        .is_prague_active_at_timestamp(attributes.timestamp)
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

    // Cache execution output for leader's new_payload AND broadcast to followers.
    // Use the recovered block by reference — it is still needed to build the payload below,
    // so we must not consume it via into_sealed_block().
    let cache_start = std::time::Instant::now();
    {
        let mut bundle_state = db.take_bundle();
        n42_execution::parallel_transfer::append_reverts(&mut bundle_state, graft_reverts);
        let block_hash = block.hash();
        let transactions_root = block.header().transactions_root();
        let senders = block.senders().to_vec();
        let execution_output = BlockExecutionOutput {
            state: bundle_state,
            result: execution_result,
        };
        reth_evm::payload_cache::store_payload_transactions_root(block_hash, transactions_root);
        if let Some(roots) = built_gov5_roots {
            crate::exec_cache::store_built_gov5_roots(block_hash, roots);
        }
        // The second copy exists to be sent to peers. A block whose roots the
        // H2-v4 normalization replaces is sent without its output unless
        // shared execution is on; the normalization then borrows the copy
        // kept for this node's own import.
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
    }
    debug!(target: "payload_builder", id=%payload_id, sealed_block_header = ?block.sealed_header(), "sealed built block");

    let block_access_list: Option<Bytes> =
        block_access_list.map(|block_access_list| alloy_rlp::encode(&block_access_list).into());
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
    use alloy_consensus::{Header, SignableTransaction, TxLegacy};
    use alloy_primitives::{Address, B256, Signature, TxKind, address};
    use n42_tx_types::Receipt;
    use reth_ethereum_primitives::TransactionSigned;
    use reth_evm::execute::{BasicBlockExecutor, Executor};
    use reth_primitives_traits::{Recovered, SealedHeader};
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
