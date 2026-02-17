use alloy_genesis::Genesis;
use alloy_primitives::Address;
use n42_primitives::BlsPublicKey;
use reth_chainspec::{Chain, ChainSpec, ChainSpecBuilder};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// N42 consensus-specific configuration, separate from the EVM chain spec.
/// This holds parameters that are unique to the HotStuff-2 consensus.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConsensusConfig {
    /// Slot duration in milliseconds.
    pub slot_time_ms: u64,
    /// Number of validators in the initial set.
    pub validator_set_size: u32,
    /// Byzantine fault tolerance threshold: f = (validator_set_size - 1) / 3
    pub fault_tolerance: u32,
    /// Base timeout for pacemaker in milliseconds.
    pub base_timeout_ms: u64,
    /// Maximum timeout for pacemaker in milliseconds.
    pub max_timeout_ms: u64,
    /// Initial validator public keys.
    pub initial_validators: Vec<ValidatorInfo>,
    /// Number of views per epoch (0 = epochs disabled, static validator set).
    /// When > 0, validator set can change at epoch boundaries.
    #[serde(default)]
    pub epoch_length: u64,
}

/// Information about a validator in the initial set.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ValidatorInfo {
    /// Validator's execution layer address (for rewards).
    pub address: Address,
    /// Validator's BLS public key (for consensus signing).
    pub bls_public_key: BlsPublicKey,
}

impl ConsensusConfig {
    /// Create a default dev/test configuration (single validator, no preset keys).
    pub fn dev() -> Self {
        Self {
            slot_time_ms: 8000,
            validator_set_size: 1,
            fault_tolerance: 0,
            // Single-node: short timeouts are fine since solo commits are instant.
            base_timeout_ms: 4000,
            max_timeout_ms: 8000,
            initial_validators: Vec::new(),
            epoch_length: 0,
        }
    }

    /// Create a dev/test configuration with `count` deterministic validators.
    ///
    /// Keys are generated deterministically: validator `i` gets a 32-byte secret
    /// key with value `(i+1)` in big-endian at the last 4 bytes. This matches
    /// the key generation in `scripts/local-testnet.sh`.
    pub fn dev_multi(count: usize) -> Self {
        let mut validators = Vec::with_capacity(count);
        for i in 0..count {
            let key_bytes = Self::deterministic_key_bytes(i);
            let sk = n42_primitives::BlsSecretKey::from_bytes(&key_bytes)
                .expect("deterministic BLS key should be valid");
            let pk = sk.public_key();
            validators.push(ValidatorInfo {
                address: Address::with_last_byte((i + 1) as u8),
                bls_public_key: pk,
            });
        }

        let f = if count >= 4 { ((count as u32) - 1) / 3 } else { 0 };

        Self {
            slot_time_ms: 8000,
            validator_set_size: count as u32,
            fault_tolerance: f,
            // Multi-node: timeout must exceed slot_time + build_time + consensus_round.
            // Production pipeline: up to 8s slot delay + 700ms build + 2s consensus = ~11s.
            // 20s base gives comfortable margin; exponential backoff handles failures.
            base_timeout_ms: 20000,
            max_timeout_ms: 60000,
            initial_validators: validators,
            epoch_length: 0,
        }
    }

    /// Generate deterministic 32-byte secret key material for validator at `index`.
    ///
    /// Matches `scripts/local-testnet.sh`: `printf '%056x%08x' 0 "$((index+1))"`.
    pub fn deterministic_key_bytes(index: usize) -> [u8; 32] {
        let mut bytes = [0u8; 32];
        let val = (index + 1) as u32;
        bytes[28..32].copy_from_slice(&val.to_be_bytes());
        bytes
    }

    /// Returns the quorum size (2f+1).
    pub fn quorum_size(&self) -> u32 {
        2 * self.fault_tolerance + 1
    }

    /// Validates that the configuration is internally consistent.
    ///
    /// Checks:
    /// - `validator_set_size >= 3f + 1` (Byzantine fault tolerance requirement)
    /// - `base_timeout_ms <= max_timeout_ms`
    /// - `slot_time_ms > 0`
    pub fn validate(&self) -> Result<(), String> {
        let min_validators = 3 * self.fault_tolerance + 1;
        if self.validator_set_size < min_validators {
            return Err(format!(
                "validator_set_size ({}) must be >= 3f+1 ({}) for fault_tolerance f={}",
                self.validator_set_size, min_validators, self.fault_tolerance
            ));
        }
        if self.base_timeout_ms > self.max_timeout_ms {
            return Err(format!(
                "base_timeout_ms ({}) must be <= max_timeout_ms ({})",
                self.base_timeout_ms, self.max_timeout_ms
            ));
        }
        if self.slot_time_ms == 0 {
            return Err("slot_time_ms must be > 0".to_string());
        }
        Ok(())
    }
}

impl Default for ConsensusConfig {
    fn default() -> Self {
        Self::dev()
    }
}

/// N42 chain ID.
pub const N42_CHAIN_ID: u64 = 4242;

/// Create the N42 dev chain spec (reth ChainSpec) for testing.
/// This enables all Ethereum hardforks up through Cancun to get a modern EVM.
pub fn n42_dev_chainspec() -> Arc<ChainSpec> {
    let genesis = Genesis::default();

    let spec = ChainSpecBuilder::default()
        .chain(Chain::from_id(N42_CHAIN_ID))
        .genesis(genesis)
        .cancun_activated()
        .build();

    Arc::new(spec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_consensus_config_dev() {
        let config = ConsensusConfig::dev();

        assert_eq!(config.slot_time_ms, 8000, "dev slot time should be 8000ms");
        assert_eq!(config.validator_set_size, 1, "dev validator set size should be 1");
        assert_eq!(config.fault_tolerance, 0, "dev fault tolerance should be 0");
        assert_eq!(config.base_timeout_ms, 4000, "dev base timeout should be 4000ms");
        assert_eq!(config.max_timeout_ms, 8000, "dev max timeout should be 8000ms");
        assert!(
            config.initial_validators.is_empty(),
            "dev initial_validators should be empty"
        );
    }

    #[test]
    fn test_consensus_config_quorum_size() {
        // With fault_tolerance = 1, quorum_size = 2*1+1 = 3.
        let mut config = ConsensusConfig::dev();
        config.fault_tolerance = 1;
        assert_eq!(config.quorum_size(), 3, "quorum size should be 2f+1 = 3 when f=1");

        // With fault_tolerance = 0, quorum_size = 1.
        let config0 = ConsensusConfig::dev();
        assert_eq!(config0.quorum_size(), 1, "quorum size should be 1 when f=0");

        // With fault_tolerance = 3, quorum_size = 7.
        let mut config3 = ConsensusConfig::dev();
        config3.fault_tolerance = 3;
        assert_eq!(config3.quorum_size(), 7, "quorum size should be 2f+1 = 7 when f=3");
    }

    #[test]
    fn test_n42_dev_chainspec() {
        let spec = n42_dev_chainspec();
        assert_eq!(
            spec.chain().id(),
            4242,
            "n42 dev chain spec should have chain_id 4242"
        );
    }

    #[test]
    fn test_consensus_config_validate() {
        // Dev config should be valid
        let dev = ConsensusConfig::dev();
        assert!(dev.validate().is_ok(), "dev config should be valid");

        // Invalid: fault_tolerance too high for validator_set_size
        let mut bad = ConsensusConfig::dev();
        bad.validator_set_size = 3;
        bad.fault_tolerance = 1; // needs 3*1+1=4 validators, only have 3
        assert!(bad.validate().is_err(), "should reject f=1 with only 3 validators");

        // Valid: 4 validators, f=1
        bad.validator_set_size = 4;
        assert!(bad.validate().is_ok(), "should accept f=1 with 4 validators");

        // Invalid: base_timeout > max_timeout
        let mut bad_timeout = ConsensusConfig::dev();
        bad_timeout.base_timeout_ms = 10000;
        bad_timeout.max_timeout_ms = 5000;
        assert!(bad_timeout.validate().is_err(), "should reject base > max timeout");

        // Invalid: slot_time_ms = 0
        let mut bad_slot = ConsensusConfig::dev();
        bad_slot.slot_time_ms = 0;
        assert!(bad_slot.validate().is_err(), "should reject zero slot time");
    }

    #[test]
    fn test_consensus_config_dev_multi() {
        let config = ConsensusConfig::dev_multi(3);
        assert_eq!(config.validator_set_size, 3);
        assert_eq!(config.fault_tolerance, 0); // 3 < 4, so f=0
        assert_eq!(config.initial_validators.len(), 3);

        // Keys should be deterministic and distinct.
        let pk0 = config.initial_validators[0].bls_public_key.to_bytes();
        let pk1 = config.initial_validators[1].bls_public_key.to_bytes();
        let pk2 = config.initial_validators[2].bls_public_key.to_bytes();
        assert_ne!(pk0, pk1);
        assert_ne!(pk1, pk2);

        // Addresses should match pattern.
        assert_eq!(config.initial_validators[0].address, Address::with_last_byte(1));
        assert_eq!(config.initial_validators[1].address, Address::with_last_byte(2));
    }

    #[test]
    fn test_consensus_config_dev_multi_4() {
        let config = ConsensusConfig::dev_multi(4);
        assert_eq!(config.fault_tolerance, 1); // (4-1)/3 = 1
        assert_eq!(config.quorum_size(), 3);   // 2*1+1 = 3
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_deterministic_key_bytes() {
        let k0 = ConsensusConfig::deterministic_key_bytes(0);
        assert_eq!(k0[31], 1);
        assert_eq!(k0[30], 0);

        let k1 = ConsensusConfig::deterministic_key_bytes(1);
        assert_eq!(k1[31], 2);

        // Verify roundtrip: key bytes → secret key → public key → sign/verify.
        use n42_primitives::BlsSecretKey;
        let sk = BlsSecretKey::from_bytes(&k0).unwrap();
        let pk = sk.public_key();
        let sig = sk.sign(b"test");
        pk.verify(b"test", &sig).unwrap();
    }

    #[test]
    fn test_consensus_config_default() {
        let default_config = ConsensusConfig::default();
        let dev_config = ConsensusConfig::dev();

        assert_eq!(default_config.slot_time_ms, dev_config.slot_time_ms);
        assert_eq!(default_config.validator_set_size, dev_config.validator_set_size);
        assert_eq!(default_config.fault_tolerance, dev_config.fault_tolerance);
        assert_eq!(default_config.base_timeout_ms, dev_config.base_timeout_ms);
        assert_eq!(default_config.max_timeout_ms, dev_config.max_timeout_ms);
        assert_eq!(
            default_config.initial_validators.len(),
            dev_config.initial_validators.len(),
            "Default and dev should have the same (empty) validator set"
        );
    }
}
