//! SBMT state-proof verification for mobile light clients.
//!
//! A phone can verify a single account/storage entry against a block's combined
//! state root **without re-executing the block or accessing any tree** — it only
//! runs blake3 over the proof. Built on the zero-dependency [`n42_bmt_core`]
//! crate, so this pulls no reth/mdbx storage stack into the mobile binary / FFI.

use alloy_primitives::B256;

pub use n42_bmt_core::{BmtVerifyError, ShardedBmtProof};

/// Verify an SBMT account/storage proof against a block's combined state root.
///
/// The proof carries the shard root + shard-tree path, the in-shard binary proof,
/// and the raw value; verification recomputes the combined root and checks the
/// value commitment. Returns `Ok(())` iff the proof is valid for `state_root`.
pub fn verify_state_proof(
    proof: &ShardedBmtProof,
    state_root: B256,
) -> Result<(), BmtVerifyError> {
    proof.verify(&state_root.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use n42_bmt_core::{EMPTY_HASH, Sbmt, ShardedBmtProof, shard_tree_path, shard_tree_root};

    /// Build a sharded proof by hand from the core primitives (mobile has no
    /// access to the `ShardedSbmt` builder, which lives in `n42-jmt`).
    fn build_single_shard_proof(key: [u8; 32], value: &[u8]) -> (ShardedBmtProof, B256) {
        let si = (key[0] >> 4) as usize;
        let mut shard = Sbmt::new();
        shard.insert(key, value);

        let mut roots = vec![EMPTY_HASH; 16];
        roots[si] = shard.root_hash();
        let combined = shard_tree_root(&roots);

        let proof = ShardedBmtProof {
            shard_index: si as u8,
            shard_root: roots[si],
            shard_path: shard_tree_path(&roots, si),
            inner: shard.prove(key),
            value: Some(value.to_vec()),
        };
        (proof, B256::from(combined))
    }

    #[test]
    fn verifies_against_correct_root() {
        let (proof, root) = build_single_shard_proof([0x05; 32], b"account-value");
        assert!(verify_state_proof(&proof, root).is_ok());
    }

    #[test]
    fn rejects_wrong_root() {
        let (proof, _) = build_single_shard_proof([0x05; 32], b"account-value");
        let wrong = B256::from([0xFF; 32]);
        assert!(verify_state_proof(&proof, wrong).is_err());
    }

    #[test]
    fn rejects_tampered_value() {
        let (mut proof, root) = build_single_shard_proof([0x05; 32], b"account-value");
        proof.value = Some(b"forged-value".to_vec());
        assert!(verify_state_proof(&proof, root).is_err());
    }
}
