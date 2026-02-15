use alloy_primitives::Address;
use n42_primitives::BlsPublicKey;
use n42_chainspec::ValidatorInfo;
use crate::error::{ConsensusError, ConsensusResult};

/// Manages the active validator set for consensus.
///
/// Validators are indexed by `ValidatorIndex` (u32), which is their position
/// in the ordered list. The set is fixed for an epoch; validator set changes
/// happen at epoch boundaries (not implemented in Phase 3).
#[derive(Debug, Clone)]
pub struct ValidatorSet {
    /// Ordered list of validators.
    validators: Vec<ValidatorEntry>,
    /// Number of Byzantine faults tolerated: f = (n - 1) / 3
    fault_tolerance: u32,
}

/// Internal entry for a validator.
#[derive(Debug, Clone)]
struct ValidatorEntry {
    address: Address,
    public_key: BlsPublicKey,
}

impl ValidatorSet {
    /// Creates a new validator set from chain configuration.
    pub fn new(validators: &[ValidatorInfo], fault_tolerance: u32) -> Self {
        let entries = validators
            .iter()
            .map(|v| ValidatorEntry {
                address: v.address,
                public_key: v.bls_public_key.clone(),
            })
            .collect();

        Self {
            validators: entries,
            fault_tolerance,
        }
    }

    /// Returns the total number of validators.
    pub fn len(&self) -> u32 {
        self.validators.len() as u32
    }

    /// Returns true if the set is empty.
    pub fn is_empty(&self) -> bool {
        self.validators.is_empty()
    }

    /// Returns the quorum size: 2f + 1.
    pub fn quorum_size(&self) -> usize {
        (2 * self.fault_tolerance + 1) as usize
    }

    /// Returns the fault tolerance f.
    pub fn fault_tolerance(&self) -> u32 {
        self.fault_tolerance
    }

    /// Gets the BLS public key for a validator by index.
    pub fn get_public_key(&self, index: u32) -> ConsensusResult<&BlsPublicKey> {
        self.validators
            .get(index as usize)
            .map(|v| &v.public_key)
            .ok_or(ConsensusError::UnknownValidator {
                index,
                set_size: self.len(),
            })
    }

    /// Gets the address for a validator by index.
    pub fn get_address(&self, index: u32) -> ConsensusResult<&Address> {
        self.validators
            .get(index as usize)
            .map(|v| &v.address)
            .ok_or(ConsensusError::UnknownValidator {
                index,
                set_size: self.len(),
            })
    }

    /// Checks if a validator index is valid.
    pub fn contains(&self, index: u32) -> bool {
        (index as usize) < self.validators.len()
    }

    /// Returns all public keys as references, in index order.
    /// Used for QC signature verification.
    pub fn all_public_keys(&self) -> Vec<&BlsPublicKey> {
        self.validators.iter().map(|v| &v.public_key).collect()
    }

    /// Returns public keys for specific indices (matching a signer bitmap).
    /// Used for verifying aggregated signatures against the signing subset.
    pub fn public_keys_for_signers(&self, signer_indices: &[u32]) -> ConsensusResult<Vec<&BlsPublicKey>> {
        signer_indices
            .iter()
            .map(|&idx| self.get_public_key(idx))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use n42_primitives::BlsSecretKey;

    /// Helper: create a ValidatorInfo with a random BLS key and a deterministic address.
    fn make_validator_info(index: u8) -> (BlsSecretKey, ValidatorInfo) {
        let sk = BlsSecretKey::random().unwrap();
        let info = ValidatorInfo {
            address: Address::with_last_byte(index),
            bls_public_key: sk.public_key(),
        };
        (sk, info)
    }

    #[test]
    fn test_validator_set_creation() {
        let infos: Vec<_> = (0..4u8).map(|i| make_validator_info(i).1).collect();
        let f = (4 - 1) / 3; // f = 1
        let vs = ValidatorSet::new(&infos, f);

        assert_eq!(vs.len(), 4, "set should have 4 validators");
        assert_eq!(vs.fault_tolerance(), 1, "fault tolerance should be 1 for n=4");
        assert_eq!(vs.quorum_size(), 3, "quorum size should be 2f+1 = 3");
        assert!(!vs.is_empty(), "set should not be empty");
    }

    #[test]
    fn test_get_public_key() {
        let items: Vec<_> = (0..4u8).map(|i| make_validator_info(i)).collect();
        let infos: Vec<_> = items.iter().map(|(_, info)| info.clone()).collect();
        let vs = ValidatorSet::new(&infos, 1);

        // Valid index returns the correct key
        for (i, (_, info)) in items.iter().enumerate() {
            let pk = vs.get_public_key(i as u32).expect("should succeed for valid index");
            assert_eq!(
                pk.to_bytes(),
                info.bls_public_key.to_bytes(),
                "public key at index {} should match",
                i
            );
        }

        // Invalid index returns error
        let result = vs.get_public_key(4);
        assert!(result.is_err(), "should error for out-of-range index 4");

        let result = vs.get_public_key(100);
        assert!(result.is_err(), "should error for out-of-range index 100");
    }

    #[test]
    fn test_contains() {
        let infos: Vec<_> = (0..4u8).map(|i| make_validator_info(i).1).collect();
        let vs = ValidatorSet::new(&infos, 1);

        assert!(vs.contains(0), "should contain index 0");
        assert!(vs.contains(1), "should contain index 1");
        assert!(vs.contains(2), "should contain index 2");
        assert!(vs.contains(3), "should contain index 3");
        assert!(!vs.contains(4), "should not contain index 4");
        assert!(!vs.contains(100), "should not contain index 100");
    }

    #[test]
    fn test_empty_set() {
        let vs = ValidatorSet::new(&[], 0);

        assert_eq!(vs.len(), 0, "empty set should have length 0");
        assert!(vs.is_empty(), "empty set should be empty");
        assert!(!vs.contains(0), "empty set should not contain index 0");
    }

    #[test]
    fn test_get_address() {
        let infos: Vec<_> = (0..3u8).map(|i| make_validator_info(i).1).collect();
        let vs = ValidatorSet::new(&infos, 0);

        for i in 0..3u8 {
            let addr = vs.get_address(i as u32).expect("should succeed for valid index");
            assert_eq!(
                *addr,
                Address::with_last_byte(i),
                "address at index {} should match",
                i
            );
        }

        let result = vs.get_address(3);
        assert!(result.is_err(), "should error for out-of-range index");
    }

    #[test]
    fn test_all_public_keys() {
        let items: Vec<_> = (0..3u8).map(|i| make_validator_info(i)).collect();
        let infos: Vec<_> = items.iter().map(|(_, info)| info.clone()).collect();
        let vs = ValidatorSet::new(&infos, 0);

        let all_pks = vs.all_public_keys();
        assert_eq!(all_pks.len(), 3, "should return 3 public keys");
        for (i, pk) in all_pks.iter().enumerate() {
            assert_eq!(
                pk.to_bytes(),
                items[i].1.bls_public_key.to_bytes(),
                "public key at position {} should match",
                i
            );
        }
    }

    #[test]
    fn test_public_keys_for_signers() {
        let items: Vec<_> = (0..4u8).map(|i| make_validator_info(i)).collect();
        let infos: Vec<_> = items.iter().map(|(_, info)| info.clone()).collect();
        let vs = ValidatorSet::new(&infos, 1);

        // Request keys for valid indices
        let pks = vs.public_keys_for_signers(&[0, 2]).expect("should succeed");
        assert_eq!(pks.len(), 2);
        assert_eq!(pks[0].to_bytes(), items[0].1.bls_public_key.to_bytes());
        assert_eq!(pks[1].to_bytes(), items[2].1.bls_public_key.to_bytes());

        // Request with invalid index
        let result = vs.public_keys_for_signers(&[0, 10]);
        assert!(result.is_err(), "should error when a signer index is out of range");
    }
}
