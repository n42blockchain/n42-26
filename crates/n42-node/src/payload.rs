use crate::consensus_state::SharedConsensusState;
use reth_basic_payload_builder::{
    BuildArguments, BuildOutcome, MissingPayloadBehaviour, PayloadBuilder, PayloadConfig,
};
use reth_chainspec::{ChainSpecProvider, EthChainSpec, EthereumHardforks};
use reth_ethereum_engine_primitives::{
    EthBuiltPayload, EthPayloadAttributes, EthPayloadBuilderAttributes,
};
use reth_ethereum_payload_builder::{default_ethereum_payload, EthereumBuilderConfig};
use std::time::Duration;
use reth_ethereum_primitives::{EthPrimitives, TransactionSigned};
use reth_evm::{ConfigureEvm, NextBlockEnvAttributes};
use reth_node_api::{FullNodeTypes, NodeTypes, PrimitivesTy, TxTy};
use reth_node_builder::{
    components::PayloadBuilderBuilder, BuilderContext, PayloadBuilderConfig, PayloadTypes,
};
use reth_payload_builder_primitives::PayloadBuilderError;
use reth_storage_api::StateProviderFactory;
use reth_transaction_pool::{PoolTransaction, TransactionPool};
use std::sync::Arc;

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
    Evm: ConfigureEvm<
            Primitives = PrimitivesTy<Types>,
            NextBlockEnvCtx = NextBlockEnvAttributes,
        > + 'static,
    Types::Payload: PayloadTypes<
        BuiltPayload = EthBuiltPayload,
        PayloadAttributes = EthPayloadAttributes,
        PayloadBuilderAttributes = EthPayloadBuilderAttributes,
    >,
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

        // Build time budget: leave headroom within the slot for consensus + import.
        // Default: 3 seconds (fits comfortably in a 4-second slot).
        let build_budget = Duration::from_millis(
            std::env::var("N42_BUILD_TIME_BUDGET_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(3000),
        );

        Ok(N42InnerPayloadBuilder {
            client: ctx.provider().clone(),
            pool,
            evm_config,
            base_config: EthereumBuilderConfig::new()
                .with_gas_limit(gas_limit)
                .with_max_blobs_per_block(conf.max_blobs_per_block())
                .with_build_time_budget(build_budget),
            consensus_state: self.consensus_state,
        })
    }
}

/// Inner payload builder using the standard Ethereum payload flow.
///
/// Note: QC data is NOT injected into extra_data because the Engine API enforces
/// a 32-byte limit incompatible with N42's QC encoding (~200 bytes). The QC is
/// stored separately in `SharedConsensusState`.
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
    type Attributes = EthPayloadBuilderAttributes;
    type BuiltPayload = EthBuiltPayload;

    fn try_build(
        &self,
        args: BuildArguments<EthPayloadBuilderAttributes, EthBuiltPayload>,
    ) -> Result<BuildOutcome<EthBuiltPayload>, PayloadBuilderError> {
        let build_start = std::time::Instant::now();
        let result = default_ethereum_payload(
            self.evm_config.clone(),
            self.client.clone(),
            self.pool.clone(),
            self.base_config.clone(),
            args,
            |attributes| self.pool.best_transactions_with_attributes(attributes),
        );

        let elapsed_ms = build_start.elapsed().as_millis() as u64;
        match &result {
            Ok(BuildOutcome::Better { payload, .. }) => {
                let tx_count = payload.block().body().transactions().count();
                let gas_used = payload.block().header().gas_used;
                metrics::histogram!("n42_payload_build_ms").record(elapsed_ms as f64);
                metrics::gauge!("n42_payload_tx_count").set(tx_count as f64);
                metrics::gauge!("n42_payload_gas_used").set(gas_used as f64);
                if tx_count > 0 || elapsed_ms > 50 {
                    tracing::info!(
                        target: "n42::payload",
                        elapsed_ms,
                        tx_count,
                        gas_used,
                        gas_limit = payload.block().header().gas_limit,
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
        let args = BuildArguments::new(Default::default(), config, Default::default(), None);
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
