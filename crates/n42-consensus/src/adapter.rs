use arc_swap::ArcSwapOption;
use reth_chainspec::{ChainSpec, EthChainSpec, EthereumHardforks};
use reth_consensus::{Consensus, ConsensusError, FullConsensus, HeaderValidator};
use reth_ethereum_consensus::EthBeaconConsensus;
use reth_execution_types::BlockExecutionResult;
use reth_primitives_traits::{
    Block, BlockHeader, NodePrimitives, RecoveredBlock, SealedBlock, SealedHeader,
};
use std::fmt::Debug;
use std::sync::Arc;

use crate::validator::ValidatorSet;

/// Resolves the validator set that should verify a QC for a given view.
pub type ValidatorSetResolver = Arc<dyn Fn(u64) -> Option<Arc<ValidatorSet>> + Send + Sync>;

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
#[derive(Clone)]
pub struct N42Consensus<C = ChainSpec> {
    /// Inner Ethereum consensus for standard EVM rule checks.
    inner: EthBeaconConsensus<C>,
    /// Validator set for QC verification.
    /// None during initial sync when validator set is not yet loaded.
    validator_set: Arc<ArcSwapOption<ValidatorSet>>,
    /// Optional epoch-aware validator-set resolver for QC verification.
    validator_set_resolver: Option<ValidatorSetResolver>,
}

impl<C> std::fmt::Debug for N42Consensus<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("N42Consensus")
            .field("has_validator_set", &self.validator_set.load().is_some())
            .field(
                "has_validator_set_resolver",
                &self.validator_set_resolver.is_some(),
            )
            .finish()
    }
}

impl<C> N42Consensus<C>
where
    C: EthChainSpec + EthereumHardforks,
{
    /// Create a new N42 consensus adapter (without validator set).
    /// QC verification is skipped until `set_validator_set` is called.
    pub fn new(chain_spec: Arc<C>) -> Self {
        Self {
            inner: EthBeaconConsensus::new(chain_spec),
            validator_set: Arc::new(ArcSwapOption::empty()),
            validator_set_resolver: None,
        }
    }

    /// Create a new N42 consensus adapter with a validator set for QC verification.
    pub fn with_validator_set(chain_spec: Arc<C>, validator_set: ValidatorSet) -> Self {
        Self::with_validator_set_store(
            chain_spec,
            Arc::new(ArcSwapOption::from_pointee(validator_set)),
        )
    }

    /// Create a new N42 consensus adapter backed by a shared validator-set store.
    pub fn with_validator_set_store(
        chain_spec: Arc<C>,
        validator_set: Arc<ArcSwapOption<ValidatorSet>>,
    ) -> Self {
        Self::with_validator_set_store_and_resolver(chain_spec, validator_set, None)
    }

    /// Create a new N42 consensus adapter backed by a shared validator-set store and
    /// an optional epoch-aware resolver.
    pub fn with_validator_set_store_and_resolver(
        chain_spec: Arc<C>,
        validator_set: Arc<ArcSwapOption<ValidatorSet>>,
        validator_set_resolver: Option<ValidatorSetResolver>,
    ) -> Self {
        Self {
            inner: EthBeaconConsensus::new(chain_spec),
            validator_set,
            validator_set_resolver,
        }
    }

    /// Sets or updates the validator set.
    pub fn set_validator_set(&mut self, validator_set: ValidatorSet) {
        self.validator_set.store(Some(Arc::new(validator_set)));
    }

    #[cfg(test)]
    fn validator_set_for_view(&self, view: u64) -> Option<Arc<ValidatorSet>> {
        self.validator_set_resolver
            .as_ref()
            .and_then(|resolver| resolver(view))
            .or_else(|| self.validator_set.load_full())
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
        block_access_list_hash: Option<alloy_primitives::B256>,
    ) -> Result<(), ConsensusError> {
        <EthBeaconConsensus<C> as FullConsensus<N>>::validate_block_post_execution(
            &self.inner,
            block,
            result,
            receipt_root_bloom,
            block_access_list_hash,
        )
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
    use n42_primitives::{BlsSecretKey, bls::AggregateSignature, consensus::QuorumCertificate};

    use crate::extra_data::{encode_qc_to_extra_data, extract_qc_from_extra_data};
    use crate::protocol::quorum::{
        commit_signing_message, signing_message, verify_commit_qc, verify_qc,
    };
    use crate::validator::ValidatorSet;

    fn test_key(seed: u8) -> BlsSecretKey {
        BlsSecretKey::key_gen(&[seed; 32]).expect("deterministic test key should be valid")
    }

    /// Helper: create a test validator set of size `n` along with the secret keys.
    fn test_validator_set(n: usize) -> (Vec<BlsSecretKey>, ValidatorSet) {
        let sks: Vec<_> = (0..n).map(|i| test_key(0x60 + i as u8)).collect();
        let infos: Vec<_> = sks
            .iter()
            .enumerate()
            .map(|(i, sk)| ValidatorInfo {
                address: Address::with_last_byte(i as u8),
                bls_public_key: sk.public_key(),
                p2p_peer_id: None,
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
        assert!(consensus.validator_set.load_full().is_none());
    }

    #[test]
    fn test_with_validator_set() {
        let chain_spec = n42_chainspec::n42_dev_chainspec();
        let (_, vs) = test_validator_set(4);
        let consensus = N42Consensus::with_validator_set(chain_spec, vs);
        let loaded = consensus.validator_set.load_full();
        assert!(loaded.is_some());
        let vs_ref = loaded.as_ref().unwrap();
        assert_eq!(vs_ref.len(), 4);
        assert_eq!(vs_ref.quorum_size(), 3);
    }

    #[test]
    fn test_set_validator_set() {
        let chain_spec = n42_chainspec::n42_dev_chainspec();
        let mut consensus = N42Consensus::new(chain_spec);
        assert!(consensus.validator_set.load_full().is_none());

        let (_, vs) = test_validator_set(7);
        consensus.set_validator_set(vs);
        let loaded = consensus.validator_set.load_full();
        assert!(loaded.is_some());
        assert_eq!(loaded.as_ref().unwrap().len(), 7);
    }

    #[test]
    fn test_validator_set_resolver_overrides_current_set_for_matching_view() {
        let chain_spec = n42_chainspec::n42_dev_chainspec();
        let (_, current_vs) = test_validator_set(4);
        let (_, resolved_vs) = test_validator_set(7);
        let expected_resolved_len = resolved_vs.len();
        let expected_current_len = current_vs.len();
        let resolver_vs = resolved_vs.clone();
        let resolver: ValidatorSetResolver = Arc::new(move |view| {
            if view == 42 {
                Some(Arc::new(resolver_vs.clone()))
            } else {
                None
            }
        });

        let consensus = N42Consensus::with_validator_set_store_and_resolver(
            chain_spec,
            Arc::new(ArcSwapOption::from_pointee(current_vs)),
            Some(resolver),
        );

        let resolved = consensus.validator_set_for_view(42).unwrap();
        assert_eq!(resolved.len(), expected_resolved_len);

        let fallback = consensus.validator_set_for_view(1).unwrap();
        assert_eq!(fallback.len(), expected_current_len);
    }

    /// End-to-end test of the QC verification path used by adapter:
    /// encode QC → extract from extra_data → verify_qc
    #[test]
    fn test_qc_extraction_and_verification_path() {
        let (sks, vs) = test_validator_set(4);
        let view = 42u64;
        let block_hash = B256::repeat_byte(0xCC);

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

        let extra_data = encode_qc_to_extra_data(&qc).unwrap();
        let extracted = extract_qc_from_extra_data(&extra_data)
            .expect("extraction should succeed")
            .expect("should contain a QC");

        let result = verify_qc(&extracted, &vs);
        assert!(result.is_ok());
    }

    /// End-to-end test of the CommitQC verification fallback path.
    /// adapter tries verify_qc first, then falls back to verify_commit_qc.
    #[test]
    fn test_commit_qc_verification_fallback_path() {
        let (sks, vs) = test_validator_set(4);
        let view = 99u64;
        let block_hash = B256::repeat_byte(0xDD);
        let changes_hash = B256::ZERO;

        let msg = commit_signing_message(view, &block_hash, &changes_hash);
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

        assert!(verify_qc(&extracted, &vs).is_err());

        let fallback_result = verify_qc(&extracted, &vs)
            .or_else(|_| verify_commit_qc(&extracted, &vs, &changes_hash));
        assert!(fallback_result.is_ok());
    }

    #[test]
    fn test_no_qc_in_extra_data_is_acceptable() {
        use alloy_primitives::Bytes;

        let result = extract_qc_from_extra_data(&Bytes::new());
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());

        let result = extract_qc_from_extra_data(&Bytes::from_static(b"some other data"));
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }
}
