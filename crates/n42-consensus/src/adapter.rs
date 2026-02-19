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
use crate::protocol::quorum::{verify_qc, verify_commit_qc};
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
                // Try PrepareQC format first, then CommitQC format.
                // The QC in extra_data could be either type depending on
                // which consensus round produced it.
                verify_qc(&qc, vs)
                    .or_else(|_| verify_commit_qc(&qc, vs))
                    .map_err(|e| {
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

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, B256};
    use bitvec::prelude::*;
    use n42_chainspec::ValidatorInfo;
    use n42_primitives::{
        BlsSecretKey,
        bls::AggregateSignature,
        consensus::QuorumCertificate,
    };

    use crate::extra_data::{encode_qc_to_extra_data, extract_qc_from_extra_data};
    use crate::protocol::quorum::{verify_qc, verify_commit_qc, signing_message, commit_signing_message};
    use crate::validator::ValidatorSet;

    /// Helper: create a test validator set of size `n` along with the secret keys.
    fn test_validator_set(n: usize) -> (Vec<BlsSecretKey>, ValidatorSet) {
        let sks: Vec<_> = (0..n).map(|_| BlsSecretKey::random().unwrap()).collect();
        let infos: Vec<_> = sks
            .iter()
            .enumerate()
            .map(|(i, sk)| ValidatorInfo {
                address: Address::with_last_byte(i as u8),
                bls_public_key: sk.public_key(),
            })
            .collect();
        let f = ((n as u32).saturating_sub(1)) / 3;
        let vs = ValidatorSet::new(&infos, f);
        (sks, vs)
    }

    #[test]
    fn test_new_without_validator_set() {
        let chain_spec = n42_chainspec::n42_dev_chainspec();
        let consensus = N42Consensus::new(chain_spec);
        assert!(consensus.validator_set.is_none(), "should have no validator set initially");
    }

    #[test]
    fn test_with_validator_set() {
        let chain_spec = n42_chainspec::n42_dev_chainspec();
        let (_, vs) = test_validator_set(4);
        let consensus = N42Consensus::with_validator_set(chain_spec, vs);
        assert!(consensus.validator_set.is_some(), "should have validator set");
        let vs_ref = consensus.validator_set.as_ref().unwrap();
        assert_eq!(vs_ref.len(), 4, "validator set should have 4 validators");
        assert_eq!(vs_ref.quorum_size(), 3, "quorum should be 2*1+1 = 3");
    }

    #[test]
    fn test_set_validator_set() {
        let chain_spec = n42_chainspec::n42_dev_chainspec();
        let mut consensus = N42Consensus::new(chain_spec);
        assert!(consensus.validator_set.is_none());

        let (_, vs) = test_validator_set(7);
        consensus.set_validator_set(vs);
        assert!(consensus.validator_set.is_some());
        assert_eq!(consensus.validator_set.as_ref().unwrap().len(), 7);
    }

    #[test]
    fn test_max_extra_data_size() {
        assert_eq!(
            N42Consensus::<ChainSpec>::MAX_EXTRA_DATA_SIZE, 4096,
            "MAX_EXTRA_DATA_SIZE must be 4096 for QC storage"
        );
    }

    /// End-to-end test of the QC verification path used by adapter:
    /// encode QC → extract from extra_data → verify_qc
    #[test]
    fn test_qc_extraction_and_verification_path() {
        let (sks, vs) = test_validator_set(4);
        let view = 42u64;
        let block_hash = B256::repeat_byte(0xCC);

        // Build a valid PrepareQC (same as what the leader produces)
        let msg = signing_message(view, &block_hash);
        let sigs: Vec<_> = sks[0..3].iter().map(|sk| sk.sign(&msg)).collect();
        let sig_refs: Vec<_> = sigs.iter().collect();
        let agg = AggregateSignature::aggregate(&sig_refs).unwrap();
        let mut signers = bitvec![u8, Msb0; 0; 4];
        signers.set(0, true);
        signers.set(1, true);
        signers.set(2, true);

        let qc = QuorumCertificate {
            view,
            block_hash,
            aggregate_signature: agg,
            signers,
        };

        // Encode QC → extra_data (what the block builder does)
        let extra_data = encode_qc_to_extra_data(&qc).unwrap();

        // Extract QC from extra_data (what adapter.validate_block_post_execution does)
        let extracted = extract_qc_from_extra_data(&extra_data)
            .expect("extraction should succeed")
            .expect("should contain a QC");

        // Verify QC (what adapter does: try verify_qc first)
        let result = verify_qc(&extracted, &vs);
        assert!(result.is_ok(), "valid PrepareQC should verify");
    }

    /// End-to-end test of the CommitQC verification fallback path.
    /// adapter tries verify_qc first, then falls back to verify_commit_qc.
    #[test]
    fn test_commit_qc_verification_fallback_path() {
        let (sks, vs) = test_validator_set(4);
        let view = 99u64;
        let block_hash = B256::repeat_byte(0xDD);

        // Build a CommitQC (signed with "commit" || view || block_hash)
        let msg = commit_signing_message(view, &block_hash);
        let sigs: Vec<_> = sks[0..3].iter().map(|sk| sk.sign(&msg)).collect();
        let sig_refs: Vec<_> = sigs.iter().collect();
        let agg = AggregateSignature::aggregate(&sig_refs).unwrap();
        let mut signers = bitvec![u8, Msb0; 0; 4];
        signers.set(0, true);
        signers.set(1, true);
        signers.set(2, true);

        let qc = QuorumCertificate {
            view,
            block_hash,
            aggregate_signature: agg,
            signers,
        };

        let extra_data = encode_qc_to_extra_data(&qc).unwrap();
        let extracted = extract_qc_from_extra_data(&extra_data).unwrap().unwrap();

        // verify_qc should FAIL (different signing message format)
        let qc_result = verify_qc(&extracted, &vs);
        assert!(qc_result.is_err(), "CommitQC should fail PrepareQC verification");

        // The adapter's fallback: verify_commit_qc should SUCCEED
        let fallback_result = verify_qc(&extracted, &vs)
            .or_else(|_| verify_commit_qc(&extracted, &vs));
        assert!(fallback_result.is_ok(), "CommitQC should pass via fallback path");
    }

    #[test]
    fn test_no_qc_in_extra_data_is_acceptable() {
        use alloy_primitives::Bytes;

        // Empty extra_data — genesis block scenario
        let result = extract_qc_from_extra_data(&Bytes::new());
        assert!(result.is_ok());
        assert!(result.unwrap().is_none(), "empty extra_data should return None");

        // Non-QC extra_data — blocks during initial sync
        let result = extract_qc_from_extra_data(&Bytes::from_static(b"some other data"));
        assert!(result.is_ok());
        assert!(result.unwrap().is_none(), "non-QC extra_data should return None");
    }
}
