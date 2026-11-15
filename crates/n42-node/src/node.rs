use crate::components::{N42ConsensusBuilder, N42ExecutorBuilder};
use crate::consensus_state::SharedConsensusState;
use crate::engine_types::N42EngineTypes;
use crate::engine_validator::N42EngineValidatorBuilder;
use crate::native_rpc::N42EthApiBuilder;
use crate::payload::N42PayloadBuilder;
use crate::pool::N42PoolBuilder;
use crate::qmdb_state_root::{Gov5QmdbStateRootStore, N42EngineTreeValidatorBuilder};
use crate::rpc_compat::Gov5RpcCompatLayer;
use n42_consensus::{N42HeaderProfile, ValidatorSetResolver};
use n42_tx_types::{N42Primitives, N42TxEnvelope};
use reth_chainspec::{ChainSpec, EthChainSpec, EthereumHardforks, Hardforks};
use reth_evm::{ConfigureEvm, EvmFactory, EvmFactoryFor, NextBlockEnvAttributes};
use reth_node_api::{FullNodeComponents, NodeAddOns};
use reth_node_builder::{
    Node, NodeAdapter,
    components::{BasicPayloadServiceBuilder, ComponentsBuilder},
    node::{FullNodeTypes, NodeTypes},
    rpc::{
        BasicEngineApiBuilder, EngineApiBuilder, EngineValidatorAddOn, EngineValidatorBuilder,
        EthApiBuilder, Identity, PayloadValidatorBuilder, RethRpcAddOns, RethRpcMiddleware,
        RpcAddOns, RpcHandle, RpcHooks,
    },
};
use reth_node_ethereum::node::EthereumNetworkBuilder;
use reth_provider::EthStorage;
use reth_rpc_eth_types::{EthApiError, error::FromEvmError};
use revm::context::TxEnv;
use std::sync::Arc;

/// N42 node type configuration.
///
/// Holds shared consensus state injected into the PayloadBuilder and available
/// to the `on_node_started` hook for the Orchestrator.
#[derive(Clone)]
pub struct N42Node {
    pub consensus_state: Arc<SharedConsensusState>,
    pub validator_set_resolver: Option<ValidatorSetResolver>,
    pub header_profile: N42HeaderProfile,
    pub qmdb_state_root_store: Option<Arc<Gov5QmdbStateRootStore>>,
    /// Accept proposers' state roots without a local QMDB forest.
    pub trusted_state_root: bool,
}

impl std::fmt::Debug for N42Node {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("N42Node")
            .field(
                "has_validator_set_resolver",
                &self.validator_set_resolver.is_some(),
            )
            .field("header_profile", &self.header_profile)
            .field(
                "has_qmdb_state_root_store",
                &self.qmdb_state_root_store.is_some(),
            )
            .field("trusted_state_root", &self.trusted_state_root)
            .finish()
    }
}

impl N42Node {
    pub fn new(consensus_state: Arc<SharedConsensusState>) -> Self {
        Self {
            consensus_state,
            validator_set_resolver: None,
            header_profile: N42HeaderProfile::Ethereum,
            qmdb_state_root_store: None,
            trusted_state_root: false,
        }
    }

    pub fn with_validator_set_resolver(
        mut self,
        validator_set_resolver: ValidatorSetResolver,
    ) -> Self {
        self.validator_set_resolver = Some(validator_set_resolver);
        self
    }

    pub const fn with_header_profile(mut self, header_profile: N42HeaderProfile) -> Self {
        self.header_profile = header_profile;
        self
    }

    /// Install an authenticated, bounded QMDB execution base. Startup wiring must keep this
    /// observer-only until continuous cross-client execution gates pass.
    pub fn with_gov5_qmdb_state_root_store(mut self, store: Arc<Gov5QmdbStateRootStore>) -> Self {
        self.qmdb_state_root_store = Some(store);
        self
    }

    /// Take proposers' state roots on trust (no local QMDB forest).
    pub fn with_gov5_trusted_state_root(mut self) -> Self {
        self.trusted_state_root = true;
        self
    }
}

impl NodeTypes for N42Node {
    type Primitives = N42Primitives;
    type ChainSpec = ChainSpec;
    type Storage = EthStorage<N42TxEnvelope>;
    type Payload = N42EngineTypes;
}

/// RPC add-ons over [`N42Primitives`].
///
/// reth's `EthereumAddOns` is bound to Ethereum's primitives. This is the same
/// wrapper around [`RpcAddOns`] for N42's, without the Ethereum-only extra
/// modules (flashbots block validation, `eth_config`, the testing builder and
/// the Engine SSZ proxy), which this node never enabled.
#[derive(Debug)]
pub struct N42AddOns<
    N: FullNodeComponents,
    EthB: EthApiBuilder<N>,
    PVB,
    EB = BasicEngineApiBuilder<PVB>,
    EVB = reth_node_builder::rpc::BasicEngineValidatorBuilder<PVB>,
    RpcMiddleware = Identity,
> {
    inner: RpcAddOns<N, EthB, PVB, EB, EVB, RpcMiddleware>,
}

impl<N, EthB, PVB, EB, EVB, RpcMiddleware> N42AddOns<N, EthB, PVB, EB, EVB, RpcMiddleware>
where
    N: FullNodeComponents,
    EthB: EthApiBuilder<N>,
{
    /// Creates the add-ons from the inner `RpcAddOns`.
    pub const fn new(inner: RpcAddOns<N, EthB, PVB, EB, EVB, RpcMiddleware>) -> Self {
        Self { inner }
    }
}

impl<N, EthB, PVB, EB, EVB, RpcMiddleware> NodeAddOns<N>
    for N42AddOns<N, EthB, PVB, EB, EVB, RpcMiddleware>
where
    N: FullNodeComponents<
            Types: NodeTypes<
                ChainSpec: EthChainSpec + Hardforks + EthereumHardforks,
                Primitives = N42Primitives,
                Payload: reth_engine_primitives::EngineTypes<
                    ExecutionData = alloy_rpc_types_engine::ExecutionData,
                >,
            >,
            Evm: ConfigureEvm<NextBlockEnvCtx = NextBlockEnvAttributes>,
        >,
    EthB: EthApiBuilder<N>,
    PVB: PayloadValidatorBuilder<N>,
    EB: EngineApiBuilder<N>,
    EVB: EngineValidatorBuilder<N>,
    EthApiError: FromEvmError<N::Evm>,
    EvmFactoryFor<N::Evm>: EvmFactory<Tx = TxEnv>,
    RpcMiddleware: RethRpcMiddleware,
{
    type Handle = RpcHandle<N, EthB::EthApi>;

    async fn launch_add_ons(
        self,
        ctx: reth_node_api::AddOnsContext<'_, N>,
    ) -> eyre::Result<Self::Handle> {
        self.inner.launch_add_ons(ctx).await
    }
}

impl<N, EthB, PVB, EB, EVB, RpcMiddleware> RethRpcAddOns<N>
    for N42AddOns<N, EthB, PVB, EB, EVB, RpcMiddleware>
where
    N: FullNodeComponents<
            Types: NodeTypes<
                ChainSpec: EthChainSpec + Hardforks + EthereumHardforks,
                Primitives = N42Primitives,
                Payload: reth_engine_primitives::EngineTypes<
                    ExecutionData = alloy_rpc_types_engine::ExecutionData,
                >,
            >,
            Evm: ConfigureEvm<NextBlockEnvCtx = NextBlockEnvAttributes>,
        >,
    EthB: EthApiBuilder<N>,
    PVB: PayloadValidatorBuilder<N>,
    EB: EngineApiBuilder<N>,
    EVB: EngineValidatorBuilder<N>,
    EthApiError: FromEvmError<N::Evm>,
    EvmFactoryFor<N::Evm>: EvmFactory<Tx = TxEnv>,
    RpcMiddleware: RethRpcMiddleware,
{
    type EthApi = EthB::EthApi;

    fn hooks_mut(&mut self) -> &mut RpcHooks<N, Self::EthApi> {
        self.inner.hooks_mut()
    }
}

impl<N, EthB, PVB, EB, EVB, RpcMiddleware> EngineValidatorAddOn<N>
    for N42AddOns<N, EthB, PVB, EB, EVB, RpcMiddleware>
where
    N: FullNodeComponents,
    EthB: EthApiBuilder<N>,
    PVB: Send,
    EB: EngineApiBuilder<N>,
    EVB: EngineValidatorBuilder<N>,
    RpcMiddleware: Send,
{
    type ValidatorBuilder = EVB;

    fn engine_validator_builder(&self) -> Self::ValidatorBuilder {
        self.inner.engine_validator_builder()
    }
}

impl<N> Node<N> for N42Node
where
    N: FullNodeTypes<Types = Self>,
{
    type ComponentsBuilder = ComponentsBuilder<
        N,
        N42PoolBuilder,
        BasicPayloadServiceBuilder<N42PayloadBuilder>,
        EthereumNetworkBuilder,
        N42ExecutorBuilder,
        N42ConsensusBuilder,
    >;

    type AddOns = N42AddOns<
        NodeAdapter<N>,
        N42EthApiBuilder,
        N42EngineValidatorBuilder,
        BasicEngineApiBuilder<N42EngineValidatorBuilder>,
        N42EngineTreeValidatorBuilder,
        Gov5RpcCompatLayer,
    >;

    fn components_builder(&self) -> Self::ComponentsBuilder {
        ComponentsBuilder::default()
            .node_types::<N>()
            .pool(N42PoolBuilder::default())
            .executor(N42ExecutorBuilder::default())
            .payload(
                BasicPayloadServiceBuilder::new(
                    N42PayloadBuilder::new(self.consensus_state.clone())
                        .with_eof_guard(self.header_profile == N42HeaderProfile::Gov5H2),
                )
                // reth copies every account the parent block changed into a
                // read cache for the next build, on the payload service's
                // own task and again per job. On a full block of transfers
                // that delays the job by tens of milliseconds and the next
                // block touches other accounts. `N42_PAYLOAD_PRECACHE=0`
                // turns it off.
                .with_pre_cache_state(
                    !std::env::var("N42_PAYLOAD_PRECACHE").is_ok_and(|v| v == "0"),
                ),
            )
            .network(EthereumNetworkBuilder::default())
            .consensus({
                let mut builder =
                    N42ConsensusBuilder::new(Some(self.consensus_state.validator_set.clone()));
                if let Some(resolver) = self.validator_set_resolver.clone() {
                    builder = builder.with_validator_set_resolver(resolver);
                }
                builder.with_header_profile(self.header_profile)
            })
    }

    fn add_ons(&self) -> Self::AddOns {
        let validator = N42EngineValidatorBuilder::new(self.header_profile);
        let rpc_middleware =
            Gov5RpcCompatLayer::new(self.header_profile == N42HeaderProfile::Gov5H2);
        N42AddOns::new(RpcAddOns::new(
            N42EthApiBuilder,
            validator,
            BasicEngineApiBuilder::default(),
            N42EngineTreeValidatorBuilder::new(validator, self.qmdb_state_root_store.clone())
                .with_trusted_state_root(self.trusted_state_root)
                .with_eof_guard(self.header_profile == N42HeaderProfile::Gov5H2),
            rpc_middleware,
            Identity::new(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use n42_consensus::ValidatorSet;

    fn make_node() -> N42Node {
        let state = Arc::new(SharedConsensusState::new(ValidatorSet::new(&[], 0)));
        N42Node::new(state)
    }

    #[test]
    fn test_n42_node_with_state() {
        let _ = make_node();
    }

    #[test]
    fn test_n42_node_clone() {
        let _ = make_node().clone();
    }

    #[test]
    fn test_n42_node_debug() {
        assert!(format!("{:?}", make_node()).contains("N42Node"));
    }
}
