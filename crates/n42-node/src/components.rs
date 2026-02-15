use n42_consensus::N42Consensus;
use n42_execution::N42EvmConfig;
use reth_chainspec::{ChainSpec, EthChainSpec, EthereumHardforks};
use reth_ethereum_primitives::EthPrimitives;
use reth_node_builder::{
    components::{ConsensusBuilder, ExecutorBuilder},
    node::{FullNodeTypes, NodeTypes},
    BuilderContext,
};
use std::sync::Arc;

/// N42 executor builder.
///
/// Creates the EVM configuration for N42 nodes.
/// Uses `N42EvmConfig` which wraps `EthEvmConfig` and provides extension points
/// for witness generation and state diff tracking.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct N42ExecutorBuilder;

impl<Types, Node> ExecutorBuilder<Node> for N42ExecutorBuilder
where
    Types: NodeTypes<
        ChainSpec = ChainSpec,
        Primitives = EthPrimitives,
    >,
    Node: FullNodeTypes<Types = Types>,
{
    type EVM = N42EvmConfig;

    async fn build_evm(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::EVM> {
        Ok(N42EvmConfig::new(ctx.chain_spec()))
    }
}

/// N42 consensus builder.
///
/// Creates the consensus engine for N42 nodes.
/// Phase 1: Delegates to `N42Consensus` which wraps `EthBeaconConsensus`.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct N42ConsensusBuilder;

impl<Node> ConsensusBuilder<Node> for N42ConsensusBuilder
where
    Node: FullNodeTypes<
        Types: NodeTypes<
            ChainSpec: EthChainSpec + EthereumHardforks,
            Primitives = EthPrimitives,
        >,
    >,
{
    type Consensus = Arc<N42Consensus<<Node::Types as NodeTypes>::ChainSpec>>;

    async fn build_consensus(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::Consensus> {
        Ok(Arc::new(N42Consensus::new(ctx.chain_spec())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_executor_builder_default() {
        let builder = N42ExecutorBuilder::default();
        let debug_str = format!("{:?}", builder);
        assert!(debug_str.contains("N42ExecutorBuilder"));
    }

    #[test]
    fn test_executor_builder_clone_copy() {
        let builder = N42ExecutorBuilder;
        let cloned = builder.clone();
        let copied = builder;
        let _ = (cloned, copied);
    }

    #[test]
    fn test_consensus_builder_default() {
        let builder = N42ConsensusBuilder::default();
        let debug_str = format!("{:?}", builder);
        assert!(debug_str.contains("N42ConsensusBuilder"));
    }

    #[test]
    fn test_consensus_builder_clone_copy() {
        let builder = N42ConsensusBuilder;
        let cloned = builder.clone();
        let copied = builder;
        let _ = (cloned, copied);
    }
}
