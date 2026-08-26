use crate::consensus_state::SharedConsensusState;
use rayon::prelude::*;
use reth_basic_payload_builder::{
    BuildArguments, BuildOutcome, MissingPayloadBehaviour, PayloadBuilder, PayloadConfig,
};
use reth_chainspec::{ChainSpecProvider, EthChainSpec, EthereumHardforks};
use reth_ethereum_engine_primitives::{EthBuiltPayload, EthPayloadAttributes};
use reth_ethereum_payload_builder::{EthereumBuilderConfig, default_ethereum_payload};
use reth_ethereum_primitives::{EthPrimitives, TransactionSigned};
use reth_evm::{ConfigureEvm, NextBlockEnvAttributes};
use reth_node_api::{FullNodeTypes, NodeTypes, PrimitivesTy, TxTy};
use reth_node_builder::{
    BuilderContext, PayloadBuilderConfig, PayloadTypes, components::PayloadBuilderBuilder,
};
use reth_payload_builder_primitives::PayloadBuilderError;
use reth_storage_api::StateProviderFactory;
use reth_transaction_pool::{
    BestTransactions, BestTransactionsAttributes, ValidPoolTransaction,
    error::InvalidPoolTransactionError,
};
use reth_transaction_pool::{PoolTransaction, TransactionPool};
use std::{
    cmp::Reverse,
    collections::{BinaryHeap, HashMap, HashSet, VecDeque},
    sync::Arc,
};

fn execution_lanes() -> usize {
    std::env::var("N42_EXECUTION_LANES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(8)
        .clamp(6, 8)
}

fn execution_lane_pool() -> &'static rayon::ThreadPool {
    static POOL: std::sync::OnceLock<rayon::ThreadPool> = std::sync::OnceLock::new();
    POOL.get_or_init(|| {
        let lanes = execution_lanes();
        rayon::ThreadPoolBuilder::new()
            .num_threads(lanes)
            .thread_name(|index| format!("n42-exec-lane-{index}"))
            .build()
            .expect("N42 execution lane pool must initialize")
    })
}

fn sender_sharded_drain_enabled() -> bool {
    !matches!(
        std::env::var("N42_SENDER_SHARDED_DRAIN").ok().as_deref(),
        Some("0") | Some("false") | Some("off")
    )
}

/// Immutable txpool snapshot prepared in sender shards, then merged by a
/// deterministic `(tip, sender, hash)` total order. Transactions of one sender
/// remain nonce-ordered and `mark_invalid` suppresses its entire remaining
/// suffix, matching `BestTransactions` dependency semantics.
struct SenderShardedTransactions<T: PoolTransaction> {
    ordered: VecDeque<Arc<ValidPoolTransaction<T>>>,
    invalid_senders: HashSet<alloy_primitives::Address>,
    skip_blobs: bool,
}

impl<T: PoolTransaction> Iterator for SenderShardedTransactions<T> {
    type Item = Arc<ValidPoolTransaction<T>>;

    fn next(&mut self) -> Option<Self::Item> {
        while let Some(tx) = self.ordered.pop_front() {
            if self.invalid_senders.contains(tx.sender_ref()) {
                continue;
            }
            if self.skip_blobs && tx.is_eip4844() {
                self.invalid_senders.insert(tx.sender());
                continue;
            }
            return Some(tx);
        }
        None
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, Some(self.ordered.len()))
    }
}

impl<T: PoolTransaction> BestTransactions for SenderShardedTransactions<T> {
    fn mark_invalid(&mut self, tx: &Self::Item, _kind: InvalidPoolTransactionError) {
        self.invalid_senders.insert(tx.sender());
    }

    fn no_updates(&mut self) {}

    fn set_skip_blobs(&mut self, skip_blobs: bool) {
        self.skip_blobs = skip_blobs;
    }
}

fn sender_sharded_transactions<T: PoolTransaction>(
    pending: Vec<Arc<ValidPoolTransaction<T>>>,
    attributes: BestTransactionsAttributes,
) -> SenderShardedTransactions<T> {
    let started = std::time::Instant::now();
    let lanes = execution_lanes().min(pending.len().max(1));

    // Each Rayon fold owns a lane-local map, so grouping never takes a shared
    // sender lock. Reduction order is deliberately irrelevant: every sender
    // chain is sorted by nonce/hash below before it enters the deterministic
    // heap merge.
    let grouped: HashMap<alloy_primitives::Address, Vec<Arc<ValidPoolTransaction<T>>>> =
        execution_lane_pool().install(|| {
            pending
                .into_par_iter()
                .fold(
                    HashMap::new,
                    |mut shard: HashMap<
                        alloy_primitives::Address,
                        Vec<Arc<ValidPoolTransaction<T>>>,
                    >,
                     tx| {
                        shard.entry(tx.sender()).or_default().push(tx);
                        shard
                    },
                )
                .reduce(HashMap::new, |mut left, right| {
                    for (sender, mut transactions) in right {
                        left.entry(sender).or_default().append(&mut transactions);
                    }
                    left
                })
        });
    let grouped_at = std::time::Instant::now();

    let mut chains: Vec<Vec<Arc<ValidPoolTransaction<T>>>> = grouped.into_values().collect();
    execution_lane_pool().install(|| {
        chains.par_iter_mut().for_each(|chain| {
            chain.sort_unstable_by(|left, right| {
                left.nonce()
                    .cmp(&right.nonce())
                    .then_with(|| left.hash().cmp(right.hash()))
            });

            // The chain is nonce ordered. Once its head cannot pay the block
            // fees, no descendant can be executable even if it individually
            // offers more; truncate the complete suffix in its owning lane.
            let executable_prefix = chain
                .iter()
                .position(|tx| {
                    tx.max_fee_per_gas() < attributes.basefee as u128
                        || tx.max_fee_per_blob_gas().is_some_and(|fee| {
                            fee < attributes.blob_fee.unwrap_or_default() as u128
                        })
                })
                .unwrap_or(chain.len());
            chain.truncate(executable_prefix);
        });
    });
    let prepared_at = std::time::Instant::now();

    // Move, rather than clone, the snapshot's Arc handles into sender chains.
    // This removes two atomic refcount operations per transaction from the
    // build hot path.
    let mut chains: Vec<VecDeque<Arc<ValidPoolTransaction<T>>>> = chains
        .into_iter()
        .filter(|chain| !chain.is_empty())
        .map(VecDeque::from)
        .collect();
    let sender_count = chains.len();

    // Compute each sender head independently. The heap merge is intentionally
    // serial and tiny (one entry per sender), making the output independent of
    // lane completion order.
    let mut heap = BinaryHeap::new();
    for (chain_index, chain) in chains.iter().enumerate() {
        let tx = &chain[0];
        heap.push((
            tx.effective_tip_per_gas(attributes.basefee)
                .unwrap_or_default(),
            Reverse(tx.sender()),
            Reverse(*tx.hash()),
            chain_index,
            0usize,
        ));
    }
    let mut ordered = VecDeque::with_capacity(chains.iter().map(VecDeque::len).sum());
    let mut heap_runs = 0u64;
    let mut batched_transactions = 0u64;
    while let Some((selected_tip, _, _, chain_index, tx_index)) = heap.pop() {
        heap_runs += 1;
        debug_assert_eq!(tx_index, 0, "sender head is consumed exactly once");
        loop {
            let tx = chains[chain_index]
                .pop_front()
                .expect("heap entry references a sender head");
            ordered.push_back(tx);
            let Some(next) = chains[chain_index].front() else {
                break;
            };
            let next_tip = next
                .effective_tip_per_gas(attributes.basefee)
                .unwrap_or_default();
            if next_tip == selected_tip {
                // Sender precedes hash in the total-order key. With the same
                // sender and tip, this next nonce remains ahead of every peer
                // the just-popped head outranked, so releasing the whole run
                // is byte-for-byte equivalent to pop+push for every tx.
                batched_transactions += 1;
                continue;
            }
            heap.push((
                next_tip,
                Reverse(next.sender()),
                Reverse(*next.hash()),
                chain_index,
                0usize,
            ));
            break;
        }
    }

    let merged_at = std::time::Instant::now();
    let elapsed = merged_at.duration_since(started);
    metrics::histogram!("n42_sender_sharded_drain_ms").record(elapsed.as_secs_f64() * 1_000.0);
    metrics::histogram!("n42_sender_sharded_group_ms")
        .record(grouped_at.duration_since(started).as_secs_f64() * 1_000.0);
    metrics::histogram!("n42_sender_sharded_prepare_ms")
        .record(prepared_at.duration_since(grouped_at).as_secs_f64() * 1_000.0);
    metrics::histogram!("n42_sender_sharded_merge_ms")
        .record(merged_at.duration_since(prepared_at).as_secs_f64() * 1_000.0);
    metrics::counter!("n42_sender_sharded_heap_runs_total").increment(heap_runs);
    metrics::counter!("n42_sender_sharded_batched_transactions_total")
        .increment(batched_transactions);
    metrics::gauge!("n42_execution_lanes").set(lanes as f64);
    metrics::gauge!("n42_sender_sharded_snapshot_txs").set(ordered.len() as f64);
    tracing::debug!(
        target: "n42::payload",
        lanes,
        senders = sender_count,
        transactions = ordered.len(),
        group_ms = grouped_at.duration_since(started).as_millis() as u64,
        prepare_ms = prepared_at.duration_since(grouped_at).as_millis() as u64,
        merge_ms = merged_at.duration_since(prepared_at).as_millis() as u64,
        heap_runs,
        batched_transactions,
        elapsed_ms = elapsed.as_millis() as u64,
        "sender-sharded txpool snapshot deterministically merged"
    );

    SenderShardedTransactions {
        ordered,
        invalid_senders: HashSet::new(),
        skip_blobs: false,
    }
}

#[cfg(test)]
mod sender_sharded_tests {
    use super::*;
    use alloy_primitives::{Address, B256};
    use reth_transaction_pool::test_utils::{MockTransaction, MockTransactionFactory};

    fn tx(
        factory: &mut MockTransactionFactory,
        sender: u8,
        nonce: u64,
        gas_price: u128,
        hash: u8,
    ) -> Arc<ValidPoolTransaction<MockTransaction>> {
        factory.validated_arc(
            MockTransaction::legacy()
                .with_sender(Address::with_last_byte(sender))
                .with_nonce(nonce)
                .with_gas_price(gas_price)
                .with_hash(B256::with_last_byte(hash)),
        )
    }

    #[test]
    fn deterministic_merge_releases_sender_nonce_chains() {
        let mut factory = MockTransactionFactory::default();
        let input = vec![
            tx(&mut factory, 1, 1, 30, 2),
            tx(&mut factory, 2, 0, 200, 3),
            tx(&mut factory, 1, 0, 100, 1),
            tx(&mut factory, 2, 1, 150, 4),
        ];

        let output: Vec<_> =
            sender_sharded_transactions(input, BestTransactionsAttributes::new(10, None))
                .map(|tx| (tx.sender(), tx.nonce()))
                .collect();

        assert_eq!(
            output,
            vec![
                (Address::with_last_byte(2), 0),
                (Address::with_last_byte(2), 1),
                (Address::with_last_byte(1), 0),
                (Address::with_last_byte(1), 1),
            ]
        );
    }

    #[test]
    fn equal_tip_sender_run_matches_repeated_heap_order() {
        let mut factory = MockTransactionFactory::default();
        let input = vec![
            tx(&mut factory, 2, 1, 100, 5),
            tx(&mut factory, 1, 2, 100, 3),
            tx(&mut factory, 2, 0, 100, 4),
            tx(&mut factory, 1, 0, 100, 1),
            tx(&mut factory, 1, 1, 100, 2),
        ];

        let output: Vec<_> =
            sender_sharded_transactions(input, BestTransactionsAttributes::new(10, None))
                .map(|tx| (tx.sender(), tx.nonce()))
                .collect();

        assert_eq!(
            output,
            vec![
                (Address::with_last_byte(1), 0),
                (Address::with_last_byte(1), 1),
                (Address::with_last_byte(1), 2),
                (Address::with_last_byte(2), 0),
                (Address::with_last_byte(2), 1),
            ]
        );
    }

    #[test]
    fn invalid_transaction_suppresses_only_its_sender_suffix() {
        let mut factory = MockTransactionFactory::default();
        let input = vec![
            tx(&mut factory, 1, 0, 100, 1),
            tx(&mut factory, 1, 1, 90, 2),
            tx(&mut factory, 2, 0, 200, 3),
            tx(&mut factory, 2, 1, 150, 4),
        ];
        let mut output =
            sender_sharded_transactions(input, BestTransactionsAttributes::new(10, None));

        let invalid = output.next().expect("highest-tip sender head");
        assert_eq!(
            (invalid.sender(), invalid.nonce()),
            (Address::with_last_byte(2), 0)
        );
        output.mark_invalid(
            &invalid,
            InvalidPoolTransactionError::ExceedsGasLimit(21_000, 0),
        );

        let remaining: Vec<_> = output.map(|tx| (tx.sender(), tx.nonce())).collect();
        assert_eq!(
            remaining,
            vec![
                (Address::with_last_byte(1), 0),
                (Address::with_last_byte(1), 1),
            ]
        );
    }

    #[test]
    fn below_basefee_head_drops_complete_sender_suffix() {
        let mut factory = MockTransactionFactory::default();
        let input = vec![
            tx(&mut factory, 1, 0, 9, 1),
            tx(&mut factory, 1, 1, 500, 2),
            tx(&mut factory, 2, 0, 20, 3),
        ];

        let output: Vec<_> =
            sender_sharded_transactions(input, BestTransactionsAttributes::new(10, None))
                .map(|tx| (tx.sender(), tx.nonce()))
                .collect();

        assert_eq!(output, vec![(Address::with_last_byte(2), 0)]);
    }
}

/// Outer payload builder that creates `N42InnerPayloadBuilder` instances.
#[derive(Clone, Debug)]
pub struct N42PayloadBuilder {
    consensus_state: Arc<SharedConsensusState>,
}

impl N42PayloadBuilder {
    pub fn new(consensus_state: Arc<SharedConsensusState>) -> Self {
        Self { consensus_state }
    }
}

impl<Types, Node, Pool, Evm> PayloadBuilderBuilder<Node, Pool, Evm> for N42PayloadBuilder
where
    Types: NodeTypes<ChainSpec: EthereumHardforks, Primitives = EthPrimitives>,
    Node: FullNodeTypes<Types = Types>,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TxTy<Node::Types>>>
        + Unpin
        + 'static,
    Evm: ConfigureEvm<Primitives = PrimitivesTy<Types>, NextBlockEnvCtx = NextBlockEnvAttributes>
        + 'static,
    Types::Payload:
        PayloadTypes<BuiltPayload = EthBuiltPayload, PayloadAttributes = EthPayloadAttributes>,
{
    type PayloadBuilder = N42InnerPayloadBuilder<Pool, Node::Provider, Evm>;

    async fn build_payload_builder(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
        evm_config: Evm,
    ) -> eyre::Result<Self::PayloadBuilder> {
        let conf = ctx.payload_builder_config();
        let gas_limit = conf.gas_limit_for(ctx.chain_spec().chain());

        Ok(N42InnerPayloadBuilder {
            client: ctx.provider().clone(),
            pool,
            evm_config,
            base_config: EthereumBuilderConfig::new()
                .with_gas_limit(gas_limit)
                .with_max_blobs_per_block(conf.max_blobs_per_block()),
            consensus_state: self.consensus_state,
        })
    }
}

/// Inner payload builder using the standard Ethereum payload flow.
///
/// Consensus evidence (QC + optional mobile attestation) is stored in the MDBX
/// `n42_consensus_evidence` table (indexed by block number).
/// `parent_beacon_block_root` is always B256::ZERO (Cancun placeholder).
/// `extra_data` follows standard Ethereum limits.
#[derive(Debug, Clone)]
pub struct N42InnerPayloadBuilder<Pool, Client, Evm> {
    client: Client,
    pool: Pool,
    evm_config: Evm,
    base_config: EthereumBuilderConfig,
    #[allow(dead_code)]
    consensus_state: Arc<SharedConsensusState>,
}

impl<Pool, Client, Evm> PayloadBuilder for N42InnerPayloadBuilder<Pool, Client, Evm>
where
    Evm: ConfigureEvm<Primitives = EthPrimitives, NextBlockEnvCtx = NextBlockEnvAttributes>,
    Client: StateProviderFactory + ChainSpecProvider<ChainSpec: EthereumHardforks> + Clone,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TransactionSigned>>,
{
    type Attributes = EthPayloadAttributes;
    type BuiltPayload = EthBuiltPayload;

    fn try_build(
        &self,
        args: BuildArguments<EthPayloadAttributes, EthBuiltPayload>,
    ) -> Result<BuildOutcome<EthBuiltPayload>, PayloadBuilderError> {
        // Log pool depth before building — key diagnostic for TPS bottleneck analysis.
        let pool_pending = self.pool.pool_size().pending;
        let pool_queued = self.pool.pool_size().queued;
        self.consensus_state
            .update_pool_depth(pool_pending, pool_queued);
        metrics::gauge!("n42_pool_pending_at_build").set(pool_pending as f64);
        metrics::gauge!("n42_pool_queued_at_build").set(pool_queued as f64);

        let build_start = std::time::Instant::now();
        let result = default_ethereum_payload(
            self.evm_config.clone(),
            self.client.clone(),
            self.pool.clone(),
            self.base_config.clone(),
            args,
            |attributes| {
                if sender_sharded_drain_enabled() {
                    Box::new(sender_sharded_transactions(
                        self.pool.pending_transactions(),
                        attributes,
                    )) as Box<_>
                } else {
                    self.pool.best_transactions_with_attributes(attributes)
                }
            },
        );

        let elapsed_ms = build_start.elapsed().as_millis() as u64;
        match &result {
            Ok(BuildOutcome::Better { payload, .. }) => {
                let tx_count = payload.block().body().transactions().count();
                let gas_used = payload.block().header().gas_used;
                let gas_limit = payload.block().header().gas_limit;
                let gas_pct = if gas_limit > 0 {
                    (gas_used as f64 / gas_limit as f64) * 100.0
                } else {
                    0.0
                };
                metrics::histogram!("n42_payload_build_ms").record(elapsed_ms as f64);
                metrics::gauge!("n42_payload_tx_count").set(tx_count as f64);
                metrics::gauge!("n42_payload_gas_used").set(gas_used as f64);
                if tx_count > 0 || elapsed_ms > 50 {
                    tracing::info!(
                        target: "n42::payload",
                        elapsed_ms,
                        tx_count,
                        gas_used,
                        gas_limit,
                        gas_pct = format!("{:.1}%", gas_pct),
                        pool_pending,
                        pool_queued,
                        "payload built"
                    );
                }
            }
            Ok(BuildOutcome::Aborted { .. }) => {
                tracing::debug!(target: "n42::payload", elapsed_ms, "payload build aborted (no improvement)");
            }
            Ok(BuildOutcome::Cancelled) | Ok(BuildOutcome::Freeze(_)) => {
                tracing::debug!(target: "n42::payload", elapsed_ms, "payload build cancelled/frozen");
            }
            Err(e) => {
                tracing::warn!(target: "n42::payload", elapsed_ms, error = %e, "payload build error");
            }
        }

        result
    }

    fn on_missing_payload(
        &self,
        _args: BuildArguments<Self::Attributes, Self::BuiltPayload>,
    ) -> MissingPayloadBehaviour<Self::BuiltPayload> {
        MissingPayloadBehaviour::AwaitInProgress
    }

    fn build_empty_payload(
        &self,
        config: PayloadConfig<Self::Attributes>,
    ) -> Result<EthBuiltPayload, PayloadBuilderError> {
        let args = BuildArguments::new(
            Default::default(),
            Default::default(),
            None,
            config,
            Default::default(),
            None,
        );
        default_ethereum_payload(
            self.evm_config.clone(),
            self.client.clone(),
            self.pool.clone(),
            self.base_config.clone(),
            args,
            |attributes| self.pool.best_transactions_with_attributes(attributes),
        )?
        .into_payload()
        .ok_or(PayloadBuilderError::MissingPayload)
    }
}
