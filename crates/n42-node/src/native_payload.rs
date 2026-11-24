//! Default payload building over [`N42Primitives`].
//!
//! Ported from reth v2.11.0 `reth-ethereum-payload-builder::default_ethereum_payload`
//! (crates/ethereum/payload/src/lib.rs). The upstream function is bound to
//! Ethereum's primitives; this copy only swaps the transaction, primitives and
//! built-payload types so 0x50 transactions can be packed. Keep the logic in
//! step with upstream when rebasing reth.

use crate::engine_types::N42BuiltPayload;
use alloy_consensus::Transaction;
use alloy_consensus::transaction::TxHashRef as _;
use alloy_primitives::U256;
use alloy_rlp::Encodable;
use alloy_rpc_types_engine::PayloadAttributes as EthPayloadAttributes;
use n42_tx_types::{N42Primitives, N42TxEnvelope};
use reth_basic_payload_builder::{BuildArguments, BuildOutcome, PayloadConfig, is_better_payload};
use reth_chainspec::{ChainSpecProvider, EthChainSpec, EthereumHardforks};
use reth_consensus_common::validation::MAX_RLP_BLOCK_SIZE;
use reth_errors::ConsensusError;
use reth_ethereum_payload_builder::EthereumBuilderConfig;
use reth_evm::{
    ConfigureEvm, EvmEnv, NextBlockEnvAttributes, OnStateHook,
    database::StateProviderDatabase,
    execute::{
        BlockBuilder, BlockBuilderOutcome, BlockExecutionError, BlockExecutionOutput,
        BlockExecutor, BlockValidationError,
    },
};
use reth_execution_cache::{CachedStateMetrics, CachedStateMetricsSource, CachedStateProvider};
use reth_payload_builder::BlobSidecars;
use reth_payload_builder_primitives::PayloadBuilderError;
use reth_payload_primitives::PayloadAttributes as _;
use reth_primitives_traits::transaction::error::InvalidTransactionError;
use reth_storage_api::{EvmStateProvider, StateProvider, StateProviderFactory};
use reth_transaction_pool::{
    BestTransactions, BestTransactionsAttributes, PoolTransaction, TransactionPool,
    ValidPoolTransaction,
    error::{Eip4844PoolTransactionError, InvalidPoolTransactionError},
};
use reth_trie_common::updates::TrieUpdates;
use std::sync::Arc;
use tracing::{debug, trace, warn};

/// Boxed best-transactions iterator for a pool.
pub type BestTransactionsIter<Pool> = Box<
    dyn BestTransactions<Item = Arc<ValidPoolTransaction<<Pool as TransactionPool>::Transaction>>>,
>;

/// Constructs an Ethereum transaction payload using the best transactions from the pool.
///
/// Given build arguments including an Ethereum client, transaction pool,
/// and configuration, this function creates a transaction payload. Returns
/// a result indicating success with the payload or an error in case of failure.
#[inline]
pub fn default_n42_payload<EvmConfig, Client, Pool, F>(
    evm_config: EvmConfig,
    client: Client,
    pool: Pool,
    builder_config: EthereumBuilderConfig,
    args: BuildArguments<EthPayloadAttributes, N42BuiltPayload>,
    best_txs: F,
) -> Result<BuildOutcome<N42BuiltPayload>, PayloadBuilderError>
where
    EvmConfig: ConfigureEvm<Primitives = N42Primitives, NextBlockEnvCtx = NextBlockEnvAttributes>,
    Client: StateProviderFactory + ChainSpecProvider<ChainSpec: EthereumHardforks>,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = N42TxEnvelope>>,
    F: FnOnce(BestTransactionsAttributes) -> BestTransactionsIter<Pool>,
{
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
    let stream_state_updates = state_root_handle.is_some() && !skip_state_root;
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
    let mut builder = evm_config
        .builder_for_next_block(cached_db, &parent_header, next_block_env_attributes)
        .map_err(PayloadBuilderError::other)?;
    if is_amsterdam {
        builder.executor_mut().enable_block_access_list_builder();
    }
    let base_fee = builder.evm_env().block_base_fee();
    let blob_fee = blob_params
        .as_ref()
        .map(|_| builder.evm_env().block_blob_base_fee());
    let separate_block_gas = builder.evm_env().uses_separate_block_gas();
    let regular_gas_limit_cap = builder.evm_env().regular_gas_limit_cap();

    // If we have a state-root task, wire a state hook that streams per-tx state diffs.
    let use_state_root_task =
        if let Some(task) = state_root_handle.as_mut().filter(|_| stream_state_updates) {
            let mut hook = task.take_state_hook();
            builder.set_state_hook(move |state| hook.on_state(state))
        } else {
            false
        };

    builder.apply_pre_execution_changes().map_err(|err| {
        warn!(target: "payload_builder", %err, "failed to apply pre-execution changes");
        PayloadBuilderError::Internal(err.into())
    })?;

    let mut best_txs = best_txs(BestTransactionsAttributes::new(base_fee, blob_fee));
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
    // AM2: on Profile 1 `hotstuff.maxBlockBytes` replaces the 8 MiB EIP-7934
    // cap at every fork; other chains keep the cap from Osaka on.
    let block_size_limit =
        n42_chainspec::profile1::installed_block_rlp_size_limit(is_osaka, MAX_RLP_BLOCK_SIZE);
    let withdrawals_rlp_length = attributes
        .withdrawals
        .as_ref()
        .map(|withdrawals| withdrawals.length())
        .unwrap_or(0);

    while let Some(pool_tx) = best_txs.next() {
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

        // convert tx to a signed transaction
        let tx = pool_tx.to_consensus();
        let tx_rlp_len = tx.inner().length();
        let estimated_block_size_with_tx =
            block_transactions_rlp_length + tx_rlp_len + withdrawals_rlp_length + 1024; // 1Kb of overhead for the block header

        if let Some(limit) = block_size_limit
            && estimated_block_size_with_tx > limit
        {
            best_txs.mark_invalid(
                &pool_tx,
                InvalidPoolTransactionError::OversizedData {
                    size: estimated_block_size_with_tx,
                    limit,
                },
            );
            continue;
        }

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

        // Add blob tx sidecar to the payload.
        if let Some(sidecar) = blob_tx_sidecar {
            blob_sidecars.push_sidecar_variant(sidecar.as_ref().clone());
        }
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

    let BlockBuilderOutcome {
        execution_result,
        execution_state,
        block,
        block_access_list,
        ..
    } = if skip_state_root {
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
    } else if use_state_root_task {
        let mut task = state_root_handle.expect("state-root task exists if hook was installed");
        // The state-root task has been computing incrementally alongside tx execution.
        // This recv() waits for the final root hash — most work is already done.
        // Fall back to sync state root if the trie pipeline fails.
        builder.finish_with_state_root(state_provider.as_ref(), |_| match task.state_root() {
            Ok(outcome) => {
                debug!(
                    target: "payload_builder",
                    id = %payload_id,
                    state_root = ?outcome.state_root,
                    job = task.name(),
                    "received state root from state-root job"
                );
                Ok(Some((outcome.state_root, Arc::unwrap_or_clone(outcome.trie_updates))))
            }
            Err(err) => {
                warn!(target: "payload_builder", id = %payload_id, %err, "state-root job failed, falling back to sync state root");
                Ok(None)
            }
        })?
    } else {
        builder.finish(state_provider.as_ref(), None)?
    };

    let requests = chain_spec
        .is_prague_active_at_timestamp(attributes.timestamp())
        .then(|| execution_result.requests.clone());

    // Retain the transaction root and execution output so compact followers
    // can reuse work already authenticated by the leader's sealed block hash.
    let block_hash = block.hash();
    let transactions_root = block.header().transactions_root;
    let senders = block.senders().to_vec();
    let execution_output = BlockExecutionOutput {
        state: execution_state,
        result: execution_result,
    };
    reth_evm::payload_cache::store_payload_transactions_root(block_hash, transactions_root);
    reth_evm::payload_cache::store_broadcast_execution(
        block_hash,
        (execution_output.clone(), senders.clone()),
    );
    reth_evm::payload_cache::store_payload_execution(block_hash, (execution_output, senders));

    debug!(
        target: "payload_builder",
        id = %payload_id,
        sealed_block_header = ?block.sealed_header(),
        "sealed built block"
    );

    if let Some(limit) = block_size_limit
        && block.rlp_length() > limit
    {
        return Err(PayloadBuilderError::other(ConsensusError::BlockTooLarge {
            rlp_length: block.rlp_length(),
            max_rlp_length: limit,
        }));
    }

    let block_access_list = block_access_list.map(|bal| bal.split().1);
    let payload = N42BuiltPayload::new(Arc::new(block), total_fees, requests, block_access_list)
        // add blob sidecars from the executed txs
        .with_sidecars(blob_sidecars);

    Ok(BuildOutcome::Better {
        payload,
        cached_reads,
    })
}
