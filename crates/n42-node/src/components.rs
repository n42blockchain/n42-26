use arc_swap::ArcSwapOption;
use n42_consensus::{
    CommitteePoolConfig, N42Consensus, N42HeaderProfile, ValidatorSet, ValidatorSetResolver,
    shared_committee_pool,
};
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
#[derive(Default, Clone)]
#[non_exhaustive]
pub struct N42ConsensusBuilder {
    validator_set: Option<Arc<ArcSwapOption<ValidatorSet>>>,
    validator_set_resolver: Option<ValidatorSetResolver>,
    header_profile: N42HeaderProfile,
}

impl std::fmt::Debug for N42ConsensusBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("N42ConsensusBuilder")
            .field("has_validator_set", &self.validator_set.is_some())
            .field(
                "has_validator_set_resolver",
                &self.validator_set_resolver.is_some(),
            )
            .field("header_profile", &self.header_profile)
            .finish()
    }
}

impl N42ConsensusBuilder {
    pub fn new(validator_set: Option<Arc<ArcSwapOption<ValidatorSet>>>) -> Self {
        Self {
            validator_set,
            validator_set_resolver: None,
            header_profile: N42HeaderProfile::Ethereum,
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

        // gov5's committee-evidence link: when the genesis enables
        // `hotstuff.committeePool`, every header's parentBeaconRoot must be
        // Blake3 of the parent's simulated evidence. The 200k-key pool is
        // derived once per process and shared with the block builder.
        let committee_pool = if self.header_profile == N42HeaderProfile::Gov5H2 {
            match CommitteePoolConfig::from_genesis(chain_spec.genesis())? {
                Some(config) => {
                    let started = std::time::Instant::now();
                    let pool = shared_committee_pool(&config)?;
                    info!(
                        target: "n42::consensus",
                        pool_size = config.pool_size,
                        committee_size = config.committee_size,
                        ramp_blocks = config.ramp_blocks,
                        derive_ms = started.elapsed().as_millis() as u64,
                        "gov5 committee pool ready: parentBeaconRoot is verified against rebuilt committee evidence"
                    );
                    Some(pool)
                }
                None => {
                    info!(
                        target: "n42::consensus",
                        "genesis has no enabled hotstuff.committeePool: parentBeaconRoot is not checked against committee evidence"
                    );
                    None
                }
            }
        } else {
            None
        };

        let mut consensus = if let Some(validator_set) = self.validator_set {
            let current = validator_set.load_full();
            info!(
                target: "n42::consensus",
                validator_count = current.as_ref().map(|vs| vs.len()).unwrap_or(0),
                fault_tolerance = current.as_ref().map(|vs| vs.fault_tolerance()).unwrap_or(0),
                "Loaded validator set for consensus"
            );
            N42Consensus::with_validator_set_store_and_resolver(
                chain_spec,
                validator_set,
                self.validator_set_resolver,
            )
            .with_header_profile(self.header_profile)
        } else {
            info!(target: "n42::consensus", "No initial validators configured, QC verification disabled");
            N42Consensus::new(chain_spec).with_header_profile(self.header_profile)
        };
        if let Some(pool) = committee_pool {
            consensus = consensus.with_committee_pool(pool);
        }

        Ok(Arc::new(consensus))
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
        let _ = (builder, builder);
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
