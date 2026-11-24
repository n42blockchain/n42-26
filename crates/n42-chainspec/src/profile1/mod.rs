//! N42 mixed production standard, Profile 1.
//!
//! `config.n42Profile = 1` in a genesis is the single switch for every rule of
//! the standard from block 0 (P2). This module owns what the switch means for
//! the chain specification:
//!
//! - [`config`]: the closed genesis key lists and value ranges (P4, AM12);
//! - [`header`]: the 24-element header layout and its seal (B1-B12, AM4);
//! - the genesis header itself ([`profile1_genesis_header`], A6), whose
//!   `stateRoot` is the QMDB root of the alloc (A5);
//! - the process-wide profile value ([`install_n42_profile`]) and the start-up
//!   refusal of environment overrides of consensus values (AM10 / J4).
//!
//! A genesis without `n42Profile` never reaches this module's checks and
//! keeps today's behaviour.

pub mod config;
pub mod header;

pub use config::{
    PROFILE_1, Profile1CommitteePool, Profile1Config, Profile1Validator, parse_dev_block_reward,
};
pub use header::{
    PROFILE1_EXTRA_MAGIC, PROFILE1_MAX_EXTRA_LEN, PROFILE1_SEAL_LEN, Profile1Extra,
    Profile1ExtraError, Profile1Header, Profile1HeaderError, Profile1SealError, profile1_extra,
};

use alloy_consensus::{EMPTY_ROOT_HASH, Header};
use alloy_genesis::Genesis;
use alloy_primitives::{B256, U256, keccak256};
use n42_twig_core::qmdb_compat::{
    QmdbCompatTree, QmdbOperation, QmdbOperationError, encode_gov5_account_value,
    gov5_account_key, gov5_storage_key,
};
use reth_chainspec::ChainSpec;
use reth_primitives_traits::SealedHeader;
use std::sync::OnceLock;

/// Why a Profile-1 genesis or start-up is refused (P4, AM10).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("profile 1 genesis: {0}")]
pub struct Profile1GenesisError(pub String);

/// The production profile a chain's genesis selects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum N42Profile {
    /// Mixed production standard, Profile 1 (`config.n42Profile = 1`).
    Profile1,
}

/// Whether a parsed genesis declares a production profile at all (any value
/// of `config.n42Profile`; a value other than 1 is refused by the readers).
pub fn genesis_declares_profile(genesis: &Genesis) -> bool {
    genesis.config.extra_fields.contains_key("n42Profile")
}

/// [`genesis_declares_profile`] for the genesis file as written.
pub fn raw_genesis_declares_profile(raw: &serde_json::Value) -> bool {
    raw.get("config")
        .and_then(serde_json::Value::as_object)
        .is_some_and(|config| config.contains_key("n42Profile"))
}

/// The profile of a parsed genesis: `None` without `n42Profile`, Profile 1
/// for a valid Profile-1 `config.hotstuff`, an error otherwise.
pub fn resolve_genesis_profile(
    genesis: &Genesis,
) -> Result<Option<(N42Profile, Profile1Config)>, Profile1GenesisError> {
    if !genesis_declares_profile(genesis) {
        return Ok(None);
    }
    Profile1Config::from_genesis(genesis).map(|config| Some((N42Profile::Profile1, config)))
}

static PROFILE: OnceLock<Option<N42Profile>> = OnceLock::new();

/// Installs the chain's profile for this process, once, at start-up and
/// before any consensus component exists. Returns the profile already in
/// force when a different one was installed or read first; the caller must
/// then refuse to start.
pub fn install_n42_profile(profile: Option<N42Profile>) -> Result<(), Option<N42Profile>> {
    let installed = *PROFILE.get_or_init(|| profile);
    if installed == profile {
        Ok(())
    } else {
        Err(installed)
    }
}

/// The profile in force in this process; `None` (no profile, today's rules)
/// in a process that installs none.
pub fn n42_profile() -> Option<N42Profile> {
    *PROFILE.get_or_init(|| None)
}

/// Environment variables that set or override a consensus-visible value in
/// this client. On a Profile-1 genesis a node refuses to start when any is
/// set (P3, J4, AM10). On other chains they keep their meaning.
pub const PROFILE1_FORBIDDEN_ENV: &[&str] = &[
    // A1: chain id and genesis hash come from the genesis file.
    "N42_CHAIN_ID",
    "N42_INTEROP_GENESIS_HASH",
    // A2/A3: consensus parameters and the validator set.
    "N42_CONSENSUS_CONFIG",
    "N42_VALIDATOR_COUNT",
    // A15/A22: leader schedule.
    "N42_LEADER_TENURE",
    "N42_HOTSTUFF_ROTATE_ON_TIMEOUT",
    // A16/A17: pacemaker and pacing.
    "N42_BASE_TIMEOUT_MS",
    "N42_MAX_TIMEOUT_MS",
    "N42_BLOCK_INTERVAL_MS",
    "N42_MAX_EMPTY_SKIPS",
    // A12: frame blocks are not in Profile 1.
    "N42_FRAME_BLOCKS",
    "N42_FRAME_REF_BLOCKS",
    // Checks that change what a block means.
    "N42_SKIP_STATE_ROOT",
    "N42_DEFER_STATE_ROOT",
    "N42_SKIP_TX_VERIFY",
    // A8: rewards come from the genesis.
    "N42_DAILY_BASE_REWARD_GWEI",
    "N42_MAX_REWARDS_PER_BLOCK",
    "N42_UNSTAKED_REWARD_RATIO",
    // §3: anchored ranges are enforced, no observe mode.
    "N42_SYNC_ANCHOR",
];

/// Prefixes of forbidden variables: every gov5 interop switch
/// (`N42_GOV5_LEGACY_SIGNING`, `N42_GOV5_PRAGUE_TIME`,
/// `N42_GOV5_QMDB_EXECUTION*`, `N42_GOV5_STATE_ROOT_TRUST`, …) and every
/// reward knob.
pub const PROFILE1_FORBIDDEN_ENV_PREFIXES: &[&str] = &["N42_GOV5_", "N42_REWARD_"];

/// The forbidden variables among `names`, sorted.
pub fn profile1_forbidden_env<I, S>(names: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut found: Vec<String> = names
        .into_iter()
        .filter(|name| {
            let name = name.as_ref();
            PROFILE1_FORBIDDEN_ENV.contains(&name)
                || PROFILE1_FORBIDDEN_ENV_PREFIXES
                    .iter()
                    .any(|prefix| name.starts_with(prefix))
        })
        .map(|name| name.as_ref().to_owned())
        .collect();
    found.sort_unstable();
    found.dedup();
    found
}

/// AM10: refuses when this process's environment sets a forbidden variable.
pub fn refuse_profile1_env() -> Result<(), Profile1GenesisError> {
    let found = profile1_forbidden_env(
        std::env::vars_os().filter_map(|(name, _)| name.into_string().ok()),
    );
    if found.is_empty() {
        return Ok(());
    }
    Err(Profile1GenesisError(format!(
        "environment sets consensus value(s) a Profile 1 chain reads only from its genesis (J4): {}",
        found.join(", ")
    )))
}

/// gov5's deterministic block-zero QMDB state of a genesis alloc: accounts
/// and non-zero storage slots in one key-sorted append batch (gov5
/// `QMDBRootComputer.ComputeRoot`). An account with nonce 0, zero balance and
/// no code writes no account leaf.
pub fn qmdb_genesis_tree(genesis: &Genesis) -> Result<QmdbCompatTree, QmdbOperationError> {
    let mut operations = Vec::new();
    for (address, account) in &genesis.alloc {
        let nonce = account.nonce.unwrap_or_default();
        let code_hash = account
            .code
            .as_ref()
            .map(|code| keccak256(code.as_ref()))
            .unwrap_or(B256::ZERO);
        let account_is_empty = nonce == 0
            && account.balance.is_zero()
            && account.code.as_ref().is_none_or(|code| code.is_empty());
        if !account_is_empty {
            operations.push(QmdbOperation {
                key: gov5_account_key(address.as_ref()),
                value: Some(encode_gov5_account_value(
                    nonce,
                    &account.balance.to_be_bytes(),
                    &code_hash.0,
                )),
            });
        }
        if let Some(storage) = &account.storage {
            for (slot, value) in storage {
                if *value == B256::ZERO {
                    continue;
                }
                operations.push(QmdbOperation {
                    key: gov5_storage_key(address.as_ref(), slot.as_ref()),
                    value: Some(value.to_vec()),
                });
            }
        }
    }
    let mut tree = QmdbCompatTree::new();
    tree.apply_sorted_ops(operations)?;
    Ok(tree)
}

/// A7 (Q-S1): no storage-only alloc entry, no zero-valued storage slot.
pub fn check_profile1_alloc(genesis: &Genesis) -> Result<(), Profile1GenesisError> {
    for (address, account) in &genesis.alloc {
        let Some(storage) = account.storage.as_ref().filter(|storage| !storage.is_empty()) else {
            continue;
        };
        let has_code = account.code.as_ref().is_some_and(|code| !code.is_empty());
        if account.nonce.unwrap_or_default() == 0 && account.balance.is_zero() && !has_code {
            return Err(Profile1GenesisError(format!(
                "alloc {address} is a storage-only entry (nonce 0, balance 0, no code; A7)"
            )));
        }
        if let Some((slot, _)) = storage.iter().find(|(_, value)| value.is_zero()) {
            return Err(Profile1GenesisError(format!(
                "alloc {address} storage slot {slot} has value 0 (A7)"
            )));
        }
    }
    Ok(())
}

/// The genesis header fields a Profile-1 genesis file may not set to
/// anything else (A6, B3): empty `extraData`, zero difficulty, nonce and
/// mixHash, block number 0 with no parent, `gasLimit = hotstuff.gasLimit`
/// (A23), a base fee, no blob gas used.
fn check_profile1_header_fields(
    genesis: &Genesis,
    config: &Profile1Config,
) -> Result<u64, Profile1GenesisError> {
    let refuse = |message: String| Err(Profile1GenesisError(message));
    if !genesis.extra_data.is_empty() {
        return refuse("extraData must be empty".to_owned());
    }
    if genesis.gas_limit != config.gas_limit {
        return refuse(format!(
            "genesis gasLimit {} differs from config.hotstuff.gasLimit {}",
            genesis.gas_limit, config.gas_limit
        ));
    }
    if genesis.difficulty != U256::ZERO {
        return refuse(format!("difficulty must be 0, got {}", genesis.difficulty));
    }
    if genesis.nonce != 0 {
        return refuse(format!("nonce must be 0, got {}", genesis.nonce));
    }
    if genesis.mix_hash != B256::ZERO {
        return refuse("mixHash must be zero".to_owned());
    }
    if genesis.number.unwrap_or_default() != 0
        || genesis.parent_hash.unwrap_or_default() != B256::ZERO
    {
        return refuse("block zero has number 0 and no parent".to_owned());
    }
    if genesis.blob_gas_used.unwrap_or_default() != 0
        || genesis.excess_blob_gas.unwrap_or_default() != 0
    {
        return refuse("blobGasUsed and excessBlobGas must be 0".to_owned());
    }
    let base_fee = genesis
        .base_fee_per_gas
        .ok_or_else(|| Profile1GenesisError("baseFeePerGas is mandatory".to_owned()))?;
    u64::try_from(base_fee)
        .map_err(|_| Profile1GenesisError(format!("baseFeePerGas {base_fee} exceeds u64")))
}

/// The Profile-1 genesis header (A6, V1): the fields of the genesis file;
/// `stateRoot` the QMDB root of the alloc (A5); empty-trie transactions,
/// receipts, withdrawals and requests roots (AM3); zero ommers hash (B3);
/// zero blob gas, parent beacon root and mobile-registry root; element 22
/// the gap; slot number 0.
pub fn profile1_genesis_header(
    genesis: &Genesis,
    config: &Profile1Config,
) -> Result<Profile1Header, Profile1GenesisError> {
    let base_fee = check_profile1_header_fields(genesis, config)?;
    check_profile1_alloc(genesis)?;
    let state_root = qmdb_genesis_tree(genesis)
        .map(|tree| B256::from(tree.root()))
        .map_err(|error| Profile1GenesisError(format!("QMDB genesis state: {error}")))?;
    Ok(Profile1Header {
        header: Header {
            parent_hash: B256::ZERO,
            ommers_hash: B256::ZERO,
            beneficiary: genesis.coinbase,
            state_root,
            transactions_root: EMPTY_ROOT_HASH,
            receipts_root: EMPTY_ROOT_HASH,
            logs_bloom: Default::default(),
            difficulty: U256::ZERO,
            number: 0,
            gas_limit: genesis.gas_limit,
            gas_used: 0,
            timestamp: genesis.timestamp,
            extra_data: Default::default(),
            mix_hash: B256::ZERO,
            nonce: Default::default(),
            base_fee_per_gas: Some(base_fee),
            withdrawals_root: Some(EMPTY_ROOT_HASH),
            blob_gas_used: Some(0),
            excess_blob_gas: Some(0),
            parent_beacon_block_root: Some(B256::ZERO),
            requests_hash: Some(EMPTY_ROOT_HASH),
            block_access_list_hash: None,
            slot_number: Some(0),
        },
        mobile_registry_root: B256::ZERO,
    })
}

/// A Profile-1 chain specification from the genesis file as written: P4 over
/// the raw JSON, then reth's chain spec of the parsed genesis with its genesis
/// header replaced by the Profile-1 header and sealed with the Profile-1 hash.
pub fn profile1_chain_spec(
    raw: &serde_json::Value,
) -> Result<(ChainSpec, Profile1Config, Profile1Header), Profile1GenesisError> {
    let config = Profile1Config::from_raw_genesis(raw)?;
    let genesis: Genesis = serde_json::from_value(raw.clone())
        .map_err(|error| Profile1GenesisError(format!("genesis does not parse: {error}")))?;
    if genesis.config.chain_id != config.chain_id {
        return Err(Profile1GenesisError(format!(
            "config.chainId {} does not parse as the chain id",
            config.chain_id
        )));
    }
    let header = profile1_genesis_header(&genesis, &config)?;
    let mut spec = ChainSpec::from(genesis);
    spec.genesis_header = SealedHeader::new(header.header.clone(), header.hash());
    Ok((spec, config, header))
}
