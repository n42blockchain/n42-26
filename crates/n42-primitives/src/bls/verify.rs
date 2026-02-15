use super::keys::{BlsError, BlsPublicKey, BlsSignature};
use blst::BLST_ERROR;

/// Batch-verify multiple (message, signature, public_key) tuples.
/// More efficient than verifying each individually when there are many.
pub fn batch_verify(
    messages: &[&[u8]],
    signatures: &[&BlsSignature],
    public_keys: &[&BlsPublicKey],
) -> Result<(), BlsError> {
    if messages.len() != signatures.len() || signatures.len() != public_keys.len() {
        return Err(BlsError::VerificationFailed(BLST_ERROR::BLST_BAD_ENCODING));
    }

    if messages.is_empty() {
        return Ok(());
    }

    // For now, fall back to individual verification.
    // blst's batch verification requires careful setup with random scalars.
    // This will be optimized in Phase 3.
    for i in 0..messages.len() {
        public_keys[i].verify(messages[i], signatures[i])?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::keys::BlsSecretKey;

    #[test]
    fn test_batch_verify_success() {
        let msg1 = b"message one";
        let msg2 = b"message two";
        let msg3 = b"message three";

        let sk1 = BlsSecretKey::random().unwrap();
        let sk2 = BlsSecretKey::random().unwrap();
        let sk3 = BlsSecretKey::random().unwrap();

        let pk1 = sk1.public_key();
        let pk2 = sk2.public_key();
        let pk3 = sk3.public_key();

        let sig1 = sk1.sign(msg1);
        let sig2 = sk2.sign(msg2);
        let sig3 = sk3.sign(msg3);

        let messages: Vec<&[u8]> = vec![msg1.as_ref(), msg2.as_ref(), msg3.as_ref()];
        let signatures = vec![&sig1, &sig2, &sig3];
        let public_keys = vec![&pk1, &pk2, &pk3];

        batch_verify(&messages, &signatures, &public_keys)
            .expect("batch verification should succeed for correct inputs");
    }

    #[test]
    fn test_batch_verify_mismatched_lengths() {
        let sk1 = BlsSecretKey::random().unwrap();
        let sk2 = BlsSecretKey::random().unwrap();
        let pk1 = sk1.public_key();
        let pk2 = sk2.public_key();
        let sig1 = sk1.sign(b"a");
        let sig2 = sk2.sign(b"b");

        // Two messages but three signatures => mismatched lengths
        let messages: Vec<&[u8]> = vec![b"a".as_ref(), b"b".as_ref()];
        let signatures = vec![&sig1, &sig2, &sig1];
        let public_keys = vec![&pk1, &pk2];

        let result = batch_verify(&messages, &signatures, &public_keys);
        assert!(result.is_err(), "batch verify should fail for mismatched lengths");
    }

    #[test]
    fn test_batch_verify_empty() {
        let messages: Vec<&[u8]> = vec![];
        let signatures: Vec<&BlsSignature> = vec![];
        let public_keys: Vec<&BlsPublicKey> = vec![];

        batch_verify(&messages, &signatures, &public_keys)
            .expect("batch verify on empty arrays should succeed");
    }
}
