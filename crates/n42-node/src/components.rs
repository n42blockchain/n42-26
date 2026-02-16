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

/// N42 executor builder.
///
/// Creates the EVM configuration for N42 nodes.
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
/// Creates the consensus adapter with validator set loaded from `ConsensusConfig`.
/// If no consensus config is available (e.g. using a standard Ethereum chainspec),
/// falls back to creating an N42Consensus without validator set (QC verification skipped).
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

        // Try to load validator set from consensus config.
        // In production, this would be stored in genesis extra_data or a separate config file.
        // For now, we try to load from ConsensusConfig::dev() which gives us a starting point.
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

/// Loads a `ValidatorSet` from a `ConsensusConfig`.
/// This is also used by the orchestrator startup to create the consensus engine.
pub fn load_validator_set(config: &ConsensusConfig) -> ValidatorSet {
    ValidatorSet::new(&config.initial_validators, config.fault_tolerance)
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

    #[test]
    fn test_load_validator_set_empty() {
        let config = ConsensusConfig::dev();
        let vs = load_validator_set(&config);
        assert_eq!(vs.len(), 0);
    }
}
