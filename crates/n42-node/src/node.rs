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
/// Holds shared consensus state injected into the PayloadBuilder and available
/// to the `on_node_started` hook for the Orchestrator.
#[derive(Debug, Clone)]
pub struct N42Node {
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
