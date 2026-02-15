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
    /// Create a default dev/test configuration.
    pub fn dev() -> Self {
        Self {
            slot_time_ms: 8000,
            validator_set_size: 1,
            fault_tolerance: 0,
            base_timeout_ms: 4000,
            max_timeout_ms: 8000,
            initial_validators: Vec::new(),
        }
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
