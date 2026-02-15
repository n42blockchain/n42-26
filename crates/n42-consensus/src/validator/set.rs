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
