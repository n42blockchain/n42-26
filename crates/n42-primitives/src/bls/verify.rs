use super::keys::{BlsError, BlsPublicKey, BlsSignature};
use super::DST;
use blst::blst_scalar;
use blst::min_pk::Signature;
use blst::BLST_ERROR;

/// Creates a blst_scalar from a 64-bit little-endian value.
/// The scalar is stored in a 256-bit field (32 bytes), with the
/// lower 8 bytes containing the value and upper bytes zeroed.
fn scalar_from_u64(val: u64) -> blst_scalar {
    let mut s = blst_scalar { b: [0u8; 32] };
    s.b[..8].copy_from_slice(&val.to_le_bytes());
    s
}

/// Batch-verify multiple (message, signature, public_key) tuples.
/// Uses blst's multi-pairing with random 64-bit scalars for rogue-key attack protection.
/// Significantly faster than individual verification (~50% savings with many signatures).
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

    // Single signature: use direct verification (no overhead from random scalars).
    if messages.len() == 1 {
        return public_keys[0].verify(messages[0], signatures[0]);
    }

    let mut rands: Vec<blst_scalar> = Vec::with_capacity(messages.len());
    rands.push(scalar_from_u64(1));

    for _ in 1..messages.len() {
        let mut rand_bytes = [0u8; 8];
        getrandom::fill(&mut rand_bytes).map_err(|_| BlsError::SigningFailed)?;
        let mut val = u64::from_le_bytes(rand_bytes);
        if val == 0 {
            val = 1;
        }
        rands.push(scalar_from_u64(val));
    }

    let sigs: Vec<&Signature> = signatures.iter().map(|s| s.inner()).collect();
    let pks: Vec<&blst::min_pk::PublicKey> = public_keys.iter().map(|pk| pk.inner()).collect();

    let result = Signature::verify_multiple_aggregate_signatures(
        messages,
        DST,
        &pks,
        false,
        &sigs,
        true,
        &rands,
        64,
    );

    if result != BLST_ERROR::BLST_SUCCESS {
        return Err(BlsError::VerificationFailed(result));
    }

    Ok(())
}

/// Batch-verify with fallback: if the batch fails, falls back to individual
/// verification to identify which signatures are invalid.
///
/// Returns `Ok(())` if all signatures are valid, or `Err` with the indices
/// of invalid signatures.
pub fn batch_verify_with_fallback(
    messages: &[&[u8]],
    signatures: &[&BlsSignature],
    public_keys: &[&BlsPublicKey],
) -> Result<(), Vec<usize>> {
    if messages.len() != signatures.len() || signatures.len() != public_keys.len() {
        return Err((0..messages.len()).collect());
    }

    if messages.is_empty() {
        return Ok(());
    }

    // Try batch verification first.
    if batch_verify(messages, signatures, public_keys).is_ok() {
        return Ok(());
    }

    // Batch failed: fall back to individual verification to find bad signatures.
    let mut bad_indices = Vec::new();
    for i in 0..messages.len() {
        if public_keys[i].verify(messages[i], signatures[i]).is_err() {
            bad_indices.push(i);
        }
    }

    if bad_indices.is_empty() {
        Ok(())
    } else {
        Err(bad_indices)
    }
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

    #[test]
    fn test_batch_verify_single() {
        let sk = BlsSecretKey::random().unwrap();
        let pk = sk.public_key();
        let msg = b"single message";
        let sig = sk.sign(msg);

        batch_verify(&[msg.as_ref()], &[&sig], &[&pk])
            .expect("single-element batch should succeed");
    }

    #[test]
    fn test_batch_verify_same_message_different_signers() {
        // Common in consensus: all validators sign the same message.
        let msg = b"view=5||block_hash=0xAA";
        let sks: Vec<_> = (0..10).map(|_| BlsSecretKey::random().unwrap()).collect();
        let pks: Vec<_> = sks.iter().map(|sk| sk.public_key()).collect();
        let sigs: Vec<_> = sks.iter().map(|sk| sk.sign(msg)).collect();

        let messages: Vec<&[u8]> = vec![msg.as_ref(); 10];
        let sig_refs: Vec<_> = sigs.iter().collect();
        let pk_refs: Vec<_> = pks.iter().collect();

        batch_verify(&messages, &sig_refs, &pk_refs)
            .expect("batch verify with same message should succeed");
    }

    #[test]
    fn test_batch_verify_detects_invalid() {
        let sk1 = BlsSecretKey::random().unwrap();
        let sk2 = BlsSecretKey::random().unwrap();
        let sk3 = BlsSecretKey::random().unwrap();

        let pk1 = sk1.public_key();
        let pk2 = sk2.public_key();
        let pk3 = sk3.public_key();

        let msg = b"consensus vote";
        let sig1 = sk1.sign(msg);
        let sig2 = sk2.sign(b"wrong message"); // Invalid!
        let sig3 = sk3.sign(msg);

        let messages: Vec<&[u8]> = vec![msg.as_ref(), msg.as_ref(), msg.as_ref()];
        let result = batch_verify(&messages, &[&sig1, &sig2, &sig3], &[&pk1, &pk2, &pk3]);
        assert!(result.is_err(), "batch should fail when one signature is invalid");
    }

    #[test]
    fn test_batch_verify_with_fallback_all_valid() {
        let msg = b"test message";
        let sks: Vec<_> = (0..5).map(|_| BlsSecretKey::random().unwrap()).collect();
        let pks: Vec<_> = sks.iter().map(|sk| sk.public_key()).collect();
        let sigs: Vec<_> = sks.iter().map(|sk| sk.sign(msg)).collect();

        let messages: Vec<&[u8]> = vec![msg.as_ref(); 5];
        let sig_refs: Vec<_> = sigs.iter().collect();
        let pk_refs: Vec<_> = pks.iter().collect();

        batch_verify_with_fallback(&messages, &sig_refs, &pk_refs)
            .expect("all valid should return Ok");
    }

    #[test]
    fn test_batch_verify_with_fallback_identifies_bad() {
        let msg = b"consensus vote";
        let sks: Vec<_> = (0..5).map(|_| BlsSecretKey::random().unwrap()).collect();
        let pks: Vec<_> = sks.iter().map(|sk| sk.public_key()).collect();

        let mut sigs: Vec<_> = sks.iter().map(|sk| sk.sign(msg)).collect();
        // Corrupt signatures at index 1 and 3
        sigs[1] = sks[1].sign(b"wrong");
        sigs[3] = sks[3].sign(b"also wrong");

        let messages: Vec<&[u8]> = vec![msg.as_ref(); 5];
        let sig_refs: Vec<_> = sigs.iter().collect();
        let pk_refs: Vec<_> = pks.iter().collect();

        let result = batch_verify_with_fallback(&messages, &sig_refs, &pk_refs);
        assert!(result.is_err());
        let bad_indices = result.unwrap_err();
        assert!(bad_indices.contains(&1), "should identify index 1 as bad");
        assert!(bad_indices.contains(&3), "should identify index 3 as bad");
        assert_eq!(bad_indices.len(), 2, "should find exactly 2 bad signatures");
    }
}
