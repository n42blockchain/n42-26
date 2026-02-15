use alloy_primitives::B256;
use ed25519_dalek::{Signature, SigningKey, VerifyingKey, Signer, Verifier};
use serde::{Deserialize, Serialize};

/// Verification receipt returned from a mobile device to the IDC node.
///
/// After re-executing a block using the verification packet, the phone
/// produces this receipt indicating whether the execution result matches
/// the expected state root and receipts root.
///
/// Receipts use Ed25519 signatures (faster than BLS on mobile hardware).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationReceipt {
    /// Hash of the verified block.
    pub block_hash: B256,
    /// Block number.
    pub block_number: u64,
    /// Whether the computed state root matches the expected one.
    pub state_root_match: bool,
    /// Whether the computed receipts root matches the expected one.
    pub receipts_root_match: bool,
    /// Ed25519 public key of the verifier (32 bytes).
    pub verifier_pubkey: [u8; 32],
    /// Ed25519 signature over the receipt content.
    pub signature: Signature,
    /// Timestamp when verification completed (milliseconds since epoch).
    pub timestamp_ms: u64,
}

impl VerificationReceipt {
    /// Returns true if the receipt indicates successful verification
    /// (both state root and receipts root match).
    pub fn is_valid(&self) -> bool {
        self.state_root_match && self.receipts_root_match
    }

    /// Constructs the signing message for this receipt.
    ///
    /// Format: block_hash (32B) || block_number (8B LE) ||
    ///         state_root_match (1B) || receipts_root_match (1B) ||
    ///         timestamp_ms (8B LE)
    pub fn signing_message(&self) -> Vec<u8> {
        let mut msg = Vec::with_capacity(50);
        msg.extend_from_slice(self.block_hash.as_slice());
        msg.extend_from_slice(&self.block_number.to_le_bytes());
        msg.push(self.state_root_match as u8);
        msg.push(self.receipts_root_match as u8);
        msg.extend_from_slice(&self.timestamp_ms.to_le_bytes());
        msg
    }

    /// Verifies the Ed25519 signature on this receipt.
    pub fn verify_signature(&self) -> Result<(), ReceiptError> {
        let pubkey = VerifyingKey::from_bytes(&self.verifier_pubkey)
            .map_err(|_| ReceiptError::InvalidPublicKey)?;
        let msg = self.signing_message();
        pubkey
            .verify(&msg, &self.signature)
            .map_err(|_| ReceiptError::InvalidSignature)
    }
}

/// Creates a signed verification receipt.
///
/// Called by the mobile verifier after executing the block.
pub fn sign_receipt(
    block_hash: B256,
    block_number: u64,
    state_root_match: bool,
    receipts_root_match: bool,
    timestamp_ms: u64,
    signing_key: &SigningKey,
) -> VerificationReceipt {
    let verifier_pubkey = signing_key.verifying_key().to_bytes();

    // Build the receipt (without signature) to compute signing message
    let mut receipt = VerificationReceipt {
        block_hash,
        block_number,
        state_root_match,
        receipts_root_match,
        verifier_pubkey,
        signature: Signature::from_bytes(&[0u8; 64]),
        timestamp_ms,
    };

    let msg = receipt.signing_message();
    let sig = signing_key.sign(&msg);
    receipt.signature = sig;

    receipt
}

/// Receipt verification errors.
#[derive(Debug, thiserror::Error)]
pub enum ReceiptError {
    /// The Ed25519 public key is malformed.
    #[error("invalid Ed25519 public key")]
    InvalidPublicKey,

    /// The Ed25519 signature verification failed.
    #[error("invalid Ed25519 signature")]
    InvalidSignature,
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    /// Helper: creates a signed receipt with the given parameters.
    fn make_receipt(
        block_hash: B256,
        block_number: u64,
        state_root_match: bool,
        receipts_root_match: bool,
        timestamp_ms: u64,
        sk: &SigningKey,
    ) -> VerificationReceipt {
        sign_receipt(block_hash, block_number, state_root_match, receipts_root_match, timestamp_ms, sk)
    }

    #[test]
    fn test_sign_and_verify() {
        let sk = SigningKey::from_bytes(&[42u8; 32]);
        let block_hash = B256::from([1u8; 32]);
        let receipt = make_receipt(block_hash, 100, true, true, 1_700_000_000_000, &sk);

        // The verifier pubkey should match the signing key's verifying key.
        assert_eq!(receipt.verifier_pubkey, sk.verifying_key().to_bytes());
        assert_eq!(receipt.block_hash, block_hash);
        assert_eq!(receipt.block_number, 100);
        assert!(receipt.state_root_match);
        assert!(receipt.receipts_root_match);
        assert_eq!(receipt.timestamp_ms, 1_700_000_000_000);

        // Signature verification must succeed.
        receipt.verify_signature().expect("signature should be valid");
    }

    #[test]
    fn test_verify_tampered_receipt() {
        let sk = SigningKey::from_bytes(&[42u8; 32]);
        let block_hash = B256::from([1u8; 32]);
        let mut receipt = make_receipt(block_hash, 100, true, true, 1_700_000_000_000, &sk);

        // Tamper with the block number after signing.
        receipt.block_number = 999;

        // Verification must fail because the signing message changed.
        let result = receipt.verify_signature();
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ReceiptError::InvalidSignature));
    }

    #[test]
    fn test_is_valid() {
        let sk = SigningKey::from_bytes(&[42u8; 32]);
        let block_hash = B256::from([2u8; 32]);

        // Both true → is_valid returns true.
        let receipt = make_receipt(block_hash, 1, true, true, 0, &sk);
        assert!(receipt.is_valid());

        // state_root_match false → is_valid returns false.
        let receipt = make_receipt(block_hash, 1, false, true, 0, &sk);
        assert!(!receipt.is_valid());

        // receipts_root_match false → is_valid returns false.
        let receipt = make_receipt(block_hash, 1, true, false, 0, &sk);
        assert!(!receipt.is_valid());

        // Both false → is_valid returns false.
        let receipt = make_receipt(block_hash, 1, false, false, 0, &sk);
        assert!(!receipt.is_valid());
    }

    #[test]
    fn test_signing_message_deterministic() {
        let sk = SigningKey::from_bytes(&[42u8; 32]);
        let block_hash = B256::from([3u8; 32]);

        let receipt1 = make_receipt(block_hash, 50, true, false, 12345, &sk);
        let receipt2 = make_receipt(block_hash, 50, true, false, 12345, &sk);

        // Same fields must produce the same signing message.
        assert_eq!(receipt1.signing_message(), receipt2.signing_message());

        // Different fields must produce a different signing message.
        let receipt3 = make_receipt(block_hash, 51, true, false, 12345, &sk);
        assert_ne!(receipt1.signing_message(), receipt3.signing_message());

        // Verify the signing message has the expected length:
        // 32 (block_hash) + 8 (block_number) + 1 (state_root_match) + 1 (receipts_root_match) + 8 (timestamp_ms) = 50
        assert_eq!(receipt1.signing_message().len(), 50);
    }
}
