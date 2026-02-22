use super::keys::{BlsError, BlsPublicKey, BlsSignature};
use super::DST;
use blst::min_pk::AggregateSignature as BlstAggSig;
use blst::BLST_ERROR;

pub struct AggregateSignature;

impl AggregateSignature {
    /// Aggregate multiple BLS signatures into a single signature.
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

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::keys::BlsSecretKey;

    #[test]
    fn test_aggregate_and_verify() {
        let message = b"consensus vote";

        let sk1 = BlsSecretKey::random().unwrap();
        let sk2 = BlsSecretKey::random().unwrap();
        let sk3 = BlsSecretKey::random().unwrap();

        let pk1 = sk1.public_key();
        let pk2 = sk2.public_key();
        let pk3 = sk3.public_key();

        let sig1 = sk1.sign(message);
        let sig2 = sk2.sign(message);
        let sig3 = sk3.sign(message);

        let agg_sig = AggregateSignature::aggregate(&[&sig1, &sig2, &sig3])
            .expect("aggregation should succeed");

        AggregateSignature::verify_aggregate(message, &agg_sig, &[&pk1, &pk2, &pk3])
            .expect("aggregate verification should succeed");
    }

    #[test]
    fn test_aggregate_wrong_message() {
        let sk1 = BlsSecretKey::random().unwrap();
        let sk2 = BlsSecretKey::random().unwrap();
        let pk1 = sk1.public_key();
        let pk2 = sk2.public_key();
        let sig1 = sk1.sign(b"correct message");
        let sig2 = sk2.sign(b"correct message");
        let agg_sig = AggregateSignature::aggregate(&[&sig1, &sig2]).unwrap();
        assert!(AggregateSignature::verify_aggregate(b"wrong message", &agg_sig, &[&pk1, &pk2]).is_err());
    }

    #[test]
    fn test_aggregate_empty_signatures() {
        let result = AggregateSignature::aggregate(&[]);
        assert!(result.is_err(), "aggregating zero signatures should return error");
    }

    #[test]
    fn test_aggregate_single_signature() {
        let message = b"single signer";
        let sk = BlsSecretKey::random().unwrap();
        let pk = sk.public_key();
        let sig = sk.sign(message);
        let agg_sig = AggregateSignature::aggregate(&[&sig]).expect("aggregation should succeed");
        AggregateSignature::verify_aggregate(message, &agg_sig, &[&pk])
            .expect("single-sig verification should succeed");
    }
}
