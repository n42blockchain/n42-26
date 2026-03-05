use alloy_genesis::{Genesis, GenesisAccount};
use alloy_primitives::{address, Address, U256};
use n42_primitives::BlsPublicKey;
use reth_chainspec::{Chain, ChainSpec, ChainSpecBuilder};
use serde::{Deserialize, Serialize};
use std::path::Path;
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
    /// Load consensus configuration from a TOML or JSON file.
    /// Format is determined by file extension (`.toml` or else JSON).
    /// The configuration is validated before returning.
    pub fn from_file(path: &std::path::Path) -> Result<Self, String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read config {}: {e}", path.display()))?;
        let config: Self = if path.extension().is_some_and(|ext| ext == "toml") {
            toml::from_str(&content)
                .map_err(|e| format!("TOML parse error in {}: {e}", path.display()))?
        } else {
            serde_json::from_str(&content)
                .map_err(|e| format!("JSON parse error in {}: {e}", path.display()))?
        };
        config.validate()?;
        Ok(config)
    }

    /// Create a default dev/test configuration (single validator, no preset keys).
    pub fn dev() -> Self {
        Self {
            slot_time_ms: 8000,
            validator_set_size: 1,
            fault_tolerance: 0,
            base_timeout_ms: 4000,
            max_timeout_ms: 8000,
            initial_validators: Vec::new(),
            epoch_length: 0,
        }
    }

    /// Create a dev/test configuration with `count` deterministic validators.
    /// Keys are deterministic: validator `i` uses bytes `[0; 28] + (i+1).to_be_bytes()`.
    /// This matches `scripts/local-testnet.sh`.
    ///
    /// # Panics
    /// Panics if `count > 65535` (exceeds practical validator set limits).
    pub fn dev_multi(count: usize) -> Self {
        assert!(count <= 65535, "dev_multi: count {count} exceeds max 65535");
        let mut validators = Vec::with_capacity(count);
        for i in 0..count {
            let key_bytes = Self::deterministic_key_bytes(i);
            let sk = n42_primitives::BlsSecretKey::from_bytes(&key_bytes)
                .expect("deterministic BLS key should be valid");
            let pk = sk.public_key();
            // Use last 2 bytes for address to support up to 65535 validators.
            let mut addr_bytes = [0u8; 20];
            let val = (i + 1) as u16;
            addr_bytes[18..20].copy_from_slice(&val.to_be_bytes());
            validators.push(ValidatorInfo {
                address: Address::from(addr_bytes),
                bls_public_key: pk,
            });
        }

        let f = if count >= 4 { ((count as u32) - 1) / 3 } else { 0 };

        Self {
            slot_time_ms: 8000,
            validator_set_size: count as u32,
            fault_tolerance: f,
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
    /// - `initial_validators.len()` matches `validator_set_size` (if validators are provided)
    /// - `base_timeout_ms <= max_timeout_ms`
    /// - `base_timeout_ms > 0` and `max_timeout_ms > 0`
    /// - `slot_time_ms > 0`
    /// - No duplicate validator addresses or BLS public keys
    pub fn validate(&self) -> Result<(), String> {
        let min_validators = 3 * self.fault_tolerance + 1;
        if self.validator_set_size < min_validators {
            return Err(format!(
                "validator_set_size ({}) must be >= 3f+1 ({}) for fault_tolerance f={}",
                self.validator_set_size, min_validators, self.fault_tolerance
            ));
        }
        // If validators are provided, their count must match validator_set_size.
        if !self.initial_validators.is_empty()
            && self.initial_validators.len() as u32 != self.validator_set_size
        {
            return Err(format!(
                "initial_validators count ({}) does not match validator_set_size ({})",
                self.initial_validators.len(),
                self.validator_set_size
            ));
        }
        if self.base_timeout_ms > self.max_timeout_ms {
            return Err(format!(
                "base_timeout_ms ({}) must be <= max_timeout_ms ({})",
                self.base_timeout_ms, self.max_timeout_ms
            ));
        }
        if self.base_timeout_ms == 0 {
            return Err("base_timeout_ms must be > 0".to_string());
        }
        if self.max_timeout_ms == 0 {
            return Err("max_timeout_ms must be > 0".to_string());
        }
        if self.slot_time_ms == 0 {
            return Err("slot_time_ms must be > 0".to_string());
        }
        // Check for duplicate validator addresses.
        if !self.initial_validators.is_empty() {
            let mut seen_addrs = std::collections::HashSet::new();
            let mut seen_pks = std::collections::HashSet::new();
            for v in &self.initial_validators {
                if !seen_addrs.insert(v.address) {
                    return Err(format!("duplicate validator address: {:?}", v.address));
                }
                if !seen_pks.insert(v.bls_public_key.to_bytes()) {
                    return Err("duplicate BLS public key in validator set".to_string());
                }
            }
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

/// Returns the effective chain ID, allowing override via `N42_CHAIN_ID` environment variable.
///
/// This enables testnet/devnet deployments to use a different chain ID without recompiling.
/// The constant `N42_CHAIN_ID = 4242` is used as the default.
///
/// # Example
/// ```bash
/// N42_CHAIN_ID=1337 ./n42-node
/// ```
fn effective_chain_id() -> u64 {
    match std::env::var("N42_CHAIN_ID") {
        Ok(s) => match s.parse::<u64>() {
            Ok(id) => id,
            Err(_) => {
                eprintln!(
                    "WARNING: N42_CHAIN_ID env var '{}' is not a valid u64, using default {}",
                    s, N42_CHAIN_ID
                );
                N42_CHAIN_ID
            }
        },
        Err(_) => N42_CHAIN_ID,
    }
}

/// Create the N42 dev chain spec (reth ChainSpec) for testing.
/// This enables all Ethereum hardforks up through Cancun to get a modern EVM.
pub fn n42_dev_chainspec() -> Arc<ChainSpec> {
    let genesis = Genesis::default();

    let spec = ChainSpecBuilder::default()
        .chain(Chain::from_id(effective_chain_id()))
        .genesis(genesis)
        .cancun_activated()
        .build();

    Arc::new(spec)
}

/// 10,000 N in wei.
fn ten_thousand_n() -> U256 {
    U256::from(10_000) * U256::from(10).pow(U256::from(18))
}

/// Treasury address for initial token distribution.
/// Derived from a BIP-39 mnemonic via BIP-44 path m/44'/60'/0'/0/0.
/// The mnemonic and private key are stored securely outside the codebase.
pub const TREASURY_ADDRESS: Address = address!("8e182397c01d36E43c31e81BA52eE6480C80C8C2");

/// 3 billion N in wei (3,000,000,000 * 10^18).
fn three_billion_n() -> U256 {
    U256::from(3_000_000_000u64) * U256::from(10).pow(U256::from(18))
}

/// Create a dev chain spec with pre-funded allocations.
///
/// Each validator address receives 10,000 N. Additionally, 10 test accounts
/// (addresses 0x10..0x19) each receive 10,000 N for transaction testing.
/// The treasury address receives 3 billion N for staker distribution and testing.
pub fn n42_dev_chainspec_with_alloc(validators: &[ValidatorInfo]) -> Arc<ChainSpec> {
    let mut alloc = std::collections::BTreeMap::new();
    let balance = ten_thousand_n();

    // Fund validator addresses.
    for v in validators {
        alloc.insert(v.address, GenesisAccount {
            balance,
            ..Default::default()
        });
    }

    // Fund 10 test accounts (0x10..0x19).
    for i in 0..10u8 {
        let addr = Address::with_last_byte(0x10 + i);
        alloc.insert(addr, GenesisAccount {
            balance,
            ..Default::default()
        });
    }

    // Fund treasury with 3 billion N for staker distribution.
    alloc.insert(TREASURY_ADDRESS, GenesisAccount {
        balance: three_billion_n(),
        ..Default::default()
    });

    let genesis = Genesis { alloc, ..Default::default() };
    Arc::new(
        ChainSpecBuilder::default()
            .chain(Chain::from_id(effective_chain_id()))
            .genesis(genesis)
            .cancun_activated()
            .build(),
    )
}

/// Load a chain spec from a genesis JSON file (production use).
///
/// The file should contain a standard Ethereum-style genesis JSON with
/// `alloc`, `config`, etc. fields.
pub fn n42_chainspec_from_genesis(genesis_path: &Path) -> Result<Arc<ChainSpec>, String> {
    let content = std::fs::read_to_string(genesis_path)
        .map_err(|e| format!("read genesis {}: {e}", genesis_path.display()))?;
    let genesis: Genesis = serde_json::from_str(&content)
        .map_err(|e| format!("parse genesis {}: {e}", genesis_path.display()))?;
    Ok(Arc::new(
        ChainSpecBuilder::default()
            .chain(Chain::from_id(effective_chain_id()))
            .genesis(genesis)
            .cancun_activated()
            .build(),
    ))
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
    fn test_consensus_config_from_json_file() {
        use std::io::Write;

        let config = ConsensusConfig::dev_multi(4);
        let json = serde_json::to_string_pretty(&config).unwrap();

        let dir = std::env::temp_dir().join("n42_test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test_config.json");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(json.as_bytes()).unwrap();

        let loaded = ConsensusConfig::from_file(&path).expect("should load JSON config");
        assert_eq!(loaded.validator_set_size, 4);
        assert_eq!(loaded.fault_tolerance, 1);
        assert_eq!(loaded.initial_validators.len(), 4);
        assert!(loaded.validate().is_ok());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_consensus_config_from_toml_file() {
        use std::io::Write;

        let toml_content = r#"
slot_time_ms = 8000
validator_set_size = 1
fault_tolerance = 0
base_timeout_ms = 4000
max_timeout_ms = 8000
initial_validators = []
epoch_length = 0
"#;
        let dir = std::env::temp_dir().join("n42_test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test_config.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(toml_content.as_bytes()).unwrap();

        let loaded = ConsensusConfig::from_file(&path).expect("should load TOML config");
        assert_eq!(loaded.slot_time_ms, 8000);
        assert_eq!(loaded.validator_set_size, 1);
        assert!(loaded.validate().is_ok());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_consensus_config_from_file_nonexistent() {
        let result = ConsensusConfig::from_file(std::path::Path::new("/nonexistent/path.json"));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("failed to read config"));
    }

    #[test]
    fn test_consensus_config_from_file_invalid_json() {
        use std::io::Write;

        let dir = std::env::temp_dir().join("n42_test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("bad_config.json");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(b"not valid json {{{").unwrap();

        let result = ConsensusConfig::from_file(&path);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("JSON parse error"));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_consensus_config_from_file_validation_fails() {
        use std::io::Write;

        // Create a config where slot_time_ms = 0 (invalid)
        let json = r#"{
            "slot_time_ms": 0,
            "validator_set_size": 1,
            "fault_tolerance": 0,
            "base_timeout_ms": 4000,
            "max_timeout_ms": 8000,
            "initial_validators": []
        }"#;
        let dir = std::env::temp_dir().join("n42_test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("invalid_config.json");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(json.as_bytes()).unwrap();

        let result = ConsensusConfig::from_file(&path);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("slot_time_ms"));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_dev_chainspec_with_alloc() {
        let config = ConsensusConfig::dev_multi(3);
        let spec = n42_dev_chainspec_with_alloc(&config.initial_validators);
        assert_eq!(spec.chain().id(), N42_CHAIN_ID);

        let genesis = &spec.genesis;
        // Validator addresses should be funded.
        for v in &config.initial_validators {
            let account = genesis.alloc.get(&v.address)
                .expect("validator should have allocation");
            assert!(account.balance > U256::ZERO, "validator balance should be non-zero");
        }
        // Test accounts should be funded.
        for i in 0..10u8 {
            let addr = Address::with_last_byte(0x10 + i);
            let account = genesis.alloc.get(&addr)
                .expect("test account should have allocation");
            assert_eq!(account.balance, ten_thousand_n());
        }
    }

    #[test]
    fn test_dev_chainspec_with_alloc_empty_validators() {
        let spec = n42_dev_chainspec_with_alloc(&[]);
        // Should have 10 test accounts + 1 treasury.
        assert_eq!(spec.genesis.alloc.len(), 11);
    }

    #[test]
    fn test_chainspec_from_genesis_file() {
        use std::io::Write;

        let genesis_json = r#"{
            "alloc": {
                "0x0000000000000000000000000000000000000001": {
                    "balance": "0xDE0B6B3A7640000"
                }
            }
        }"#;

        let dir = std::env::temp_dir().join("n42_test_genesis");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("genesis.json");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(genesis_json.as_bytes()).unwrap();

        let spec = n42_chainspec_from_genesis(&path).expect("should load genesis");
        assert_eq!(spec.chain().id(), N42_CHAIN_ID);
        assert!(!spec.genesis.alloc.is_empty());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_chainspec_from_genesis_nonexistent() {
        let result = n42_chainspec_from_genesis(std::path::Path::new("/nonexistent/genesis.json"));
        assert!(result.is_err());
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

    #[test]
    fn test_from_file_invalid_toml() {
        use std::io::Write;

        let dir = std::env::temp_dir().join("n42_test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("bad_config.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(b"not = [valid toml {{{{").unwrap();

        let result = ConsensusConfig::from_file(&path);
        assert!(result.is_err(), "invalid TOML should fail");
        assert!(
            result.unwrap_err().contains("TOML parse error"),
            "error should mention TOML parse error"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_dev_multi_zero_requires_validation() {
        let config = ConsensusConfig::dev_multi(0);
        assert_eq!(config.validator_set_size, 0);
        assert!(config.initial_validators.is_empty());
        // validator_set_size=0 < 3*0+1=1 → validation fails
        let result = config.validate();
        assert!(result.is_err(), "dev_multi(0) should fail validation");
    }

    #[test]
    fn test_epoch_length_serde_default() {
        // JSON without epoch_length field should deserialize with epoch_length=0
        let json = r#"{
            "slot_time_ms": 8000,
            "validator_set_size": 1,
            "fault_tolerance": 0,
            "base_timeout_ms": 4000,
            "max_timeout_ms": 8000,
            "initial_validators": []
        }"#;
        let config: ConsensusConfig = serde_json::from_str(json)
            .expect("should deserialize without epoch_length field");
        assert_eq!(config.epoch_length, 0, "epoch_length should default to 0");
    }

    #[test]
    fn test_dev_multi_deterministic_keys() {
        // Two calls with same count should produce identical keys
        let c1 = ConsensusConfig::dev_multi(4);
        let c2 = ConsensusConfig::dev_multi(4);
        for i in 0..4 {
            assert_eq!(
                c1.initial_validators[i].bls_public_key.to_bytes(),
                c2.initial_validators[i].bls_public_key.to_bytes(),
                "validator {} public key should be deterministic",
                i
            );
            assert_eq!(
                c1.initial_validators[i].address,
                c2.initial_validators[i].address,
                "validator {} address should be deterministic",
                i
            );
        }
    }

    #[test]
    fn test_validate_equal_timeouts() {
        // base_timeout == max_timeout should be valid (edge of <= check)
        let mut config = ConsensusConfig::dev();
        config.base_timeout_ms = 5000;
        config.max_timeout_ms = 5000;
        assert!(config.validate().is_ok(), "base == max timeout should be valid");
    }

    #[test]
    fn test_dev_multi_7_and_10_validators() {
        // n=7: f=(7-1)/3=2, quorum=2*2+1=5, need 3*2+1=7 ≤ 7 → valid
        let c7 = ConsensusConfig::dev_multi(7);
        assert_eq!(c7.fault_tolerance, 2);
        assert_eq!(c7.quorum_size(), 5);
        assert_eq!(c7.initial_validators.len(), 7);
        assert!(c7.validate().is_ok(), "7-validator config should be valid");

        // n=10: f=(10-1)/3=3, quorum=2*3+1=7, need 3*3+1=10 ≤ 10 → valid
        let c10 = ConsensusConfig::dev_multi(10);
        assert_eq!(c10.fault_tolerance, 3);
        assert_eq!(c10.quorum_size(), 7);
        assert_eq!(c10.initial_validators.len(), 10);
        assert!(c10.validate().is_ok(), "10-validator config should be valid");
    }

    #[test]
    fn test_chain_id_default() {
        // Without env override, effective_chain_id() returns the compile-time constant.
        // (Only safe to assert when N42_CHAIN_ID env var is not set in the test environment.)
        if std::env::var("N42_CHAIN_ID").is_err() {
            assert_eq!(effective_chain_id(), N42_CHAIN_ID);
        }
    }

    #[test]
    fn test_chain_id_env_override() {
        // Temporarily set the env var to a custom value and verify it takes effect.
        // NOTE: env vars are process-wide; this test must not run concurrently with
        // other tests that call effective_chain_id(). Mark tests appropriately in CI.
        unsafe { std::env::set_var("N42_CHAIN_ID", "1337") };
        let id = effective_chain_id();
        unsafe { std::env::remove_var("N42_CHAIN_ID") };
        assert_eq!(id, 1337, "N42_CHAIN_ID env var should override the default chain ID");
    }
}
