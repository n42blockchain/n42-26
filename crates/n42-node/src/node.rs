use crate::components::{N42ConsensusBuilder, N42ExecutorBuilder};
use crate::consensus_state::SharedConsensusState;
use crate::engine_validator::N42EngineValidatorBuilder;
use crate::payload::N42PayloadBuilder;
use crate::pool::N42PoolBuilder;
use crate::qmdb_state_root::{Gov5QmdbStateRootStore, N42EngineTreeValidatorBuilder};
use n42_consensus::{N42HeaderProfile, ValidatorSetResolver};
use reth_chainspec::ChainSpec;
use reth_ethereum_engine_primitives::EthEngineTypes;
use reth_ethereum_primitives::EthPrimitives;
use reth_node_builder::{
    Node, NodeAdapter,
    components::{BasicPayloadServiceBuilder, ComponentsBuilder},
    node::{FullNodeTypes, NodeTypes},
    rpc::{BasicEngineApiBuilder, Identity, RpcAddOns},
};
use reth_node_ethereum::node::{EthereumAddOns, EthereumEthApiBuilder, EthereumNetworkBuilder};
use reth_provider::EthStorage;
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
}

impl NodeTypes for N42Node {
    type Primitives = EthPrimitives;
    type ChainSpec = ChainSpec;
    type Storage = EthStorage;
    type Payload = EthEngineTypes;
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

    type AddOns = EthereumAddOns<
        NodeAdapter<N>,
        EthereumEthApiBuilder,
        N42EngineValidatorBuilder,
        BasicEngineApiBuilder<N42EngineValidatorBuilder>,
        N42EngineTreeValidatorBuilder,
    >;

    fn components_builder(&self) -> Self::ComponentsBuilder {
        ComponentsBuilder::default()
            .node_types::<N>()
            .pool(N42PoolBuilder::default())
            .executor(N42ExecutorBuilder::default())
            .payload(BasicPayloadServiceBuilder::new(N42PayloadBuilder::new(
                self.consensus_state.clone(),
            )))
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
        EthereumAddOns::new(RpcAddOns::new(
            EthereumEthApiBuilder::default(),
            validator,
            BasicEngineApiBuilder::default(),
            N42EngineTreeValidatorBuilder::new(validator, self.qmdb_state_root_store.clone()),
            Default::default(),
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
