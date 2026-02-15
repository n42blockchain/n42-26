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
