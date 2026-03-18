use crate::components::{N42ConsensusBuilder, N42ExecutorBuilder};
use crate::consensus_state::SharedConsensusState;
use crate::payload::N42PayloadBuilder;
use crate::pool::N42PoolBuilder;
use n42_consensus::ValidatorSetResolver;
use reth_chainspec::ChainSpec;
use reth_ethereum_engine_primitives::EthEngineTypes;
use reth_ethereum_primitives::EthPrimitives;
use reth_node_builder::{
    Node, NodeAdapter,
    components::{BasicPayloadServiceBuilder, ComponentsBuilder},
    node::{FullNodeTypes, NodeTypes},
};
use reth_node_ethereum::node::{
    EthereumAddOns, EthereumEngineValidatorBuilder, EthereumEthApiBuilder, EthereumNetworkBuilder,
};
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
}

impl std::fmt::Debug for N42Node {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("N42Node")
            .field(
                "has_validator_set_resolver",
                &self.validator_set_resolver.is_some(),
            )
            .finish()
    }
}

impl N42Node {
    pub fn new(consensus_state: Arc<SharedConsensusState>) -> Self {
        Self {
            consensus_state,
            validator_set_resolver: None,
        }
    }

    pub fn with_validator_set_resolver(
        mut self,
        validator_set_resolver: ValidatorSetResolver,
    ) -> Self {
        self.validator_set_resolver = Some(validator_set_resolver);
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

    type AddOns =
        EthereumAddOns<NodeAdapter<N>, EthereumEthApiBuilder, EthereumEngineValidatorBuilder>;

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
                let builder =
                    N42ConsensusBuilder::new(Some(self.consensus_state.validator_set.clone()));
                if let Some(resolver) = self.validator_set_resolver.clone() {
                    builder.with_validator_set_resolver(resolver)
                } else {
                    builder
                }
            })
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
