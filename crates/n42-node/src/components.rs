use arc_swap::ArcSwapOption;
use n42_consensus::{N42Consensus, ValidatorSet};
use n42_execution::N42EvmConfig;
use reth_chainspec::{ChainSpec, EthChainSpec, EthereumHardforks};
use reth_ethereum_primitives::EthPrimitives;
use reth_node_builder::{
    BuilderContext,
    components::{ConsensusBuilder, ExecutorBuilder},
    node::{FullNodeTypes, NodeTypes},
};
use std::sync::Arc;
use tracing::info;

/// N42 executor builder — creates the EVM configuration for N42 nodes.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct N42ExecutorBuilder;

impl<Types, Node> ExecutorBuilder<Node> for N42ExecutorBuilder
where
    Types: NodeTypes<ChainSpec = ChainSpec, Primitives = EthPrimitives>,
    Node: FullNodeTypes<Types = Types>,
{
    type EVM = N42EvmConfig;

    async fn build_evm(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::EVM> {
        Ok(N42EvmConfig::new(ctx.chain_spec()))
    }
}

/// N42 consensus builder.
///
/// Loads the validator set from `ConsensusConfig`. If no validators are configured
/// (e.g. a standard Ethereum chainspec), falls back to N42Consensus without a
/// validator set (QC verification is skipped).
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct N42ConsensusBuilder {
    validator_set: Option<Arc<ArcSwapOption<ValidatorSet>>>,
}

impl N42ConsensusBuilder {
    pub fn new(validator_set: Option<Arc<ArcSwapOption<ValidatorSet>>>) -> Self {
        Self { validator_set }
    }
}

impl<Node> ConsensusBuilder<Node> for N42ConsensusBuilder
where
    Node: FullNodeTypes<
        Types: NodeTypes<ChainSpec: EthChainSpec + EthereumHardforks, Primitives = EthPrimitives>,
    >,
{
    type Consensus = Arc<N42Consensus<<Node::Types as NodeTypes>::ChainSpec>>;

    async fn build_consensus(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::Consensus> {
        let chain_spec = ctx.chain_spec();

        if let Some(validator_set) = self.validator_set {
            let current = validator_set.load_full();
            info!(
                target: "n42::consensus",
                validator_count = current.as_ref().map(|vs| vs.len()).unwrap_or(0),
                fault_tolerance = current.as_ref().map(|vs| vs.fault_tolerance()).unwrap_or(0),
                "Loaded validator set for consensus"
            );
            Ok(Arc::new(N42Consensus::with_validator_set_store(
                chain_spec,
                validator_set,
            )))
        } else {
            info!(target: "n42::consensus", "No initial validators configured, QC verification disabled");
            Ok(Arc::new(N42Consensus::new(chain_spec)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_executor_builder_default() {
        let builder = N42ExecutorBuilder::default();
        assert!(format!("{:?}", builder).contains("N42ExecutorBuilder"));
    }

    #[test]
    fn test_executor_builder_clone_copy() {
        let builder = N42ExecutorBuilder;
        let _ = (builder.clone(), builder);
    }

    #[test]
    fn test_consensus_builder_default() {
        let builder = N42ConsensusBuilder::default();
        assert!(format!("{:?}", builder).contains("N42ConsensusBuilder"));
    }

    #[test]
    fn test_consensus_builder_clone() {
        let builder = N42ConsensusBuilder::default();
        let _ = builder.clone();
    }
}
