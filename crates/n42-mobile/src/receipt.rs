use alloy_primitives::B256;
use n42_primitives::{BlsPublicKey, BlsSecretKey, BlsSignature};
use serde::{Deserialize, Serialize};

use crate::serde_helpers::pubkey_48;

/// Verification receipt returned from a mobile device to the IDC node.
///
/// After re-executing a block using the verification packet, the phone
/// produces this receipt indicating whether the execution result matches
/// the expected state root and receipts root.
///
/// Receipts use BLS12-381 signatures for aggregation compatibility.
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
    /// BLS12-381 public key of the verifier (48 bytes).
    #[serde(with = "pubkey_48")]
    pub verifier_pubkey: [u8; 48],
    /// BLS12-381 signature over the receipt content.
    pub signature: BlsSignature,
    /// Timestamp when verification completed (milliseconds since epoch).
    pub timestamp_ms: u64,
}

/// Builds the canonical signing message from receipt fields.
///
/// Format: block_hash (32B) || block_number (8B LE) ||
///         state_root_match (1B) || receipts_root_match (1B) ||
///         timestamp_ms (8B LE)
///
/// This is the single source of truth for the receipt signing format,
/// used by both `sign_receipt()` and `VerificationReceipt::verify_signature()`.
fn build_signing_message(
    block_hash: &B256,
    block_number: u64,
    state_root_match: bool,
    receipts_root_match: bool,
    timestamp_ms: u64,
) -> Vec<u8> {
    let mut msg = Vec::with_capacity(50);
    msg.extend_from_slice(block_hash.as_slice());
    msg.extend_from_slice(&block_number.to_le_bytes());
    msg.push(state_root_match as u8);
    msg.push(receipts_root_match as u8);
    msg.extend_from_slice(&timestamp_ms.to_le_bytes());
    msg
}

impl VerificationReceipt {
    /// Returns true if the receipt indicates successful verification
    /// (both state root and receipts root match).
    pub fn is_valid(&self) -> bool {
        self.state_root_match && self.receipts_root_match
    }

    /// Constructs the signing message for this receipt.
    pub fn signing_message(&self) -> Vec<u8> {
        build_signing_message(
            &self.block_hash,
            self.block_number,
            self.state_root_match,
            self.receipts_root_match,
            self.timestamp_ms,
        )
    }

    /// Verifies the BLS12-381 signature on this receipt.
    pub fn verify_signature(&self) -> Result<(), ReceiptError> {
        let pubkey = BlsPublicKey::from_bytes(&self.verifier_pubkey)
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
    signing_key: &BlsSecretKey,
) -> VerificationReceipt {
    let verifier_pubkey = signing_key.public_key().to_bytes();
    let msg = build_signing_message(
        &block_hash, block_number, state_root_match, receipts_root_match, timestamp_ms,
    );
    let signature = signing_key.sign(&msg);

    VerificationReceipt {
        block_hash,
        block_number,
        state_root_match,
        receipts_root_match,
        verifier_pubkey,
        signature,
        timestamp_ms,
    }
}

/// Receipt verification errors.
#[derive(Debug, thiserror::Error)]
pub enum ReceiptError {
    /// The BLS12-381 public key is malformed.
    #[error("invalid BLS12-381 public key")]
    InvalidPublicKey,

    /// The BLS12-381 signature verification failed.
    #[error("invalid BLS12-381 signature")]
    InvalidSignature,
}

#[cfg(test)]
mod tests {
    use super::*;
    use n42_primitives::BlsSecretKey;

    /// Helper: creates a signed receipt with the given parameters.
    fn make_receipt(
        block_hash: B256,
        block_number: u64,
        state_root_match: bool,
        receipts_root_match: bool,
        timestamp_ms: u64,
        sk: &BlsSecretKey,
    ) -> VerificationReceipt {
        sign_receipt(block_hash, block_number, state_root_match, receipts_root_match, timestamp_ms, sk)
    }

    #[test]
    fn test_sign_and_verify() {
        let sk = BlsSecretKey::key_gen(&[42u8; 32]).unwrap();
        let block_hash = B256::from([1u8; 32]);
        let receipt = make_receipt(block_hash, 100, true, true, 1_700_000_000_000, &sk);

        // The verifier pubkey should match the signing key's public key.
        assert_eq!(receipt.verifier_pubkey, sk.public_key().to_bytes());
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
        let sk = BlsSecretKey::key_gen(&[42u8; 32]).unwrap();
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
        let sk = BlsSecretKey::key_gen(&[42u8; 32]).unwrap();
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
        let sk = BlsSecretKey::key_gen(&[42u8; 32]).unwrap();
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
