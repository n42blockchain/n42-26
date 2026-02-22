use n42_chainspec::ConsensusConfig;
use n42_consensus::{N42Consensus, ValidatorSet};
use n42_execution::N42EvmConfig;
use reth_chainspec::{ChainSpec, EthChainSpec, EthereumHardforks};
use reth_ethereum_primitives::EthPrimitives;
use reth_node_builder::{
    components::{ConsensusBuilder, ExecutorBuilder},
    node::{FullNodeTypes, NodeTypes},
    BuilderContext,
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
        let chain_spec = ctx.chain_spec();
        let consensus_config = ConsensusConfig::dev();

        if consensus_config.initial_validators.is_empty() {
            info!(target: "n42::consensus", "No initial validators configured, QC verification disabled");
            Ok(Arc::new(N42Consensus::new(chain_spec)))
        } else {
            let validator_set = ValidatorSet::new(
                &consensus_config.initial_validators,
                consensus_config.fault_tolerance,
            );
            info!(
                target: "n42::consensus",
                validator_count = validator_set.len(),
                fault_tolerance = consensus_config.fault_tolerance,
                "Loaded validator set for consensus"
            );
            Ok(Arc::new(N42Consensus::with_validator_set(chain_spec, validator_set)))
        }
    }
}

/// Loads a `ValidatorSet` from a `ConsensusConfig`. Also used by orchestrator startup.
pub fn load_validator_set(config: &ConsensusConfig) -> ValidatorSet {
    ValidatorSet::new(&config.initial_validators, config.fault_tolerance)
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
    fn test_consensus_builder_clone_copy() {
        let builder = N42ConsensusBuilder;
        let _ = (builder.clone(), builder);
    }

    #[test]
    fn test_load_validator_set_empty() {
        let vs = load_validator_set(&ConsensusConfig::dev());
        assert_eq!(vs.len(), 0);
    }
}
