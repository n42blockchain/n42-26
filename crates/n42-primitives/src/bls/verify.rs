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
