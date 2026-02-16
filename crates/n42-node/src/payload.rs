use crate::consensus_state::SharedConsensusState;
use n42_consensus::encode_qc_to_extra_data;
use reth_basic_payload_builder::{
    BuildArguments, BuildOutcome, MissingPayloadBehaviour, PayloadBuilder, PayloadConfig,
};
use reth_chainspec::{ChainSpecProvider, EthChainSpec, EthereumHardforks};
use reth_ethereum_engine_primitives::{
    EthBuiltPayload, EthPayloadAttributes, EthPayloadBuilderAttributes,
};
use reth_ethereum_payload_builder::{default_ethereum_payload, EthereumBuilderConfig};
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
use tracing::warn;

/// N42 payload builder that injects the latest committed QC into block `extra_data`.
///
/// This is the outer builder (implements `PayloadBuilderBuilder`) that creates
/// `N42InnerPayloadBuilder` instances for the `BasicPayloadServiceBuilder`.
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
        let chain = ctx.chain_spec().chain();
        let gas_limit = conf.gas_limit_for(chain);

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

/// Inner payload builder that dynamically reads QC from shared consensus state
/// and injects it into `extra_data` on every `try_build()` invocation.
#[derive(Debug, Clone)]
pub struct N42InnerPayloadBuilder<Pool, Client, Evm> {
    client: Client,
    pool: Pool,
    evm_config: Evm,
    base_config: EthereumBuilderConfig,
    consensus_state: Arc<SharedConsensusState>,
}

impl<Pool, Client, Evm> N42InnerPayloadBuilder<Pool, Client, Evm> {
    /// Builds the `EthereumBuilderConfig` with the latest QC encoded as `extra_data`.
    fn config_with_qc(&self) -> EthereumBuilderConfig {
        let extra_data = match self.consensus_state.load_committed_qc().as_ref() {
            Some(qc) => match encode_qc_to_extra_data(qc) {
                Ok(data) => data,
                Err(e) => {
                    warn!(error = %e, "failed to encode QC for extra_data, using empty");
                    Default::default()
                }
            },
            None => Default::default(),
        };

        self.base_config.clone().with_extra_data(extra_data)
    }
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
        let config = self.config_with_qc();
        default_ethereum_payload(
            self.evm_config.clone(),
            self.client.clone(),
            self.pool.clone(),
            config,
            args,
            |attributes| self.pool.best_transactions_with_attributes(attributes),
        )
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
        let builder_config = self.config_with_qc();
        let args = BuildArguments::new(Default::default(), config, Default::default(), None);

        default_ethereum_payload(
            self.evm_config.clone(),
            self.client.clone(),
            self.pool.clone(),
            builder_config,
            args,
            |attributes| self.pool.best_transactions_with_attributes(attributes),
        )?
        .into_payload()
        .ok_or(PayloadBuilderError::MissingPayload)
    }
}
