use alloy_primitives::{keccak256, B256};
use serde::{Deserialize, Serialize};

/// Commitment-first verification protocol.
///
/// Prevents mobile devices from copying verification results from peers:
///
/// 1. **Commit phase**: Phone computes result, sends `VerificationCommitment`
///    containing `commitment_hash = keccak256(block_hash || result || nonce)`
/// 2. **Reveal phase**: Phone sends `VerificationReveal` with actual result + nonce
/// 3. **Verify**: IDC checks that `keccak256(block_hash || result || nonce) == commitment_hash`
///
/// A phone that copies another phone's result cannot produce a matching
/// commitment because it doesn't know the original nonce ahead of time.

/// Phase 1: Commitment sent by the phone before revealing the result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationCommitment {
    /// Block being verified.
    pub block_hash: B256,
    /// Block number.
    pub block_number: u64,
    /// Ed25519 public key of the verifier.
    pub verifier_pubkey: [u8; 32],
    /// keccak256(block_hash || state_root_match || receipts_root_match || nonce)
    pub commitment_hash: B256,
    /// Timestamp when commitment was made (ms since epoch).
    pub timestamp_ms: u64,
}

/// Phase 2: Reveal sent by the phone after the commitment window closes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationReveal {
    /// Block being verified (must match commitment).
    pub block_hash: B256,
    /// Block number.
    pub block_number: u64,
    /// Ed25519 public key (must match commitment).
    pub verifier_pubkey: [u8; 32],
    /// Actual verification result: state root matches.
    pub state_root_match: bool,
    /// Actual verification result: receipts root matches.
    pub receipts_root_match: bool,
    /// Random nonce used in the commitment (32 bytes).
    pub nonce: B256,
}

impl VerificationReveal {
    /// Computes the commitment hash from the reveal data.
    ///
    /// This should match the `commitment_hash` in the corresponding
    /// `VerificationCommitment`.
    pub fn compute_commitment_hash(&self) -> B256 {
        compute_commitment(
            &self.block_hash,
            self.state_root_match,
            self.receipts_root_match,
            &self.nonce,
        )
    }

    /// Verifies that this reveal matches the given commitment.
    pub fn verify_against_commitment(
        &self,
        commitment: &VerificationCommitment,
    ) -> Result<(), CommitmentError> {
        // Check block hash matches
        if self.block_hash != commitment.block_hash {
            return Err(CommitmentError::BlockMismatch);
        }

        // Check verifier matches
        if self.verifier_pubkey != commitment.verifier_pubkey {
            return Err(CommitmentError::VerifierMismatch);
        }

        // Check commitment hash matches
        let expected_hash = self.compute_commitment_hash();
        if expected_hash != commitment.commitment_hash {
            return Err(CommitmentError::HashMismatch);
        }

        Ok(())
    }
}

/// Computes a commitment hash from the verification result and nonce.
///
/// `commitment = keccak256(block_hash || state_root_match || receipts_root_match || nonce)`
pub fn compute_commitment(
    block_hash: &B256,
    state_root_match: bool,
    receipts_root_match: bool,
    nonce: &B256,
) -> B256 {
    let mut data = Vec::with_capacity(66);
    data.extend_from_slice(block_hash.as_slice());
    data.push(state_root_match as u8);
    data.push(receipts_root_match as u8);
    data.extend_from_slice(nonce.as_slice());
    keccak256(&data)
}

/// Commitment protocol errors.
#[derive(Debug, thiserror::Error)]
pub enum CommitmentError {
    /// The reveal's block hash doesn't match the commitment's.
    #[error("block hash mismatch between commitment and reveal")]
    BlockMismatch,

    /// The reveal's verifier doesn't match the commitment's.
    #[error("verifier public key mismatch")]
    VerifierMismatch,

    /// The computed commitment hash doesn't match the committed one.
    #[error("commitment hash mismatch (possible result copying)")]
    HashMismatch,
}
