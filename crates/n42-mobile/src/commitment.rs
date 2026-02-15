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
            self.block_number,
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

        // Check block number matches
        if self.block_number != commitment.block_number {
            return Err(CommitmentError::BlockNumberMismatch);
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
/// `commitment = keccak256(block_hash || block_number || state_root_match || receipts_root_match || nonce)`
pub fn compute_commitment(
    block_hash: &B256,
    block_number: u64,
    state_root_match: bool,
    receipts_root_match: bool,
    nonce: &B256,
) -> B256 {
    let mut data = Vec::with_capacity(74); // 32 + 8 + 1 + 1 + 32
    data.extend_from_slice(block_hash.as_slice());
    data.extend_from_slice(&block_number.to_le_bytes());
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

    /// The reveal's block number doesn't match the commitment's.
    #[error("block number mismatch between commitment and reveal")]
    BlockNumberMismatch,

    /// The computed commitment hash doesn't match the committed one.
    #[error("commitment hash mismatch (possible result copying)")]
    HashMismatch,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_commitment_deterministic() {
        let block_hash = B256::from([1u8; 32]);
        let nonce = B256::from([99u8; 32]);

        let hash1 = compute_commitment(&block_hash, 1, true, true, &nonce);
        let hash2 = compute_commitment(&block_hash, 1, true, true, &nonce);

        assert_eq!(hash1, hash2, "same inputs must produce the same commitment hash");
    }

    #[test]
    fn test_compute_commitment_different_nonce() {
        let block_hash = B256::from([1u8; 32]);
        let nonce_a = B256::from([10u8; 32]);
        let nonce_b = B256::from([20u8; 32]);

        let hash_a = compute_commitment(&block_hash, 1, true, true, &nonce_a);
        let hash_b = compute_commitment(&block_hash, 1, true, true, &nonce_b);

        assert_ne!(hash_a, hash_b, "different nonces must produce different commitment hashes");
    }

    /// Helper: builds a matching (commitment, reveal) pair.
    fn make_commitment_reveal_pair(
        block_hash: B256,
        block_number: u64,
        verifier_pubkey: [u8; 32],
        state_root_match: bool,
        receipts_root_match: bool,
        nonce: B256,
    ) -> (VerificationCommitment, VerificationReveal) {
        let commitment_hash = compute_commitment(&block_hash, block_number, state_root_match, receipts_root_match, &nonce);

        let commitment = VerificationCommitment {
            block_hash,
            block_number,
            verifier_pubkey,
            commitment_hash,
            timestamp_ms: 1_000_000,
        };

        let reveal = VerificationReveal {
            block_hash,
            block_number,
            verifier_pubkey,
            state_root_match,
            receipts_root_match,
            nonce,
        };

        (commitment, reveal)
    }

    #[test]
    fn test_verify_against_commitment_success() {
        let block_hash = B256::from([5u8; 32]);
        let pubkey = [42u8; 32];
        let nonce = B256::from([77u8; 32]);

        let (commitment, reveal) = make_commitment_reveal_pair(
            block_hash, 100, pubkey, true, true, nonce,
        );

        // Verification must succeed with matching fields.
        reveal
            .verify_against_commitment(&commitment)
            .expect("matching reveal should pass verification");
    }

    #[test]
    fn test_verify_against_commitment_wrong_nonce() {
        let block_hash = B256::from([5u8; 32]);
        let pubkey = [42u8; 32];
        let nonce = B256::from([77u8; 32]);
        let wrong_nonce = B256::from([88u8; 32]);

        // Build a valid commitment with the correct nonce.
        let (commitment, _) = make_commitment_reveal_pair(
            block_hash, 100, pubkey, true, true, nonce,
        );

        // Build a reveal with the wrong nonce.
        let bad_reveal = VerificationReveal {
            block_hash,
            block_number: 100,
            verifier_pubkey: pubkey,
            state_root_match: true,
            receipts_root_match: true,
            nonce: wrong_nonce,
        };

        let result = bad_reveal.verify_against_commitment(&commitment);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), CommitmentError::HashMismatch));
    }

    #[test]
    fn test_verify_against_commitment_wrong_block() {
        let block_hash = B256::from([5u8; 32]);
        let different_block = B256::from([6u8; 32]);
        let pubkey = [42u8; 32];
        let nonce = B256::from([77u8; 32]);

        let (commitment, _) = make_commitment_reveal_pair(
            block_hash, 100, pubkey, true, true, nonce,
        );

        // Build a reveal referencing a different block hash.
        let bad_reveal = VerificationReveal {
            block_hash: different_block,
            block_number: 100,
            verifier_pubkey: pubkey,
            state_root_match: true,
            receipts_root_match: true,
            nonce,
        };

        let result = bad_reveal.verify_against_commitment(&commitment);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), CommitmentError::BlockMismatch));
    }

    #[test]
    fn test_verify_against_commitment_wrong_verifier() {
        let block_hash = B256::from([5u8; 32]);
        let pubkey = [42u8; 32];
        let other_pubkey = [99u8; 32];
        let nonce = B256::from([77u8; 32]);

        let (commitment, _) = make_commitment_reveal_pair(
            block_hash, 100, pubkey, true, true, nonce,
        );

        // Build a reveal with a different verifier.
        let bad_reveal = VerificationReveal {
            block_hash,
            block_number: 100,
            verifier_pubkey: other_pubkey,
            state_root_match: true,
            receipts_root_match: true,
            nonce,
        };

        let result = bad_reveal.verify_against_commitment(&commitment);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), CommitmentError::VerifierMismatch));
    }
}
