use crate::consensus_state::SharedConsensusState;
use crate::engine_types::N42BuiltPayload;
use crate::native_payload::default_n42_payload;
use alloy_rpc_types_engine::PayloadAttributes as EthPayloadAttributes;
use n42_execution::ExecutionPath;
use n42_tx_types::N42Primitives;
use rayon::prelude::*;
use reth_basic_payload_builder::{
    BuildArguments, BuildOutcome, MissingPayloadBehaviour, PayloadBuilder, PayloadConfig,
};
use reth_chainspec::{ChainSpecProvider, EthChainSpec, EthereumHardforks};
use reth_ethereum_payload_builder::EthereumBuilderConfig;
use reth_node_api::{FullNodeTypes, NodeTypes};
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
    eof_guard: bool,
}

impl N42PayloadBuilder {
    pub fn new(consensus_state: Arc<SharedConsensusState>) -> Self {
        Self {
            consensus_state,
            eof_guard: false,
        }
    }

    /// Never build a block that carries EOF code (gov5 profile; see [`crate::eof_guard`]).
    pub const fn with_eof_guard(mut self, enabled: bool) -> Self {
        self.eof_guard = enabled;
        self
    }
}

impl<Types, Node, Pool> PayloadBuilderBuilder<Node, Pool, n42_execution::N42EvmConfig>
    for N42PayloadBuilder
where
    Types: NodeTypes<ChainSpec: EthereumHardforks, Primitives = N42Primitives>,
    Node: FullNodeTypes<Types = Types>,
    Pool: TransactionPool<Transaction = crate::native_pool::N42PooledTransaction> + Unpin + 'static,
    Types::Payload:
        PayloadTypes<BuiltPayload = N42BuiltPayload, PayloadAttributes = EthPayloadAttributes>,
{
    type PayloadBuilder = N42InnerPayloadBuilder<Pool, Node::Provider, n42_execution::N42EvmConfig>;

    async fn build_payload_builder(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
        evm_config: n42_execution::N42EvmConfig,
    ) -> eyre::Result<Self::PayloadBuilder> {
        let conf = ctx.payload_builder_config();
        let gas_limit = conf.gas_limit_for(ctx.chain_spec().chain());

        let builder = N42InnerPayloadBuilder {
            client: ctx.provider().clone(),
            pool,
            evm_config,
            base_config: EthereumBuilderConfig::new()
                .with_gas_limit(gas_limit)
                .with_max_blobs_per_block(conf.max_blobs_per_block())
                .with_skip_state_root(builder_state_root_replaced()),
            consensus_state: self.consensus_state,
            eof_guard: self.eof_guard,
            parallel_transfers: crate::parallel_payload::enabled(),
        };
        // Only the parallel builder reads its parent's state through the
        // sealed-block overlay.
        if crate::build_on_output::enabled() && builder.parallel_transfers {
            let direct = builder.clone();
            crate::build_on_output::register_builder(Box::new(move |parent, attributes| {
                direct
                    .build_on(parent, attributes)
                    .map_err(|error| error.to_string())
            }));
        }
        Ok(builder)
    }
}

static BUILDER_STATE_ROOT_REPLACED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Declares that every payload this process builds has its state root
/// replaced before it is proposed (H2-v4 participant mode: the consensus
/// bridge normalizes the block to the Gov5 header profile and writes the QMDB
/// root). The builder then skips its own MPT state root, which would be
/// computed only to be discarded. Must be called before the node launches.
pub fn set_builder_state_root_replaced(replaced: bool) {
    BUILDER_STATE_ROOT_REPLACED.store(replaced, std::sync::atomic::Ordering::Relaxed);
}

static BUILDER_QMDB_STORE: std::sync::OnceLock<
    std::sync::Arc<crate::qmdb_state_root::Gov5QmdbStateRootStore>,
> = std::sync::OnceLock::new();

/// Hands the builder the QMDB state-root store, so that it derives the root
/// that replaces its own while the block is being assembled instead of
/// leaving it to the normalization step afterwards. Without it, or with
/// `N42_BUILDER_QMDB_ROOT=0`, normalization derives the root as before.
pub fn set_builder_qmdb_store(
    store: std::sync::Arc<crate::qmdb_state_root::Gov5QmdbStateRootStore>,
) {
    let _ = BUILDER_QMDB_STORE.set(store);
}

/// The store the builder derives the replacing root from, when it does.
pub(crate) fn builder_qmdb_store()
-> Option<&'static std::sync::Arc<crate::qmdb_state_root::Gov5QmdbStateRootStore>> {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let enabled = *ENABLED
        .get_or_init(|| !std::env::var("N42_BUILDER_QMDB_ROOT").is_ok_and(|value| value == "0"));
    if !enabled || !builder_state_root_replaced() {
        return None;
    }
    BUILDER_QMDB_STORE.get()
}

/// Whether built payloads have their state root replaced before proposal.
pub fn builder_state_root_replaced() -> bool {
    BUILDER_STATE_ROOT_REPLACED.load(std::sync::atomic::Ordering::Relaxed)
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
    /// Skip EOF initcode transactions and refuse a built block that carries one.
    eof_guard: bool,
    parallel_transfers: bool,
}

impl<Pool, Client> N42InnerPayloadBuilder<Pool, Client, n42_execution::N42EvmConfig> {
    /// Construct the same builder used by the node with explicit execution settings.
    pub fn new(
        client: Client,
        pool: Pool,
        evm_config: n42_execution::N42EvmConfig,
        base_config: EthereumBuilderConfig,
        consensus_state: Arc<SharedConsensusState>,
    ) -> Self {
        Self {
            client,
            pool,
            evm_config,
            base_config,
            consensus_state,
            eof_guard: false,
            parallel_transfers: crate::parallel_payload::enabled(),
        }
    }

    /// Select the parallel prefix without changing process-global environment.
    pub const fn with_parallel_transfers(mut self, enabled: bool) -> Self {
        self.parallel_transfers = enabled;
        self
    }
}

impl<Pool, Client> N42InnerPayloadBuilder<Pool, Client, n42_execution::N42EvmConfig>
where
    Client: StateProviderFactory + ChainSpecProvider<ChainSpec: EthereumHardforks> + Clone,
    Pool: TransactionPool<Transaction = crate::native_pool::N42PooledTransaction> + 'static,
{
    /// Builds one block on `parent` outside a payload job: the job generator
    /// needs the parent in the provider, and an own block that is only sealed
    /// is not (`N42_BUILD_ON_OUTPUT=1`). Everything else is the job's build.
    pub fn build_on(
        &self,
        parent: reth_primitives_traits::SealedHeader,
        attributes: EthPayloadAttributes,
    ) -> Result<N42BuiltPayload, PayloadBuilderError> {
        let id = {
            let mut seed = [0u8; 40];
            seed[..32].copy_from_slice(parent.hash().as_slice());
            seed[32..].copy_from_slice(&attributes.timestamp.to_be_bytes());
            let digest = alloy_primitives::keccak256(seed);
            alloy_rpc_types_engine::PayloadId::new(digest[..8].try_into().expect("8 bytes"))
        };
        let args = BuildArguments::new(
            Default::default(),
            None,
            None,
            PayloadConfig::new(Arc::new(parent), attributes, id),
            Default::default(),
            None,
        );
        match self.try_build(args)? {
            BuildOutcome::Better { payload, .. } | BuildOutcome::Freeze(payload) => Ok(payload),
            BuildOutcome::Aborted { .. } => Err(PayloadBuilderError::other(
                std::io::Error::other("build on a sealed block aborted"),
            )),
            BuildOutcome::Cancelled => Err(PayloadBuilderError::other(std::io::Error::other(
                "build on a sealed block cancelled",
            ))),
        }
    }
}

/// `N42_IDLE_BLOCK_WAIT_MS=<ms>`: a build that finds no transaction waiting
/// waits up to that long for one before it builds an empty block. An idle
/// chain then makes at most about one block per wait instead of one per
/// consensus round, which keeps the engine's unpersisted blocks from piling up
/// behind a persistence backlog. A transaction that arrives ends the wait.
/// Unset or 0: no wait.
fn idle_block_wait() -> Option<std::time::Duration> {
    static WAIT: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    let ms = *WAIT.get_or_init(|| {
        std::env::var("N42_IDLE_BLOCK_WAIT_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0)
            .min(2_000)
    });
    (ms > 0).then(|| std::time::Duration::from_millis(ms))
}

/// Waits, when [`idle_block_wait`] is set and `pending` counts no
/// transaction, until one is pending or the wait is over. Returns how long
/// it waited, if it did.
fn idle_wait_for_transactions(pending: impl Fn() -> usize) -> Option<std::time::Duration> {
    idle_wait_up_to(idle_block_wait()?, pending)
}

fn idle_wait_up_to(
    wait: std::time::Duration,
    pending: impl Fn() -> usize,
) -> Option<std::time::Duration> {
    if pending() > 0 {
        return None;
    }
    let started = std::time::Instant::now();
    while started.elapsed() < wait && pending() == 0 {
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    let waited = started.elapsed();
    metrics::histogram!("n42_idle_block_wait_ms").record(waited.as_secs_f64() * 1_000.0);
    Some(waited)
}

impl<Pool, Client> PayloadBuilder
    for N42InnerPayloadBuilder<Pool, Client, n42_execution::N42EvmConfig>
where
    Client: StateProviderFactory + ChainSpecProvider<ChainSpec: EthereumHardforks> + Clone,
    Pool: TransactionPool<Transaction = crate::native_pool::N42PooledTransaction> + 'static,
{
    type Attributes = EthPayloadAttributes;
    type BuiltPayload = N42BuiltPayload;

    fn try_build(
        &self,
        args: BuildArguments<EthPayloadAttributes, N42BuiltPayload>,
    ) -> Result<BuildOutcome<N42BuiltPayload>, PayloadBuilderError> {
        // Log pool depth before building — key diagnostic for TPS bottleneck analysis.
        let frame_queue =
            crate::frame_queue::frame_queue_enabled().then(crate::frame_queue::FrameQueue::global);
        let mut parent_wait_ms = 0.0;
        if let Some(queue) = frame_queue {
            // The parent's transactions leave the queue when its canonical
            // notification is applied; offering them again would only have
            // the builder skip them one by one.
            let parent = args.config.parent_header.number;
            let wait = std::time::Instant::now();
            if !queue.wait_applied(parent, std::time::Duration::from_millis(500)) {
                tracing::warn!(
                    target: "n42::payload",
                    parent,
                    "frame queue has not applied the parent block; building anyway"
                );
            }
            parent_wait_ms = wait.elapsed().as_secs_f64() * 1_000.0;
            metrics::histogram!("n42_frame_queue_parent_wait_ms").record(parent_wait_ms);
        }
        // Behind a persistence backlog the build waits first, so the
        // transactions it then takes are the ones there after the wait.
        let backlog_wait_ms = crate::memory_watch::wait_for_persistence()
            .map_or(0.0, |waited| waited.as_secs_f64() * 1_000.0);
        let pending =
            || self.pool.pool_size().pending + frame_queue.map_or(0, |queue| queue.pending());
        let idle_wait_ms = idle_wait_for_transactions(pending)
            .map_or(0.0, |waited| waited.as_secs_f64() * 1_000.0);
        let pool_size = self.pool.pool_size();
        let pool_pending = pool_size.pending + frame_queue.map_or(0, |queue| queue.pending());
        let pool_queued = pool_size.queued;
        self.consensus_state
            .update_pool_depth(pool_pending, pool_queued);
        metrics::gauge!("n42_pool_pending_at_build").set(pool_pending as f64);
        metrics::gauge!("n42_pool_queued_at_build").set(pool_queued as f64);

        let build_start = std::time::Instant::now();
        // Marks when the builder is entered; the distance from reth's
        // "New payload job created" line is time spent before any packing.
        tracing::info!(
            target: "n42::payload",
            id = %args.config.payload_id(),
            parent = %args.config.parent_header.hash(),
            pool_pending,
            frame_queue_parent_wait_ms = parent_wait_ms,
            idle_wait_ms,
            backlog_wait_ms,
            "N42_BUILD_ENTRY: payload builder entered"
        );
        let result = crate::parallel_payload::build_with_parallel(
            self.evm_config.clone(),
            self.client.clone(),
            self.pool.clone(),
            self.base_config.clone(),
            args,
            |attributes| {
                let pool = self.pool.clone();
                let ordinary = move || {
                    if sender_sharded_drain_enabled() {
                        Box::new(sender_sharded_transactions(
                            pool.pending_transactions(),
                            attributes,
                        )) as Box<_>
                    } else {
                        pool.best_transactions_with_attributes(attributes)
                    }
                };
                let best = if let Some(queue) = frame_queue {
                    // Queued frames first; the pool holds only what did not
                    // come in as a native frame.
                    Box::new(crate::frame_queue::QueuedFrameTransactions::new(
                        queue, ordinary,
                    )) as Box<_>
                } else if crate::frame_order::frame_order_build_enabled() {
                    // Frames first, in arrival order; the pool-wide order only
                    // once they are used up.
                    let lookup_pool = self.pool.clone();
                    Box::new(crate::frame_order::FrameOrderedTransactions::new(
                        n42_tx_types::FrameStore::global(),
                        move |hashes| lookup_pool.get_all(hashes),
                        ordinary,
                    )) as Box<_>
                } else {
                    ordinary()
                };
                if self.eof_guard {
                    Box::new(crate::eof_guard::EofFilteringTransactions::new(best)) as Box<_>
                } else {
                    best
                }
            },
            self.parallel_transfers,
        );
        let result = match result {
            Ok(BuildOutcome::Better {
                payload,
                cached_reads,
            }) if self.eof_guard => {
                match crate::eof_guard::check_built_block(payload.recovered_block()) {
                    Ok(()) => Ok(BuildOutcome::Better {
                        payload,
                        cached_reads,
                    }),
                    Err(error) => Err(PayloadBuilderError::Other(Box::new(error))),
                }
            }
            other => other,
        };

        let elapsed_ms = build_start.elapsed().as_millis() as u64;
        let path = if self.parallel_transfers {
            "live_parallel_transfer_builder"
        } else {
            ExecutionPath::LIVE_SEQUENTIAL.label()
        };
        let outcome = match &result {
            Ok(BuildOutcome::Better { .. }) => "better",
            Ok(BuildOutcome::Aborted { .. }) => "aborted",
            Ok(BuildOutcome::Cancelled) => "cancelled",
            Ok(BuildOutcome::Freeze(_)) => "freeze",
            Err(_) => "error",
        };
        metrics::histogram!(
            "n42_evm_path_duration_ms",
            "path" => path,
            "phase" => "payload_build",
        )
        .record(build_start.elapsed().as_secs_f64() * 1_000.0);
        metrics::counter!(
            "n42_evm_path_calls_total",
            "path" => path,
            "phase" => "payload_build",
            "outcome" => outcome,
        )
        .increment(1);
        match &result {
            Ok(BuildOutcome::Better { payload, .. }) => {
                if let Some(queue) = frame_queue {
                    // Take this block's transactions out of the queue now, off
                    // the build thread, instead of when its canonical
                    // notification arrives: the next build on top of it then
                    // does not wait for that notification.
                    let number = payload.block().header().number;
                    let hashes: Vec<_> = payload
                        .block()
                        .body()
                        .transactions()
                        .map(|tx| *tx.hash())
                        .collect();
                    // On a thread of its own: on the global pool it queued
                    // behind the block's root work while the child build
                    // waited for it (`wait_applied`).
                    let mark = move || {
                        queue.mark_mined(n42_tx_types::FrameStore::global(), number, &hashes);
                    };
                    let mark = std::sync::Arc::new(std::sync::Mutex::new(Some(mark)));
                    let spawned = {
                        let mark = mark.clone();
                        std::thread::Builder::new().name("frame-mined".into()).spawn(move || {
                            if let Some(mark) = mark.lock().ok().and_then(|mut m| m.take()) {
                                mark();
                            }
                        })
                    };
                    if spawned.is_err()
                        && let Some(mark) = mark.lock().ok().and_then(|mut m| m.take())
                    {
                        rayon::spawn(mark);
                    }
                }
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
    ) -> Result<N42BuiltPayload, PayloadBuilderError> {
        let args = BuildArguments::new(
            Default::default(),
            Default::default(),
            None,
            config,
            Default::default(),
            None,
        );
        default_n42_payload(
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

#[cfg(test)]
mod idle_wait_tests {
    use super::idle_wait_up_to;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    #[test]
    fn an_idle_build_waits_until_a_transaction_or_the_end_of_the_wait() {
        // Work waiting: no wait at all.
        assert_eq!(idle_wait_up_to(Duration::from_secs(5), || 3), None);
        // Nothing arrives: the whole wait.
        let waited = idle_wait_up_to(Duration::from_millis(30), || 0).expect("waited");
        assert!(waited >= Duration::from_millis(30));
        // A transaction arriving ends the wait.
        let pending = AtomicUsize::new(0);
        let started = Instant::now();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(20));
                pending.store(1, Ordering::Relaxed);
            });
            let waited =
                idle_wait_up_to(Duration::from_secs(5), || pending.load(Ordering::Relaxed))
                    .expect("waited");
            assert!(waited < Duration::from_secs(1));
        });
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
