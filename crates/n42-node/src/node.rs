use crate::components::{N42ConsensusBuilder, N42ExecutorBuilder};
use crate::consensus_state::SharedConsensusState;
use crate::payload::N42PayloadBuilder;
use crate::pool::N42PoolBuilder;
use reth_chainspec::ChainSpec;
use reth_ethereum_engine_primitives::EthEngineTypes;
use reth_ethereum_primitives::EthPrimitives;
use reth_node_builder::{
    components::{BasicPayloadServiceBuilder, ComponentsBuilder},
    node::{FullNodeTypes, NodeTypes},
    Node, NodeAdapter,
};
use reth_node_ethereum::node::{
    EthereumAddOns, EthereumEthApiBuilder, EthereumEngineValidatorBuilder,
    EthereumNetworkBuilder,
};
use reth_provider::EthStorage;
use std::sync::Arc;

/// N42 node type configuration.
///
/// Holds shared consensus state that is injected into the PayloadBuilder
/// and made available to the on_node_started hook for the Orchestrator.
#[derive(Debug, Clone)]
pub struct N42Node {
    /// Shared consensus state between Orchestrator and PayloadBuilder.
    pub consensus_state: Arc<SharedConsensusState>,
}

impl N42Node {
    pub fn new(consensus_state: Arc<SharedConsensusState>) -> Self {
        Self { consensus_state }
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

    type AddOns =
        EthereumAddOns<NodeAdapter<N>, EthereumEthApiBuilder, EthereumEngineValidatorBuilder>;

    fn components_builder(&self) -> Self::ComponentsBuilder {
        ComponentsBuilder::default()
            .node_types::<N>()
            .pool(N42PoolBuilder::default())
            .executor(N42ExecutorBuilder::default())
            .payload(BasicPayloadServiceBuilder::new(
                N42PayloadBuilder::new(self.consensus_state.clone()),
            ))
            .network(EthereumNetworkBuilder::default())
            .consensus(N42ConsensusBuilder::default())
    }

    fn add_ons(&self) -> Self::AddOns {
        EthereumAddOns::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use n42_consensus::ValidatorSet;

    #[test]
    fn test_n42_node_with_state() {
        let vs = ValidatorSet::new(&[], 0);
        let state = Arc::new(SharedConsensusState::new(vs));
        let node = N42Node::new(state);
        let _ = node;
    }

    #[test]
    fn test_n42_node_clone() {
        let vs = ValidatorSet::new(&[], 0);
        let state = Arc::new(SharedConsensusState::new(vs));
        let node = N42Node::new(state);
        let cloned = node.clone();
        let _ = cloned;
    }

    #[test]
    fn test_n42_node_debug() {
        let vs = ValidatorSet::new(&[], 0);
        let state = Arc::new(SharedConsensusState::new(vs));
        let node = N42Node::new(state);
        let debug_str = format!("{:?}", node);
        assert!(
            debug_str.contains("N42Node"),
            "Debug output should contain 'N42Node'"
        );
    }
}
