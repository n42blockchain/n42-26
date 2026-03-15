use crate::hasher::Blake3Hasher;
use crate::metrics;

use alloy_primitives::B256;
use jmt::proof::SparseMerkleProof;
use jmt::{KeyHash, RootHash};
use std::time::Instant;
use thiserror::Error;

/// Serializable JMT proof for mobile verification.
///
/// Fields:
/// - `shard_index`: which of the 16 shards this proof targets (0-15)
/// - `shard_roots`: all 16 shard root hashes (needed to recompute combined root)
/// - `proof_bytes`: bincode-serialized `SparseMerkleProof<Blake3Hasher>`
/// - `key`: the 32-byte `KeyHash` being proven
/// - `value`: the leaf value (`None` for exclusion/non-existence proof)
///
/// Verification is self-contained: the mobile client only needs the expected
/// combined root hash (from block header). No tree access required.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct JmtProof {
    pub shard_index: u8,
    pub shard_roots: Vec<[u8; 32]>,
    pub proof_bytes: Vec<u8>,
    pub key: [u8; 32],
    pub value: Option<Vec<u8>>,
}

#[derive(Error, Debug)]
pub enum VerifyError {
    #[error("shard index {0} out of range (max 15)")]
    ShardIndexOutOfRange(u8),
    #[error("expected 16 shard roots, got {0}")]
    WrongShardRootCount(usize),
    #[error("combined root mismatch: expected {expected}, computed {computed}")]
    CombinedRootMismatch { expected: B256, computed: B256 },
    #[error("shard proof verification failed: {0}")]
    ShardProofFailed(String),
    #[error("proof deserialization failed: {0}")]
    DeserializationFailed(String),
}

impl JmtProof {
    /// Verify this proof against a known combined root hash.
    ///
    /// Steps:
    /// 1. Recompute combined_root from `shard_roots` using blake3
    /// 2. Compare with `expected_root`
    /// 3. Deserialize and verify the shard-level SparseMerkleProof
    ///
    /// This is the function mobile clients call — no tree access needed.
    pub fn verify(&self, expected_root: &B256) -> Result<(), VerifyError> {
        let start = Instant::now();

        let result = self.verify_inner(expected_root);

        let ms = start.elapsed().as_secs_f64() * 1000.0;
        metrics::record_proof_verification(result.is_ok(), ms);

        result
    }

    fn verify_inner(&self, expected_root: &B256) -> Result<(), VerifyError> {
        if self.shard_index > 15 {
            return Err(VerifyError::ShardIndexOutOfRange(self.shard_index));
        }
        if self.shard_roots.len() != 16 {
            return Err(VerifyError::WrongShardRootCount(self.shard_roots.len()));
        }

        // Step 1: Verify combined root.
        let mut hasher = blake3::Hasher::new();
        for root in &self.shard_roots {
            hasher.update(root);
        }
        let computed = B256::from(*hasher.finalize().as_bytes());
        if computed != *expected_root {
            return Err(VerifyError::CombinedRootMismatch {
                expected: *expected_root,
                computed,
            });
        }

        // Step 2: Deserialize shard proof.
        let proof: SparseMerkleProof<Blake3Hasher> = bincode::deserialize(&self.proof_bytes)
            .map_err(|e| VerifyError::DeserializationFailed(e.to_string()))?;

        // Step 3: Verify shard proof against shard root.
        let shard_root = RootHash(self.shard_roots[self.shard_index as usize]);
        let key_hash = KeyHash(self.key);
        proof
            .verify(shard_root, key_hash, self.value.as_deref())
            .map_err(|e| VerifyError::ShardProofFailed(e.to_string()))?;

        Ok(())
    }

    /// Estimate the serialized size of this proof in bytes.
    pub fn estimated_size(&self) -> usize {
        // shard_index(1) + 16 shard roots(512) + proof bytes + key(32) + value
        1 + 16 * 32 + self.proof_bytes.len() + 32 + self.value.as_ref().map_or(0, |v| v.len())
    }
}

/// Build a `JmtProof` from a sharded JMT for a given key.
///
/// Collects all shard roots and serializes the shard-level proof.
/// Returns `None` if the target shard is empty (key guaranteed absent, no proof needed).
pub fn build_proof(
    sharded_jmt: &crate::ShardedJmt,
    key_hash: KeyHash,
) -> eyre::Result<Option<JmtProof>> {
    let start = Instant::now();

    let shard_idx = (key_hash.0[0] >> 4) as u8;

    // Generate shard-level proof. None if shard is empty.
    let Some(shard_proof) = sharded_jmt.get_proof(key_hash)? else {
        return Ok(None);
    };

    // Collect all 16 shard roots via public API.
    let shard_roots = sharded_jmt.shard_roots()?;

    let proof_bytes = bincode::serialize(&shard_proof)?;
    let value = sharded_jmt.get(key_hash)?;

    let proof = JmtProof {
        shard_index: shard_idx,
        shard_roots,
        proof_bytes,
        key: key_hash.0,
        value,
    };

    let ms = start.elapsed().as_secs_f64() * 1000.0;
    metrics::record_proof_generation(proof.estimated_size(), ms);

    Ok(Some(proof))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ShardedJmt, account_key};
    use alloy_primitives::{Address, U256};
    use n42_execution::state_diff::{AccountChangeType, AccountDiff, StateDiff, ValueChange};
    use std::collections::BTreeMap;

    fn setup_tree_with_account(addr: Address, balance: u64) -> ShardedJmt {
        let mut jmt = ShardedJmt::new();
        let mut accounts = BTreeMap::new();
        accounts.insert(
            addr,
            AccountDiff {
                change_type: AccountChangeType::Created,
                balance: Some(ValueChange::new(U256::ZERO, U256::from(balance))),
                nonce: Some(ValueChange::new(0, 1)),
                code_change: None,
                storage: BTreeMap::new(),
            },
        );
        jmt.apply_diff(&StateDiff { accounts }).unwrap();
        jmt
    }

    #[test]
    fn build_and_verify_inclusion_proof() {
        let addr = Address::repeat_byte(0xAA);
        let jmt = setup_tree_with_account(addr, 1000);
        let combined_root = jmt.root_hash().unwrap();
        let key = account_key(&addr);

        let proof = build_proof(&jmt, key).unwrap().expect("proof should exist");
        assert_eq!(proof.shard_index, (key.0[0] >> 4) as u8);
        assert!(proof.value.is_some());
        assert_eq!(proof.shard_roots.len(), 16);

        proof.verify(&combined_root).expect("proof should verify");
    }

    #[test]
    fn verify_exclusion_proof() {
        // Create multiple accounts so all/most shards have data.
        let mut jmt = ShardedJmt::new();
        let mut accounts = BTreeMap::new();
        for i in 0..200u8 {
            let addr = Address::with_last_byte(i);
            accounts.insert(
                addr,
                AccountDiff {
                    change_type: AccountChangeType::Created,
                    balance: Some(ValueChange::new(U256::ZERO, U256::from(100))),
                    nonce: Some(ValueChange::new(0, 1)),
                    code_change: None,
                    storage: BTreeMap::new(),
                },
            );
        }
        jmt.apply_diff(&StateDiff { accounts }).unwrap();
        let combined_root = jmt.root_hash().unwrap();

        let missing_addr = Address::repeat_byte(0xFF);
        let key = account_key(&missing_addr);
        let proof = build_proof(&jmt, key)
            .unwrap()
            .expect("shard should have data");
        assert!(proof.value.is_none());

        proof
            .verify(&combined_root)
            .expect("exclusion proof should verify");
    }

    #[test]
    fn verify_fails_with_wrong_root() {
        let addr = Address::repeat_byte(0xAA);
        let jmt = setup_tree_with_account(addr, 1000);
        let key = account_key(&addr);

        let proof = build_proof(&jmt, key).unwrap().expect("proof should exist");
        let wrong_root = B256::repeat_byte(0xFF);

        let err = proof.verify(&wrong_root).unwrap_err();
        assert!(matches!(err, VerifyError::CombinedRootMismatch { .. }));
    }

    #[test]
    fn proof_size_reasonable() {
        let addr = Address::repeat_byte(0xCC);
        let jmt = setup_tree_with_account(addr, 5000);
        let key = account_key(&addr);

        let proof = build_proof(&jmt, key).unwrap().expect("proof should exist");
        let size = proof.estimated_size();
        assert!(size < 2048, "proof too large: {size} bytes");
    }

    #[test]
    fn empty_shard_returns_none() {
        let jmt = ShardedJmt::new();
        let key = account_key(&Address::repeat_byte(0x01));
        let proof = build_proof(&jmt, key).unwrap();
        assert!(proof.is_none(), "empty shard should return None");
    }

    #[test]
    fn verify_bad_shard_index() {
        let proof = JmtProof {
            shard_index: 16, // invalid
            shard_roots: vec![[0u8; 32]; 16],
            proof_bytes: vec![],
            key: [0u8; 32],
            value: None,
        };
        let err = proof.verify(&B256::ZERO).unwrap_err();
        assert!(matches!(err, VerifyError::ShardIndexOutOfRange(16)));
    }

    #[test]
    fn verify_wrong_shard_count() {
        let proof = JmtProof {
            shard_index: 0,
            shard_roots: vec![[0u8; 32]; 8], // wrong count
            proof_bytes: vec![],
            key: [0u8; 32],
            value: None,
        };
        let err = proof.verify(&B256::ZERO).unwrap_err();
        assert!(matches!(err, VerifyError::WrongShardRootCount(8)));
    }
}
