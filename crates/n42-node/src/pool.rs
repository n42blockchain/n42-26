use reth_chainspec::EthereumHardforks;
use reth_evm::ConfigureEvm;
use reth_node_builder::{
    components::{PoolBuilder, TxPoolBuilder},
    node::{FullNodeTypes, NodeTypes},
    BuilderContext, PrimitivesTy,
};
use reth_tasks::{RuntimeBuilder, RuntimeConfig, TokioConfig};
use reth_transaction_pool::{
    blobstore::DiskFileBlobStore, CoinbaseTipOrdering, EthPooledTransaction,
    EthTransactionValidator, PoolConfig, SubPoolLimit, TransactionValidationTaskExecutor,
};
use tracing::info;

/// N42 transaction pool type — uses DiskFileBlobStore for EIP-4844 blob transaction support.
pub type N42TransactionPool<Provider, Evm> = reth_transaction_pool::Pool<
    TransactionValidationTaskExecutor<
        EthTransactionValidator<Provider, EthPooledTransaction, Evm>,
    >,
    CoinbaseTipOrdering<EthPooledTransaction>,
    DiskFileBlobStore,
>;

/// IDC high-bandwidth transaction pool builder.
///
/// Differences from EthereumPoolBuilder:
/// - EIP-4844 blob transactions enabled (DiskFileBlobStore)
/// - Larger pending/basefee/queued limits for IDC throughput
/// - Higher per-account slot limit
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct N42PoolBuilder;

impl<Types, Node, Evm> PoolBuilder<Node, Evm> for N42PoolBuilder
where
    Types: NodeTypes<
        ChainSpec: EthereumHardforks,
        Primitives: reth_primitives_traits::NodePrimitives<
            SignedTx = reth_ethereum_primitives::TransactionSigned,
        >,
    >,
    Node: FullNodeTypes<Types = Types>,
    Evm: ConfigureEvm<Primitives = PrimitivesTy<Types>> + Clone + 'static,
{
    type Pool = N42TransactionPool<Node::Provider, Evm>;

    async fn build_pool(
        self,
        ctx: &BuilderContext<Node>,
        evm_config: Evm,
    ) -> eyre::Result<Self::Pool> {
        let blob_store = reth_node_builder::components::create_blob_store(ctx)?;

        // Dedicated runtime for TX pool validation tasks.
        // Isolates ECDSA recovery + DB read CPU from consensus/GossipSub,
        // preventing R1_collect latency spikes under high TX load.
        let validation_threads: usize = std::env::var("N42_POOL_VALIDATION_THREADS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(4);

        // Leak the runtime so it lives as long as the process — avoids
        // "Cannot drop a runtime in a context where blocking is not allowed"
        // since build_pool() is async and Runtime::drop needs blocking.
        let pool_runtime: &'static _ = Box::leak(Box::new(
            RuntimeBuilder::new(
                RuntimeConfig::default().with_tokio(TokioConfig::Owned {
                    worker_threads: Some(validation_threads),
                    thread_keep_alive: std::time::Duration::from_secs(60),
                    thread_name: "pool-val",
                }),
            )
            .build()?,
        ));

        info!(
            target: "n42::pool",
            threads = validation_threads,
            "TX pool validation running on dedicated runtime"
        );

        let additional_tasks = ctx.config().txpool.additional_validation_tasks.max(16);
        let validator =
            TransactionValidationTaskExecutor::eth_builder(ctx.provider().clone(), evm_config)
                .with_local_transactions_config(
                    ctx.pool_config().local_transactions_config.clone(),
                )
                .set_tx_fee_cap(ctx.config().rpc.rpc_tx_fee_cap)
                .with_max_tx_input_bytes(ctx.config().txpool.max_tx_input_bytes)
                .with_max_tx_gas_limit(ctx.config().txpool.max_tx_gas_limit)
                .with_minimum_priority_fee(ctx.config().txpool.minimum_priority_fee)
                .with_additional_tasks(additional_tasks)
                .build_with_tasks(pool_runtime.clone(), blob_store.clone());

        let pool_config = idc_pool_config(&ctx.pool_config());

        let pool = TxPoolBuilder::new(ctx)
            .with_validator(validator)
            .build_and_spawn_maintenance_task(blob_store, pool_config)?;

        Ok(pool)
    }
}

/// IDC-optimized pool configuration: higher limits for high-bandwidth, large-memory nodes.
///
/// With 2G gas limit and 2s slot, each block can hold ~95k txs.
/// Pool must buffer several blocks worth of txs during burst injection.
fn idc_pool_config(base: &PoolConfig) -> PoolConfig {
    PoolConfig {
        pending_limit: SubPoolLimit { max_txs: 200_000, max_size: 400 * 1024 * 1024 },
        basefee_limit: SubPoolLimit { max_txs: 100_000, max_size: 200 * 1024 * 1024 },
        queued_limit: SubPoolLimit { max_txs: 100_000, max_size: 200 * 1024 * 1024 },
        blob_limit: SubPoolLimit { max_txs: 256, max_size: 50 * 1024 * 1024 },
        max_account_slots: 16_384,
        ..base.clone()
    }
}
