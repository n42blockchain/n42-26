use reth_chainspec::EthereumHardforks;
use reth_evm::ConfigureEvm;
use reth_node_builder::{
    components::{PoolBuilder, TxPoolBuilder},
    node::{FullNodeTypes, NodeTypes},
    BuilderContext, PrimitivesTy,
};
use reth_transaction_pool::{
    blobstore::DiskFileBlobStore, CoinbaseTipOrdering, EthPooledTransaction,
    EthTransactionValidator, PoolConfig, SubPoolLimit, TransactionValidationTaskExecutor,
};

/// N42 交易池类型别名 — 使用 DiskFileBlobStore 支持 EIP-4844 blob 交易
pub type N42TransactionPool<Provider, Evm> = reth_transaction_pool::Pool<
    TransactionValidationTaskExecutor<
        EthTransactionValidator<Provider, EthPooledTransaction, Evm>,
    >,
    CoinbaseTipOrdering<EthPooledTransaction>,
    DiskFileBlobStore,
>;

/// IDC 高带宽节点交易池构建器
///
/// 与 EthereumPoolBuilder 的差异：
/// - EIP-4844 blob 交易启用（DiskFileBlobStore，生态兼容用途）
/// - IDC 优化的池容量（更大的 pending/basefee/queued 限制）
/// - 更高的单账户槽位限制
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

        // 创建验证器 — 完全复用 reth 的 EthTransactionValidator
        // EIP-4844 默认启用（不再调用 .set_eip4844(false)）
        let validator =
            TransactionValidationTaskExecutor::eth_builder(ctx.provider().clone(), evm_config)
                .with_local_transactions_config(
                    ctx.pool_config().local_transactions_config.clone(),
                )
                .set_tx_fee_cap(ctx.config().rpc.rpc_tx_fee_cap)
                .with_max_tx_input_bytes(ctx.config().txpool.max_tx_input_bytes)
                .with_max_tx_gas_limit(ctx.config().txpool.max_tx_gas_limit)
                .with_minimum_priority_fee(ctx.config().txpool.minimum_priority_fee)
                .with_additional_tasks(ctx.config().txpool.additional_validation_tasks)
                .build_with_tasks(ctx.task_executor().clone(), blob_store.clone());

        // IDC 优化的池配置
        let pool_config = idc_pool_config(&ctx.pool_config());

        // 构建池 + 启动维护任务 — 复用 reth 的 TxPoolBuilder
        let pool = TxPoolBuilder::new(ctx)
            .with_validator(validator)
            .build_and_spawn_maintenance_task(blob_store, pool_config)?;

        Ok(pool)
    }
}

/// IDC 机房优化的池配置
///
/// 高带宽、大内存场景，提升池容量上限
fn idc_pool_config(base: &PoolConfig) -> PoolConfig {
    PoolConfig {
        pending_limit: SubPoolLimit { max_txs: 50_000, max_size: 100 * 1024 * 1024 },
        basefee_limit: SubPoolLimit { max_txs: 25_000, max_size: 50 * 1024 * 1024 },
        queued_limit: SubPoolLimit { max_txs: 25_000, max_size: 50 * 1024 * 1024 },
        blob_limit: SubPoolLimit { max_txs: 256, max_size: 50 * 1024 * 1024 },
        max_account_slots: 64,
        ..base.clone()
    }
}
