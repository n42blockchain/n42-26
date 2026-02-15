use crate::components::{N42ConsensusBuilder, N42ExecutorBuilder};
use reth_chainspec::ChainSpec;
use reth_ethereum_engine_primitives::EthEngineTypes;
use reth_ethereum_primitives::EthPrimitives;
use reth_node_builder::{
    components::ComponentsBuilder,
    node::{FullNodeTypes, NodeTypes},
    Node, NodeAdapter,
};
use reth_node_ethereum::node::{
    EthereumAddOns, EthereumEthApiBuilder, EthereumEngineValidatorBuilder,
    EthereumNetworkBuilder, EthereumPoolBuilder,
};
use reth_node_ethereum::EthereumPayloadBuilder;
use reth_node_builder::components::BasicPayloadServiceBuilder;
use reth_provider::EthStorage;

/// N42 node type configuration.
///
/// This defines the core types used by the N42 blockchain node.
/// Phase 1 reuses Ethereum primitives, engine types, and storage.
/// Custom consensus and execution builders are used.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct N42Node;

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
        EthereumPoolBuilder,
        BasicPayloadServiceBuilder<EthereumPayloadBuilder>,
        EthereumNetworkBuilder,
        N42ExecutorBuilder,
        N42ConsensusBuilder,
    >;

    type AddOns =
        EthereumAddOns<NodeAdapter<N>, EthereumEthApiBuilder, EthereumEngineValidatorBuilder>;

    fn components_builder(&self) -> Self::ComponentsBuilder {
        ComponentsBuilder::default()
            .node_types::<N>()
            .pool(EthereumPoolBuilder::default())
            .executor(N42ExecutorBuilder::default())
            .payload(BasicPayloadServiceBuilder::default())
            .network(EthereumNetworkBuilder::default())
            .consensus(N42ConsensusBuilder::default())
    }

    fn add_ons(&self) -> Self::AddOns {
        EthereumAddOns::default()
    }
}
