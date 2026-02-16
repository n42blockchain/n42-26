use reth_chainspec::{ChainSpec, EthChainSpec, EthereumHardforks};
use reth_consensus::{Consensus, ConsensusError, FullConsensus, HeaderValidator};
use reth_ethereum_consensus::EthBeaconConsensus;
use reth_execution_types::BlockExecutionResult;
use reth_primitives_traits::{
    AlloyBlockHeader, Block, BlockHeader, NodePrimitives, RecoveredBlock, SealedBlock, SealedHeader,
};
use std::fmt::Debug;
use std::sync::Arc;

use crate::extra_data::extract_qc_from_extra_data;
use crate::protocol::quorum::verify_qc;
use crate::validator::ValidatorSet;

/// N42 consensus adapter that integrates with the reth node builder.
///
/// Wraps `EthBeaconConsensus` for standard Ethereum validation (gas limits,
/// receipt roots, etc.) and layers N42-specific HotStuff-2 validation on top:
/// - QC verification for committed blocks
/// - Validator set checks
///
/// The adapter is intentionally stateless—it validates individual blocks/headers
/// against the consensus rules. The stateful consensus engine (`ConsensusEngine`)
/// runs as a separate background task.
#[derive(Debug, Clone)]
pub struct N42Consensus<C = ChainSpec> {
    /// Inner Ethereum consensus for standard EVM rule checks.
    inner: EthBeaconConsensus<C>,
    /// Validator set for QC verification.
    /// None during initial sync when validator set is not yet loaded.
    validator_set: Option<ValidatorSet>,
}

impl<C> N42Consensus<C>
where
    C: EthChainSpec + EthereumHardforks,
{
    /// Maximum extra_data size for N42 blocks.
    /// N42 stores QuorumCertificate (BLS aggregate signature + signer bitmap)
    /// in the header extra_data field, which exceeds Ethereum's default 32-byte limit.
    const MAX_EXTRA_DATA_SIZE: usize = 4096;

    /// Create a new N42 consensus adapter (without validator set).
    /// QC verification is skipped until `set_validator_set` is called.
    pub fn new(chain_spec: Arc<C>) -> Self {
        Self {
            inner: EthBeaconConsensus::new(chain_spec)
                .with_max_extra_data_size(Self::MAX_EXTRA_DATA_SIZE),
            validator_set: None,
        }
    }

    /// Create a new N42 consensus adapter with a validator set for QC verification.
    pub fn with_validator_set(chain_spec: Arc<C>, validator_set: ValidatorSet) -> Self {
        Self {
            inner: EthBeaconConsensus::new(chain_spec)
                .with_max_extra_data_size(Self::MAX_EXTRA_DATA_SIZE),
            validator_set: Some(validator_set),
        }
    }

    /// Sets or updates the validator set.
    pub fn set_validator_set(&mut self, validator_set: ValidatorSet) {
        self.validator_set = Some(validator_set);
    }
}

impl<C, N> FullConsensus<N> for N42Consensus<C>
where
    C: EthChainSpec<Header = N::BlockHeader> + EthereumHardforks + Debug + Send + Sync,
    N: NodePrimitives,
{
    fn validate_block_post_execution(
        &self,
        block: &RecoveredBlock<N::Block>,
        result: &BlockExecutionResult<N::Receipt>,
        receipt_root_bloom: Option<reth_consensus::ReceiptRootBloom>,
    ) -> Result<(), ConsensusError> {
        // First: standard Ethereum validation (gas, receipts, state root, etc.)
        <EthBeaconConsensus<C> as FullConsensus<N>>::validate_block_post_execution(
            &self.inner,
            block,
            result,
            receipt_root_bloom,
        )?;

        // Extract and verify QC from block extra_data.
        // If the validator set is loaded and the header contains a QC,
        // verify the aggregate BLS signature against the signer public keys.
        if let Some(ref vs) = self.validator_set {
            let extra_data = block.header().extra_data();
            if let Some(qc) = extract_qc_from_extra_data(extra_data)? {
                verify_qc(&qc, vs).map_err(|e| {
                    ConsensusError::Other(e.to_string())
                })?;
            }
            // No QC in extra_data is acceptable for:
            // - Genesis block (view 0)
            // - Blocks during initial sync before consensus engine activation
        }

        Ok(())
    }
}

impl<B, C> Consensus<B> for N42Consensus<C>
where
    B: Block,
    C: EthChainSpec<Header = B::Header> + EthereumHardforks + Debug + Send + Sync,
{
    fn validate_body_against_header(
        &self,
        body: &B::Body,
        header: &SealedHeader<B::Header>,
    ) -> Result<(), ConsensusError> {
        <EthBeaconConsensus<C> as Consensus<B>>::validate_body_against_header(
            &self.inner,
            body,
            header,
        )
    }

    fn validate_block_pre_execution(&self, block: &SealedBlock<B>) -> Result<(), ConsensusError> {
        self.inner.validate_block_pre_execution(block)
    }
}

impl<H, C> HeaderValidator<H> for N42Consensus<C>
where
    H: BlockHeader,
    C: EthChainSpec<Header = H> + EthereumHardforks + Debug + Send + Sync,
{
    fn validate_header(&self, header: &SealedHeader<H>) -> Result<(), ConsensusError> {
        self.inner.validate_header(header)
    }

    fn validate_header_against_parent(
        &self,
        header: &SealedHeader<H>,
        parent: &SealedHeader<H>,
    ) -> Result<(), ConsensusError> {
        self.inner.validate_header_against_parent(header, parent)
    }
}
