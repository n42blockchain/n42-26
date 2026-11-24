//! The Profile-1 genesis `config` (standard §5, P2-P4, A1-A8, A15-A19, A23,
//! B15, AM8, AM12, AM16, AM22) and its closed key lists (V8 `allowed-keys.json`).
//!
//! A Profile-1 genesis is refused at start-up when `config`, `config.hotstuff`,
//! `config.hotstuff.committeePool` or a `validators[]` entry has a key outside
//! its list, misses a mandatory key, or holds a value this client does not
//! implement (P4). Every value is read from the genesis and nowhere else (P3).

use super::Profile1GenesisError as Error;
use crate::ValidatorInfo;
use alloy_genesis::Genesis;
use alloy_primitives::{Address, B256, U256};
use n42_primitives::BlsPublicKey;
use serde_json::{Map, Value};
use std::collections::HashSet;

/// The value of `config.n42Profile` this client implements.
pub const PROFILE_1: u64 = 1;

/// `config` keys of a Profile-1 genesis (V8 `allowed-keys.json`).
pub const CONFIG_MANDATORY_KEYS: &[&str] = &[
    "amsterdamTime",
    "berlinBlock",
    "byzantiumBlock",
    "cancunTime",
    "chainId",
    "consensus",
    "constantinopleBlock",
    "eip150Block",
    "eip155Block",
    "eip158Block",
    "homesteadBlock",
    "hotstuff",
    "istanbulBlock",
    "londonBlock",
    "mergeNetsplitBlock",
    "n42Profile",
    "osakaTime",
    "petersburgBlock",
    "pragueTime",
    "shanghaiTime",
    "stateScheme",
    "terminalTotalDifficulty",
    "terminalTotalDifficultyPassed",
];
/// Optional `config` keys.
pub const CONFIG_OPTIONAL_KEYS: &[&str] = &["blobSchedule"];
/// `config.hotstuff` keys (all mandatory).
pub const HOTSTUFF_KEYS: &[&str] = &[
    "baseTimeoutMs",
    "blockIntervalMs",
    "committeePool",
    "deferredExecutionDepth",
    "devBlockReward",
    "devFaucetAddress",
    "epochLength",
    "gasLimit",
    "leaderTenure",
    "maxBlockBytes",
    "maxTimeoutMs",
    "validators",
];
/// `config.hotstuff.committeePool` keys (all mandatory).
pub const COMMITTEE_POOL_KEYS: &[&str] = &["committeeSize", "poolSize", "rampBlocks", "seedHex"];
/// `config.hotstuff.validators[]` keys (all mandatory).
pub const VALIDATOR_KEYS: &[&str] = &["address", "blsKey"];

/// Ethereum fork fields that name the EVM rule set; each must be the JSON
/// number 0 (A4: everything from genesis, no block-number or later forks).
const ZERO_FORK_FIELDS: &[&str] = &[
    "homesteadBlock",
    "eip150Block",
    "eip155Block",
    "eip158Block",
    "byzantiumBlock",
    "constantinopleBlock",
    "petersburgBlock",
    "istanbulBlock",
    "berlinBlock",
    "londonBlock",
    "mergeNetsplitBlock",
    "terminalTotalDifficulty",
    "shanghaiTime",
    "cancunTime",
    "pragueTime",
    "osakaTime",
    "amsterdamTime",
];

/// The only `deferredExecutionDepth` this client accepts (A10, O4).
pub const PROFILE1_DEFERRED_EXECUTION_DEPTH: u64 = 1;
/// The smallest validator set (A3).
pub const PROFILE1_MIN_VALIDATORS: usize = 4;

const GWEI: u64 = 1_000_000_000;

/// `config.hotstuff.committeePool` (A19): always on, no `enabled` flag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile1CommitteePool {
    pub seed: B256,
    pub pool_size: u64,
    pub committee_size: u64,
    pub ramp_blocks: u64,
}

/// One `config.hotstuff.validators[]` entry; list order is the bitmap order
/// and the leader index (A3).
#[derive(Clone, Debug)]
pub struct Profile1Validator {
    pub address: Address,
    pub bls_key: BlsPublicKey,
}

/// Every consensus-visible value of a Profile-1 chain, read from its genesis.
#[derive(Clone, Debug)]
pub struct Profile1Config {
    pub chain_id: u64,
    pub validators: Vec<Profile1Validator>,
    /// A15: views per leader, >= 1.
    pub leader_tenure: u64,
    /// A16.
    pub base_timeout_ms: u64,
    /// A16, >= `base_timeout_ms`.
    pub max_timeout_ms: u64,
    /// A17: pacing, not a timestamp step.
    pub block_interval_ms: u64,
    /// A18: views per epoch, >= 1.
    pub epoch_length: u64,
    /// A10: always 1 on this client.
    pub deferred_execution_depth: u64,
    /// A23: every header's gasLimit.
    pub gas_limit: u64,
    /// B15.
    pub max_block_bytes: u64,
    pub committee_pool: Profile1CommitteePool,
    /// A8, AM8: wei to the coinbase and to the faucet each block; a whole
    /// number of gwei, at most `u64::MAX` gwei (AM12).
    pub dev_block_reward: U256,
    /// A8, AM16: non-zero.
    pub dev_faucet_address: Address,
}

fn object<'a>(value: &'a Value, path: &str) -> Result<&'a Map<String, Value>, Error> {
    value
        .as_object()
        .ok_or_else(|| Error(format!("{path} is not an object")))
}

/// P4: `obj` holds every key of `mandatory`, and nothing outside
/// `mandatory ∪ optional`.
fn check_keys(
    obj: &Map<String, Value>,
    path: &str,
    mandatory: &[&str],
    optional: &[&str],
) -> Result<(), Error> {
    let mut unknown: Vec<&str> = obj
        .keys()
        .map(String::as_str)
        .filter(|key| !mandatory.contains(key) && !optional.contains(key))
        .collect();
    if !unknown.is_empty() {
        unknown.sort_unstable();
        return Err(Error(format!(
            "{path}: field(s) not in the Profile 1 key list (unknown or legacy): {}",
            unknown.join(", ")
        )));
    }
    if let Some(missing) = mandatory.iter().find(|key| !obj.contains_key(**key)) {
        return Err(Error(format!("{path}: mandatory field \"{missing}\" missing")));
    }
    Ok(())
}

/// A JSON unsigned integer (not a string, not a float).
fn json_u64(obj: &Map<String, Value>, path: &str, key: &str) -> Result<u64, Error> {
    match obj.get(key) {
        Some(Value::Number(number)) => number
            .as_u64()
            .ok_or_else(|| Error(format!("{path}.{key} is not an unsigned 64-bit integer"))),
        Some(_) => Err(Error(format!("{path}.{key} must be a JSON number"))),
        None => Err(Error(format!("{path}: mandatory field \"{key}\" missing"))),
    }
}

fn json_str<'a>(obj: &'a Map<String, Value>, path: &str, key: &str) -> Result<&'a str, Error> {
    obj.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| Error(format!("{path}.{key} must be a JSON string")))
}

/// AM8 / AM12: a decimal string of wei, a whole number of gwei, at most
/// `u64::MAX` gwei.
pub fn parse_dev_block_reward(value: &Value) -> Result<U256, Error> {
    let Value::String(text) = value else {
        return Err(Error(
            "config.hotstuff.devBlockReward must be a decimal string of wei, not a JSON number"
                .to_owned(),
        ));
    };
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(Error(format!(
            "config.hotstuff.devBlockReward {text:?} is not a decimal string of wei"
        )));
    }
    let wei = U256::from_str_radix(text, 10).map_err(|_| {
        Error(format!(
            "config.hotstuff.devBlockReward {text} exceeds 2^64-1 gwei"
        ))
    })?;
    let gwei = U256::from(GWEI);
    if wei % gwei != U256::ZERO {
        return Err(Error(format!(
            "config.hotstuff.devBlockReward {text} is not a whole number of gwei"
        )));
    }
    if wei / gwei > U256::from(u64::MAX) {
        return Err(Error(format!(
            "config.hotstuff.devBlockReward {text} exceeds 2^64-1 gwei"
        )));
    }
    Ok(wei)
}

fn parse_validators(value: &Value) -> Result<Vec<Profile1Validator>, Error> {
    let entries = value
        .as_array()
        .ok_or_else(|| Error("config.hotstuff.validators is not an array".to_owned()))?;
    if entries.len() < PROFILE1_MIN_VALIDATORS {
        return Err(Error(format!(
            "config.hotstuff.validators needs at least {PROFILE1_MIN_VALIDATORS} entries, got {}",
            entries.len()
        )));
    }
    let mut addresses = HashSet::new();
    let mut keys = HashSet::new();
    let mut validators = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let path = format!("config.hotstuff.validators[{index}]");
        let entry = object(entry, &path)?;
        check_keys(entry, &path, VALIDATOR_KEYS, &[])?;
        let address: Address = json_str(entry, &path, "address")?
            .parse()
            .map_err(|error| Error(format!("{path}.address: {error}")))?;
        let key_text = json_str(entry, &path, "blsKey")?;
        let key_bytes: [u8; 48] = key_text
            .strip_prefix("0x")
            .and_then(|hex| alloy_primitives::hex::decode(hex).ok())
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or_else(|| Error(format!("{path}.blsKey is not 0x-prefixed 48-byte hex")))?;
        let bls_key = BlsPublicKey::from_bytes(&key_bytes)
            .map_err(|error| Error(format!("{path}.blsKey: {error}")))?;
        bls_key
            .validate()
            .map_err(|error| Error(format!("{path}.blsKey: {error}")))?;
        if !addresses.insert(address) {
            return Err(Error(format!("{path}: duplicate address {address}")));
        }
        if !keys.insert(key_bytes) {
            return Err(Error(format!("{path}: duplicate blsKey")));
        }
        validators.push(Profile1Validator { address, bls_key });
    }
    Ok(validators)
}

fn parse_committee_pool(value: &Value) -> Result<Profile1CommitteePool, Error> {
    let path = "config.hotstuff.committeePool";
    let pool = object(value, path)?;
    check_keys(pool, path, COMMITTEE_POOL_KEYS, &[])?;
    let seed_text = json_str(pool, path, "seedHex")?;
    let seed = seed_text
        .strip_prefix("0x")
        .filter(|hex| hex.len() == 64)
        .and_then(|hex| hex.parse::<B256>().ok())
        .ok_or_else(|| Error(format!("{path}.seedHex is not 0x-prefixed 32-byte hex")))?;
    let pool_size = json_u64(pool, path, "poolSize")?;
    let committee_size = json_u64(pool, path, "committeeSize")?;
    let ramp_blocks = json_u64(pool, path, "rampBlocks")?;
    if committee_size == 0 || committee_size > pool_size {
        return Err(Error(format!(
            "{path}: committeeSize {committee_size} must be in 1..=poolSize ({pool_size})"
        )));
    }
    Ok(Profile1CommitteePool {
        seed,
        pool_size,
        committee_size,
        ramp_blocks,
    })
}

impl Profile1Config {
    /// Reads and checks `config.hotstuff` (closed key lists, ranges). The
    /// `config`-level keys are checked by [`Self::from_raw_genesis`], which
    /// sees the file as written.
    pub fn from_hotstuff(chain_id: u64, hotstuff: &Value) -> Result<Self, Error> {
        let path = "config.hotstuff";
        let obj = object(hotstuff, path)?;
        check_keys(obj, path, HOTSTUFF_KEYS, &[])?;
        let positive = |key: &str| -> Result<u64, Error> {
            let value = json_u64(obj, path, key)?;
            if value == 0 {
                return Err(Error(format!("{path}.{key} must be >= 1")));
            }
            Ok(value)
        };
        let leader_tenure = positive("leaderTenure")?;
        let epoch_length = positive("epochLength")?;
        let base_timeout_ms = positive("baseTimeoutMs")?;
        let max_timeout_ms = positive("maxTimeoutMs")?;
        if max_timeout_ms < base_timeout_ms {
            return Err(Error(format!(
                "{path}.maxTimeoutMs {max_timeout_ms} is below baseTimeoutMs {base_timeout_ms}"
            )));
        }
        let block_interval_ms = positive("blockIntervalMs")?;
        let deferred_execution_depth = json_u64(obj, path, "deferredExecutionDepth")?;
        if deferred_execution_depth != PROFILE1_DEFERRED_EXECUTION_DEPTH {
            return Err(Error(format!(
                "{path}.deferredExecutionDepth {deferred_execution_depth} is not implemented (only {PROFILE1_DEFERRED_EXECUTION_DEPTH})"
            )));
        }
        let gas_limit = positive("gasLimit")?;
        let max_block_bytes = positive("maxBlockBytes")?;
        let dev_block_reward = parse_dev_block_reward(&obj["devBlockReward"])?;
        let dev_faucet_address: Address = json_str(obj, path, "devFaucetAddress")?
            .parse()
            .map_err(|error| Error(format!("{path}.devFaucetAddress: {error}")))?;
        if dev_faucet_address.is_zero() {
            return Err(Error(format!(
                "{path}.devFaucetAddress is mandatory and must not be the zero address"
            )));
        }
        let committee_pool = parse_committee_pool(&obj["committeePool"])?;
        let validators = parse_validators(&obj["validators"])?;
        Ok(Self {
            chain_id,
            validators,
            leader_tenure,
            base_timeout_ms,
            max_timeout_ms,
            block_interval_ms,
            epoch_length,
            deferred_execution_depth,
            gas_limit,
            max_block_bytes,
            committee_pool,
            dev_block_reward,
            dev_faucet_address,
        })
    }

    /// P4 over the genesis file as written: the closed `config` key list,
    /// the fork fields, `n42Profile`, `consensus`, `stateScheme`, then
    /// `config.hotstuff`.
    pub fn from_raw_genesis(raw: &Value) -> Result<Self, Error> {
        let root = object(raw, "genesis")?;
        let config = object(
            root.get("config")
                .ok_or_else(|| Error("genesis has no config".to_owned()))?,
            "config",
        )?;
        check_profile_field(config.get("n42Profile"))?;
        check_keys(config, "config", CONFIG_MANDATORY_KEYS, CONFIG_OPTIONAL_KEYS)?;
        for key in ZERO_FORK_FIELDS {
            match config.get(*key) {
                Some(Value::Number(number)) if number.as_u64() == Some(0) => {}
                _ => return Err(Error(format!("config.{key} must be the JSON number 0"))),
            }
        }
        if config.get("terminalTotalDifficultyPassed") != Some(&Value::Bool(true)) {
            return Err(Error(
                "config.terminalTotalDifficultyPassed must be true".to_owned(),
            ));
        }
        if json_str(config, "config", "consensus")? != "hotstuff" {
            return Err(Error("config.consensus must be \"hotstuff\"".to_owned()));
        }
        if json_str(config, "config", "stateScheme")? != "qmdb" {
            return Err(Error("config.stateScheme must be \"qmdb\" (A5)".to_owned()));
        }
        if let Some(schedule) = config.get("blobSchedule") {
            object(schedule, "config.blobSchedule")?;
        }
        let chain_id = json_u64(config, "config", "chainId")?;
        if chain_id == 0 {
            return Err(Error("config.chainId must be >= 1".to_owned()));
        }
        Self::from_hotstuff(chain_id, &config["hotstuff"])
    }

    /// Reads the configuration back from a genesis already parsed by alloy
    /// (the node's chain spec, checked as raw JSON by the chain-spec
    /// parser). Re-checks `n42Profile` and the whole `config.hotstuff`
    /// object, which alloy keeps verbatim.
    pub fn from_genesis(genesis: &Genesis) -> Result<Self, Error> {
        let extra = &genesis.config.extra_fields;
        check_profile_field(extra.get("n42Profile"))?;
        let hotstuff = extra
            .get("hotstuff")
            .ok_or_else(|| Error("config: mandatory field \"hotstuff\" missing".to_owned()))?;
        Self::from_hotstuff(genesis.config.chain_id, hotstuff)
    }

    /// Byzantine fault tolerance `f = ⌊(n−1)/3⌋` (A3).
    pub fn fault_tolerance(&self) -> u32 {
        ((self.validators.len() - 1) / 3) as u32
    }

    /// The validator set in this client's form, genesis order.
    pub fn validator_infos(&self) -> Vec<ValidatorInfo> {
        self.validators
            .iter()
            .map(|validator| ValidatorInfo {
                address: validator.address,
                bls_public_key: validator.bls_key.clone(),
                p2p_peer_id: None,
            })
            .collect()
    }

    /// The consensus configuration of a Profile-1 chain: every value from
    /// the genesis (A2, A3, A16-A18).
    pub fn consensus_config(&self) -> crate::ConsensusConfig {
        crate::ConsensusConfig {
            slot_time_ms: self.block_interval_ms,
            validator_set_size: self.validators.len() as u32,
            fault_tolerance: self.fault_tolerance(),
            base_timeout_ms: self.base_timeout_ms,
            max_timeout_ms: self.max_timeout_ms,
            initial_validators: self.validator_infos(),
            epoch_length: self.epoch_length,
            max_historical_epochs: crate::default_max_historical_epochs(),
        }
    }
}

/// `config.n42Profile`: the JSON number 1, nothing else (P4).
fn check_profile_field(value: Option<&Value>) -> Result<(), Error> {
    match value {
        Some(Value::Number(number)) if number.as_u64() == Some(PROFILE_1) => Ok(()),
        Some(other) => Err(Error(format!("config.n42Profile {other} is not implemented"))),
        None => Err(Error("config: mandatory field \"n42Profile\" missing".to_owned())),
    }
}
