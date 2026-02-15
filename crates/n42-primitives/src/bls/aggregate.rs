use super::keys::{BlsError, BlsPublicKey, BlsSignature};
use blst::min_pk::AggregateSignature as BlstAggSig;
use blst::BLST_ERROR;

const DST: &[u8] = b"BLS_SIG_BLS12381G2_XMD:SHA-256_SSWU_RO_NUL_";

pub struct AggregateSignature;

impl AggregateSignature {
    /// Aggregate multiple BLS signatures into one.
    /// Returns the aggregated signature.
    pub fn aggregate(signatures: &[&BlsSignature]) -> Result<BlsSignature, BlsError> {
        if signatures.is_empty() {
            return Err(BlsError::SigningFailed);
        }

        let sigs: Vec<&blst::min_pk::Signature> =
            signatures.iter().map(|s| s.inner()).collect();

        let agg = BlstAggSig::aggregate(&sigs, true)
            .map_err(|_| BlsError::SigningFailed)?;

        Ok(BlsSignature(agg.to_signature()))
    }

    /// Verify an aggregated signature against multiple (message, public_key) pairs.
    /// All messages must be the same for this use case (quorum voting).
    pub fn verify_aggregate(
        message: &[u8],
        signature: &BlsSignature,
        public_keys: &[&BlsPublicKey],
    ) -> Result<(), BlsError> {
        let pks: Vec<&blst::min_pk::PublicKey> =
            public_keys.iter().map(|pk| pk.inner()).collect();

        let result = signature.inner().fast_aggregate_verify(true, message, DST, &pks);

        if result != BLST_ERROR::BLST_SUCCESS {
            return Err(BlsError::VerificationFailed(result));
        }
        Ok(())
    }
}
