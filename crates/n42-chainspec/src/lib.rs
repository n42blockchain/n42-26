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
