use alloy_eips::eip4895::Withdrawal;
use alloy_primitives::{address, Address, U256};
use reth_primitives_traits::SignerRecoverable;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

/// System staking address — a "black hole" with no private key.
pub const STAKING_ADDRESS: Address = address!("0000000000000000000000000000000000000042");

/// Minimum stake: 32 N (32 * 10^18 wei).
/// 32 * 10^18 = 0x1_BC16D674EC800000
///   low limb:  0xBC16D674EC800000
///   high limb: 0x1
pub const MIN_STAKE_WEI: U256 = U256::from_limbs([0xBC16D674EC800000, 0x1, 0, 0]);

/// Unstake cooldown: ~1 day at 4s/block.
pub const UNSTAKE_COOLDOWN_BLOCKS: u64 = 21600;

/// Function selector for `unstake()`: first 4 bytes of keccak256("unstake()").
const UNSTAKE_SELECTOR: [u8; 4] = [0x2d, 0xef, 0x66, 0x20];

/// Status of a stake entry.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub enum StakeStatus {
    Active,
    Unstaking { initiated_block: u64 },
}

/// A single staker's entry.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StakeEntry {
    /// The EVM address that sent the staking transaction.
    pub staker: Address,
    /// The associated BLS public key (48 bytes).
    #[serde(with = "hex_bytes_48")]
    pub bls_pubkey: [u8; 48],
    /// Cumulative staked amount in wei.
    #[serde(with = "u256_dec")]
    pub amount: U256,
    /// Block number of the first stake.
    pub staked_at_block: u64,
    /// Current status.
    pub status: StakeStatus,
}

/// A registered (non-staking) verifier entry.
/// Registers a BLS pubkey → EVM address mapping so rewards can be delivered.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RegistrationEntry {
    pub evm_address: Address,
    #[serde(with = "hex_bytes_48")]
    pub bls_pubkey: [u8; 48],
    pub registered_at_block: u64,
}

/// Manages on-chain staking state by scanning committed blocks.
#[derive(Debug)]
pub struct StakingManager {
    stakes: HashMap<Address, StakeEntry>,
    /// Pure registrations (not staked). Upgraded to stakes when user stakes.
    registrations: HashMap<Address, RegistrationEntry>,
    bls_to_staker: HashMap<[u8; 48], Address>,
    /// Reward address (BLS-derived keccak) → staker EVM address mapping.
    reward_to_staker: HashMap<Address, Address>,
    pending_returns: VecDeque<Withdrawal>,
    state_file: Option<PathBuf>,
    /// The most recently scanned block number (for RPC current-block reference).
    last_scanned_block: u64,
    /// Monotonic counter for globally unique withdrawal indices.
    next_withdrawal_index: u64,
}

impl Default for StakingManager {
    fn default() -> Self {
        Self::new()
    }
}

impl StakingManager {
    pub fn new() -> Self {
        Self {
            stakes: HashMap::new(),
            registrations: HashMap::new(),
            bls_to_staker: HashMap::new(),
            reward_to_staker: HashMap::new(),
            pending_returns: VecDeque::new(),
            state_file: None,
            last_scanned_block: 0,
            next_withdrawal_index: 1_000_000,
        }
    }

    /// Loads state from file, or creates a new manager if the file doesn't exist.
    pub fn load_or_new(path: &Path) -> Self {
        match Self::load(path) {
            Ok(mut mgr) => {
                mgr.state_file = Some(path.to_path_buf());
                info!(
                    target: "n42::staking",
                    stakers = mgr.stakes.len(),
                    registrations = mgr.registrations.len(),
                    pending_returns = mgr.pending_returns.len(),
                    last_scanned_block = mgr.last_scanned_block,
                    "staking state loaded from disk"
                );
                mgr
            }
            Err(e) => {
                if path.exists() {
                    warn!(target: "n42::staking", error = %e, "failed to load staking state, starting fresh");
                } else {
                    debug!(target: "n42::staking", "no staking state file found, starting fresh");
                }
                let mut mgr = Self::new();
                mgr.state_file = Some(path.to_path_buf());
                mgr
            }
        }
    }

    fn load(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let data = std::fs::read(path)?;
        let persisted: PersistedStakingState = serde_json::from_slice(&data)?;
        let mut mgr = Self::new();
        mgr.last_scanned_block = persisted.last_scanned_block;
        mgr.next_withdrawal_index = persisted.next_withdrawal_index;
        for entry in persisted.stakes {
            mgr.bls_to_staker.insert(entry.bls_pubkey, entry.staker);
            let reward_addr = bls_pubkey_to_reward_address(&entry.bls_pubkey);
            mgr.reward_to_staker.insert(reward_addr, entry.staker);
            mgr.stakes.insert(entry.staker, entry);
        }
        // Restore registrations
        for reg in persisted.registrations {
            mgr.bls_to_staker.insert(reg.bls_pubkey, reg.evm_address);
            let reward_addr = bls_pubkey_to_reward_address(&reg.bls_pubkey);
            mgr.reward_to_staker.insert(reward_addr, reg.evm_address);
            mgr.registrations.insert(reg.evm_address, reg);
        }
        // Restore pending returns (Fix #1: Critical — fund loss prevention)
        for pw in persisted.pending_returns {
            mgr.pending_returns.push_back(Withdrawal {
                index: pw.index,
                validator_index: pw.validator_index,
                address: pw.address.parse().unwrap_or_default(),
                amount: pw.amount,
            });
        }
        Ok(mgr)
    }

    /// Persists the current state to disk using atomic write (Fix #4).
    pub fn save(&self) {
        let path = match &self.state_file {
            Some(p) => p,
            None => return,
        };
        let persisted = PersistedStakingState {
            stakes: self.stakes.values().cloned().collect(),
            registrations: self.registrations.values().cloned().collect(),
            pending_returns: self
                .pending_returns
                .iter()
                .map(|w| PersistedWithdrawal {
                    index: w.index,
                    validator_index: w.validator_index,
                    address: format!("{}", w.address),
                    amount: w.amount,
                })
                .collect(),
            last_scanned_block: self.last_scanned_block,
            next_withdrawal_index: self.next_withdrawal_index,
        };
        match serde_json::to_vec_pretty(&persisted) {
            Ok(data) => {
                // Atomic write: write to temp file, then rename.
                let tmp_path = path.with_extension("json.tmp");
                if let Err(e) = std::fs::write(&tmp_path, &data) {
                    warn!(target: "n42::staking", error = %e, "failed to write staking state temp file");
                    return;
                }
                if let Err(e) = std::fs::rename(&tmp_path, path) {
                    warn!(target: "n42::staking", error = %e, "failed to rename staking state temp file");
                    // Clean up temp file on rename failure
                    let _ = std::fs::remove_file(&tmp_path);
                }
            }
            Err(e) => {
                warn!(target: "n42::staking", error = %e, "failed to serialize staking state");
            }
        }
    }

    /// Scans a committed block's execution payload for staking/unstaking transactions.
    pub fn scan_committed_block(&mut self, block_number: u64, payload_json: &[u8]) {
        self.last_scanned_block = block_number;

        let parsed: serde_json::Value = match serde_json::from_slice(payload_json) {
            Ok(v) => v,
            Err(e) => {
                debug!(target: "n42::staking", error = %e, "failed to parse payload JSON for staking scan");
                return;
            }
        };

        let transactions = match parsed.get("transactions").and_then(|t| t.as_array()) {
            Some(txs) => txs,
            None => return,
        };

        for tx_hex in transactions {
            let tx_str = match tx_hex.as_str() {
                Some(s) => s.strip_prefix("0x").unwrap_or(s),
                None => continue,
            };
            let tx_bytes = match hex::decode(tx_str) {
                Ok(b) => b,
                Err(_) => continue,
            };

            self.process_transaction(block_number, &tx_bytes);
        }
    }

    fn process_transaction(&mut self, block_number: u64, tx_bytes: &[u8]) {
        use alloy_consensus::TxEnvelope;
        use alloy_primitives::TxKind;

        let envelope: TxEnvelope = match alloy_rlp::Decodable::decode(&mut &tx_bytes[..]) {
            Ok(env) => env,
            Err(_) => return,
        };

        // Check if this transaction targets the staking address BEFORE recovering
        // the sender (secp256k1 recovery is expensive and most txs are not staking).
        let (to, value, input) = match &envelope {
            TxEnvelope::Legacy(signed) => {
                let tx = signed.tx();
                match tx.to {
                    TxKind::Call(addr) => (addr, tx.value, tx.input.as_ref()),
                    TxKind::Create => return,
                }
            }
            TxEnvelope::Eip2930(signed) => {
                let tx = signed.tx();
                match tx.to {
                    TxKind::Call(addr) => (addr, tx.value, tx.input.as_ref()),
                    TxKind::Create => return,
                }
            }
            TxEnvelope::Eip1559(signed) => {
                let tx = signed.tx();
                match tx.to {
                    TxKind::Call(addr) => (addr, tx.value, tx.input.as_ref()),
                    TxKind::Create => return,
                }
            }
            TxEnvelope::Eip4844(signed) => {
                let tx = signed.tx().tx();
                (tx.to, tx.value, tx.input.as_ref())
            }
            TxEnvelope::Eip7702(signed) => {
                let tx = signed.tx();
                (tx.to, tx.value, tx.input.as_ref())
            }
        };

        if to != STAKING_ADDRESS {
            return;
        }

        // Only recover sender (expensive secp256k1) for staking-targeted transactions.
        let sender = match envelope.recover_signer() {
            Ok(addr) => addr,
            Err(_) => return,
        };

        // Determine operation type
        if !value.is_zero() && input.len() == 48 {
            // Stake: value > 0, data = 48-byte BLS pubkey
            let mut bls_pubkey = [0u8; 48];
            bls_pubkey.copy_from_slice(input);
            self.handle_stake(sender, bls_pubkey, value, block_number);
        } else if value.is_zero() && input.len() >= 4 && input[..4] == UNSTAKE_SELECTOR {
            // Unstake: value = 0, data starts with unstake() selector
            self.handle_unstake(sender, block_number);
        } else if value.is_zero() && input.len() == 48 {
            // Register: value = 0, data = 48-byte BLS pubkey (not unstake selector)
            let mut bls_pubkey = [0u8; 48];
            bls_pubkey.copy_from_slice(input);
            self.handle_register(sender, bls_pubkey, block_number);
        }
    }

    fn handle_stake(
        &mut self,
        staker: Address,
        bls_pubkey: [u8; 48],
        value: U256,
        block_number: u64,
    ) {
        // Fix #5: Reject all-zero BLS pubkey
        if bls_pubkey == [0u8; 48] {
            warn!(
                target: "n42::staking",
                %staker,
                block = block_number,
                "rejecting stake with all-zero BLS pubkey"
            );
            return;
        }

        // Fix #6: Reject if a different staker already owns this BLS pubkey
        if let Some(&existing_staker) = self.bls_to_staker.get(&bls_pubkey)
            && existing_staker != staker
        {
            warn!(
                target: "n42::staking",
                %staker,
                existing_staker = %existing_staker,
                block = block_number,
                "rejecting stake: BLS pubkey already registered to a different staker"
            );
            return;
        }

        if let Some(entry) = self.stakes.get_mut(&staker) {
            // Append stake
            entry.amount = entry.amount.saturating_add(value);
            if entry.bls_pubkey != bls_pubkey {
                // Update BLS pubkey mapping
                self.bls_to_staker.remove(&entry.bls_pubkey);
                let old_reward_addr = bls_pubkey_to_reward_address(&entry.bls_pubkey);
                self.reward_to_staker.remove(&old_reward_addr);

                entry.bls_pubkey = bls_pubkey;
                self.bls_to_staker.insert(bls_pubkey, staker);
                let new_reward_addr = bls_pubkey_to_reward_address(&bls_pubkey);
                self.reward_to_staker.insert(new_reward_addr, staker);
            }
            entry.status = StakeStatus::Active;
            info!(
                target: "n42::staking",
                %staker,
                added = %value,
                total = %entry.amount,
                block = block_number,
                "stake appended"
            );
        } else {
            // Upgrade from registration to stake if applicable
            if let Some(old_reg) = self.registrations.remove(&staker) {
                // Clean up old BLS mappings if the key changed
                if old_reg.bls_pubkey != bls_pubkey {
                    self.bls_to_staker.remove(&old_reg.bls_pubkey);
                    let old_reward_addr = bls_pubkey_to_reward_address(&old_reg.bls_pubkey);
                    self.reward_to_staker.remove(&old_reward_addr);
                }
                info!(
                    target: "n42::staking",
                    %staker,
                    block = block_number,
                    "upgrading registered verifier to staker"
                );
            }
            // New stake
            let entry = StakeEntry {
                staker,
                bls_pubkey,
                amount: value,
                staked_at_block: block_number,
                status: StakeStatus::Active,
            };
            self.stakes.insert(staker, entry);
            self.bls_to_staker.insert(bls_pubkey, staker);
            let reward_addr = bls_pubkey_to_reward_address(&bls_pubkey);
            self.reward_to_staker.insert(reward_addr, staker);
            info!(
                target: "n42::staking",
                %staker,
                amount = %value,
                block = block_number,
                "new stake registered"
            );
        }
    }

    fn handle_unstake(&mut self, staker: Address, block_number: u64) {
        if let Some(entry) = self.stakes.get_mut(&staker) {
            if matches!(entry.status, StakeStatus::Active) {
                entry.status = StakeStatus::Unstaking {
                    initiated_block: block_number,
                };
                info!(
                    target: "n42::staking",
                    %staker,
                    amount = %entry.amount,
                    cooldown_blocks = UNSTAKE_COOLDOWN_BLOCKS,
                    "unstake initiated"
                );
            } else {
                debug!(target: "n42::staking", %staker, "unstake ignored: already unstaking");
            }
        } else {
            debug!(target: "n42::staking", %staker, "unstake ignored: no stake found");
        }
    }

    /// Checks if a BLS public key has an active stake >= MIN_STAKE_WEI.
    pub fn is_staked(&self, bls_pubkey: &[u8; 48]) -> bool {
        if let Some(staker) = self.bls_to_staker.get(bls_pubkey)
            && let Some(entry) = self.stakes.get(staker)
        {
            return matches!(entry.status, StakeStatus::Active)
                && entry.amount >= MIN_STAKE_WEI;
        }
        false
    }

    /// Handles a registration transaction (value=0, data=48-byte BLS pubkey).
    /// Creates a BLS→EVM mapping so rewards are delivered to the correct address.
    fn handle_register(&mut self, sender: Address, bls_pubkey: [u8; 48], block_number: u64) {
        // Reject all-zero BLS pubkey
        if bls_pubkey == [0u8; 48] {
            warn!(
                target: "n42::staking",
                %sender,
                block = block_number,
                "rejecting registration with all-zero BLS pubkey"
            );
            return;
        }

        // Reject if a different address already owns this BLS pubkey
        if let Some(&existing) = self.bls_to_staker.get(&bls_pubkey)
            && existing != sender
        {
            warn!(
                target: "n42::staking",
                %sender,
                existing = %existing,
                block = block_number,
                "rejecting registration: BLS pubkey already owned by another address"
            );
            return;
        }

        // If already staked, registration is a no-op
        if self.stakes.contains_key(&sender) {
            debug!(
                target: "n42::staking",
                %sender,
                block = block_number,
                "registration ignored: address already staked"
            );
            return;
        }

        // If already registered, update BLS pubkey if different
        if let Some(existing_reg) = self.registrations.get_mut(&sender) {
            if existing_reg.bls_pubkey != bls_pubkey {
                self.bls_to_staker.remove(&existing_reg.bls_pubkey);
                let old_reward_addr = bls_pubkey_to_reward_address(&existing_reg.bls_pubkey);
                self.reward_to_staker.remove(&old_reward_addr);

                existing_reg.bls_pubkey = bls_pubkey;
                self.bls_to_staker.insert(bls_pubkey, sender);
                let new_reward_addr = bls_pubkey_to_reward_address(&bls_pubkey);
                self.reward_to_staker.insert(new_reward_addr, sender);
                info!(
                    target: "n42::staking",
                    %sender,
                    block = block_number,
                    "registration BLS pubkey updated"
                );
            }
            return;
        }

        // New registration
        let entry = RegistrationEntry {
            evm_address: sender,
            bls_pubkey,
            registered_at_block: block_number,
        };
        self.registrations.insert(sender, entry);
        self.bls_to_staker.insert(bls_pubkey, sender);
        let reward_addr = bls_pubkey_to_reward_address(&bls_pubkey);
        self.reward_to_staker.insert(reward_addr, sender);
        info!(
            target: "n42::staking",
            %sender,
            block = block_number,
            "new verifier registered (no stake)"
        );
    }

    /// Checks if a BLS public key is registered (staked OR registered).
    /// Used by mobile bridge to authorize verifiers.
    pub fn is_registered(&self, bls_pubkey: &[u8; 48]) -> bool {
        if let Some(addr) = self.bls_to_staker.get(bls_pubkey) {
            // Check if it's an active staker or a registered verifier
            if let Some(entry) = self.stakes.get(addr) {
                return matches!(entry.status, StakeStatus::Active);
            }
            if self.registrations.contains_key(addr) {
                return true;
            }
        }
        false
    }

    /// Returns the set of all BLS pubkeys that have an active stake >= MIN_STAKE_WEI.
    /// Used to determine the reward multiplier for each verifier.
    pub fn staked_bls_pubkeys(&self) -> HashSet<[u8; 48]> {
        self.stakes
            .values()
            .filter(|e| matches!(e.status, StakeStatus::Active) && e.amount >= MIN_STAKE_WEI)
            .map(|e| e.bls_pubkey)
            .collect()
    }

    /// Returns a registration entry by EVM address.
    pub fn get_registration(&self, address: &Address) -> Option<&RegistrationEntry> {
        self.registrations.get(address)
    }

    /// Returns the count of pure registrations (not staked).
    pub fn registration_count(&self) -> usize {
        self.registrations.len()
    }

    /// Checks cooldown expirations and generates return withdrawals.
    pub fn check_cooldowns(&mut self, current_block: u64) {
        let mut matured = Vec::new();
        for (addr, entry) in &self.stakes {
            if let StakeStatus::Unstaking { initiated_block } = entry.status
                && current_block >= initiated_block + UNSTAKE_COOLDOWN_BLOCKS
            {
                matured.push(*addr);
            }
        }

        for addr in matured {
            if let Some(entry) = self.stakes.remove(&addr) {
                self.bls_to_staker.remove(&entry.bls_pubkey);
                let reward_addr = bls_pubkey_to_reward_address(&entry.bls_pubkey);
                self.reward_to_staker.remove(&reward_addr);

                // Convert amount to gwei for Withdrawal.amount field
                // Withdrawal.amount is in Gwei (10^9 wei)
                let amount_gwei = entry.amount / U256::from(1_000_000_000u64);
                let amount_gwei_u64 = if amount_gwei > U256::from(u64::MAX) {
                    u64::MAX
                } else {
                    amount_gwei.to::<u64>()
                };

                // Fix #8: Use monotonic withdrawal index
                let idx = self.next_withdrawal_index;
                self.next_withdrawal_index += 1;

                self.pending_returns.push_back(Withdrawal {
                    index: idx,
                    validator_index: 0,
                    address: entry.staker,
                    amount: amount_gwei_u64,
                });

                info!(
                    target: "n42::staking",
                    staker = %entry.staker,
                    amount_gwei = amount_gwei_u64,
                    withdrawal_index = idx,
                    "unstake cooldown expired, return queued"
                );
            }
        }
    }

    /// Takes pending return withdrawals for injection into payload_attributes.
    pub fn take_pending_returns(&mut self, _block_number: u64, max: usize) -> Vec<Withdrawal> {
        let count = max.min(self.pending_returns.len());
        let mut results = Vec::with_capacity(count);
        for _ in 0..count {
            if let Some(w) = self.pending_returns.pop_front() {
                results.push(w);
            }
        }
        results
    }

    /// Maps a BLS public key to the staker's EVM address (for reward address resolution).
    pub fn staker_address(&self, bls_pubkey: &[u8; 48]) -> Option<Address> {
        self.bls_to_staker.get(bls_pubkey).copied()
    }

    /// Maps a reward address (BLS-derived keccak) to the staker's actual EVM address.
    pub fn staker_address_by_reward(&self, reward_addr: Address) -> Option<Address> {
        self.reward_to_staker.get(&reward_addr).copied()
    }

    /// Returns a staker's entry by EVM address.
    pub fn get_stake(&self, address: &Address) -> Option<&StakeEntry> {
        self.stakes.get(address)
    }

    /// Returns total staked amount and staker count.
    pub fn total_staked(&self) -> (U256, u64) {
        let mut total = U256::ZERO;
        let mut count = 0u64;
        for entry in self.stakes.values() {
            if matches!(entry.status, StakeStatus::Active) {
                total = total.saturating_add(entry.amount);
                count += 1;
            }
        }
        (total, count)
    }

    /// Returns the most recently scanned block number (Fix #2).
    pub fn last_scanned_block(&self) -> u64 {
        self.last_scanned_block
    }
}

/// Derives a reward address from a BLS public key using keccak256.
/// This matches MobileRewardManager's `bls_pubkey_to_address` logic.
fn bls_pubkey_to_reward_address(pubkey: &[u8; 48]) -> Address {
    n42_mobile::bls_pubkey_to_address(pubkey)
}

// ── Serialization helpers ──

mod u256_dec {
    use alloy_primitives::U256;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(val: &U256, s: S) -> Result<S::Ok, S::Error> {
        format!("{val}").serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<U256, D::Error> {
        let s = String::deserialize(d)?;
        U256::from_str_radix(&s, 10).map_err(serde::de::Error::custom)
    }
}

mod hex_bytes_48 {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(val: &[u8; 48], s: S) -> Result<S::Ok, S::Error> {
        hex::encode(val).serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 48], D::Error> {
        let s = String::deserialize(d)?;
        let bytes = hex::decode(&s).map_err(serde::de::Error::custom)?;
        bytes
            .try_into()
            .map_err(|v: Vec<u8>| serde::de::Error::custom(format!("expected 48 bytes, got {}", v.len())))
    }
}

/// Serializable withdrawal for persistence (avoids dependency on alloy serde feature).
#[derive(serde::Serialize, serde::Deserialize)]
struct PersistedWithdrawal {
    index: u64,
    validator_index: u64,
    address: String,
    amount: u64,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct PersistedStakingState {
    stakes: Vec<StakeEntry>,
    /// Pure registrations (no stake).
    #[serde(default)]
    registrations: Vec<RegistrationEntry>,
    /// Pending return withdrawals awaiting injection (Fix #1).
    #[serde(default)]
    pending_returns: Vec<PersistedWithdrawal>,
    /// Last scanned block number for RPC reference.
    #[serde(default)]
    last_scanned_block: u64,
    /// Monotonic withdrawal index counter (Fix #8).
    #[serde(default = "default_withdrawal_index")]
    next_withdrawal_index: u64,
}

fn default_withdrawal_index() -> u64 {
    1_000_000
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, U256};
    use tempfile::NamedTempFile;

    fn make_bls_pubkey(seed: u8) -> [u8; 48] {
        let mut key = [0u8; 48];
        key[0] = seed;
        key[47] = seed;
        key
    }

    fn ether(n: u64) -> U256 {
        U256::from(n) * U256::from(1_000_000_000_000_000_000u64)
    }

    #[test]
    fn test_min_stake_constant() {
        assert_eq!(MIN_STAKE_WEI, ether(32));
    }

    #[test]
    fn test_stake_and_query() {
        let mut mgr = StakingManager::new();
        let staker = Address::with_last_byte(0x01);
        let bls = make_bls_pubkey(1);

        mgr.handle_stake(staker, bls, ether(32), 100);

        assert!(mgr.is_staked(&bls));
        assert_eq!(mgr.staker_address(&bls), Some(staker));
        assert_eq!(mgr.get_stake(&staker).unwrap().amount, ether(32));
    }

    #[test]
    fn test_stake_below_minimum() {
        let mut mgr = StakingManager::new();
        let staker = Address::with_last_byte(0x02);
        let bls = make_bls_pubkey(2);

        mgr.handle_stake(staker, bls, ether(31), 100);
        assert!(!mgr.is_staked(&bls), "31 N should not qualify");
    }

    #[test]
    fn test_append_stake() {
        let mut mgr = StakingManager::new();
        let staker = Address::with_last_byte(0x03);
        let bls = make_bls_pubkey(3);

        mgr.handle_stake(staker, bls, ether(20), 100);
        assert!(!mgr.is_staked(&bls));

        mgr.handle_stake(staker, bls, ether(15), 110);
        assert!(mgr.is_staked(&bls));
        assert_eq!(mgr.get_stake(&staker).unwrap().amount, ether(35));
    }

    #[test]
    fn test_unstake_and_cooldown() {
        let mut mgr = StakingManager::new();
        let staker = Address::with_last_byte(0x04);
        let bls = make_bls_pubkey(4);

        mgr.handle_stake(staker, bls, ether(32), 100);
        assert!(mgr.is_staked(&bls));

        mgr.handle_unstake(staker, 200);
        assert!(!mgr.is_staked(&bls));
        assert!(matches!(
            mgr.get_stake(&staker).unwrap().status,
            StakeStatus::Unstaking { initiated_block: 200 }
        ));

        // Before cooldown
        mgr.check_cooldowns(200 + UNSTAKE_COOLDOWN_BLOCKS - 1);
        assert!(mgr.get_stake(&staker).is_some());
        assert!(mgr.pending_returns.is_empty());

        // After cooldown
        mgr.check_cooldowns(200 + UNSTAKE_COOLDOWN_BLOCKS);
        assert!(mgr.get_stake(&staker).is_none());
        assert_eq!(mgr.pending_returns.len(), 1);

        let returns = mgr.take_pending_returns(500, 8);
        assert_eq!(returns.len(), 1);
        assert_eq!(returns[0].address, staker);
        // 32 ether = 32 * 10^9 gwei
        assert_eq!(returns[0].amount, 32_000_000_000u64);
    }

    #[test]
    fn test_reward_address_resolution() {
        let mut mgr = StakingManager::new();
        let staker = Address::with_last_byte(0x05);
        let bls = make_bls_pubkey(5);

        mgr.handle_stake(staker, bls, ether(32), 100);

        let reward_addr = bls_pubkey_to_reward_address(&bls);
        assert_eq!(mgr.staker_address_by_reward(reward_addr), Some(staker));
        assert_ne!(reward_addr, staker, "reward addr should differ from staker addr");
    }

    #[test]
    fn test_total_staked() {
        let mut mgr = StakingManager::new();

        mgr.handle_stake(Address::with_last_byte(1), make_bls_pubkey(1), ether(32), 100);
        mgr.handle_stake(Address::with_last_byte(2), make_bls_pubkey(2), ether(64), 100);

        let (total, count) = mgr.total_staked();
        assert_eq!(count, 2);
        assert_eq!(total, ether(96));
    }

    #[test]
    fn test_persistence() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();

        {
            let mut mgr = StakingManager::new();
            mgr.state_file = Some(path.clone());
            mgr.handle_stake(Address::with_last_byte(1), make_bls_pubkey(1), ether(50), 100);
            mgr.save();
        }

        let mgr = StakingManager::load_or_new(&path);
        assert_eq!(mgr.stakes.len(), 1);
        assert!(mgr.is_staked(&make_bls_pubkey(1)));
        assert_eq!(
            mgr.get_stake(&Address::with_last_byte(1)).unwrap().amount,
            ether(50)
        );
    }

    #[test]
    fn test_persistence_with_pending_returns() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();

        {
            let mut mgr = StakingManager::new();
            mgr.state_file = Some(path.clone());
            mgr.handle_stake(Address::with_last_byte(1), make_bls_pubkey(1), ether(32), 100);
            mgr.handle_unstake(Address::with_last_byte(1), 200);
            mgr.check_cooldowns(200 + UNSTAKE_COOLDOWN_BLOCKS);
            assert_eq!(mgr.pending_returns.len(), 1);
            mgr.save();
        }

        let mgr = StakingManager::load_or_new(&path);
        assert_eq!(mgr.pending_returns.len(), 1, "pending_returns must survive restart");
        assert_eq!(mgr.pending_returns[0].address, Address::with_last_byte(1));
        assert_eq!(mgr.pending_returns[0].amount, 32_000_000_000u64);
    }

    #[test]
    fn test_unstake_selector() {
        // Verify unstake() selector matches keccak256("unstake()")[..4]
        let hash = alloy_primitives::keccak256(b"unstake()");
        assert_eq!(&hash[..4], &UNSTAKE_SELECTOR);
    }

    #[test]
    fn test_double_unstake_ignored() {
        let mut mgr = StakingManager::new();
        let staker = Address::with_last_byte(0x06);
        let bls = make_bls_pubkey(6);

        mgr.handle_stake(staker, bls, ether(32), 100);
        mgr.handle_unstake(staker, 200);
        mgr.handle_unstake(staker, 300); // Should be ignored

        if let StakeStatus::Unstaking { initiated_block } = mgr.get_stake(&staker).unwrap().status
        {
            assert_eq!(initiated_block, 200, "second unstake should not update block");
        } else {
            panic!("expected Unstaking status");
        }
    }

    #[test]
    fn test_restake_after_unstaking() {
        let mut mgr = StakingManager::new();
        let staker = Address::with_last_byte(0x07);
        let bls = make_bls_pubkey(7);

        mgr.handle_stake(staker, bls, ether(32), 100);
        mgr.handle_unstake(staker, 200);
        assert!(!mgr.is_staked(&bls));

        // Re-stake while unstaking resets status to Active
        mgr.handle_stake(staker, bls, ether(10), 250);
        assert!(mgr.is_staked(&bls));
        assert_eq!(mgr.get_stake(&staker).unwrap().amount, ether(42));
    }

    #[test]
    fn test_unstake_no_stake_ignored() {
        let mut mgr = StakingManager::new();
        mgr.handle_unstake(Address::with_last_byte(0x08), 100);
        assert!(mgr.stakes.is_empty());
    }

    #[test]
    fn test_all_zero_bls_pubkey_rejected() {
        let mut mgr = StakingManager::new();
        let staker = Address::with_last_byte(0x09);
        let zero_bls = [0u8; 48];

        mgr.handle_stake(staker, zero_bls, ether(32), 100);
        assert!(mgr.stakes.is_empty(), "all-zero BLS pubkey must be rejected");
    }

    #[test]
    fn test_bls_pubkey_conflict_rejected() {
        let mut mgr = StakingManager::new();
        let staker_a = Address::with_last_byte(0x0A);
        let staker_b = Address::with_last_byte(0x0B);
        let bls = make_bls_pubkey(10);

        mgr.handle_stake(staker_a, bls, ether(32), 100);
        assert!(mgr.is_staked(&bls));

        // Different staker trying to use the same BLS pubkey
        mgr.handle_stake(staker_b, bls, ether(32), 110);
        assert!(
            mgr.get_stake(&staker_b).is_none(),
            "conflicting BLS pubkey must be rejected"
        );
        assert_eq!(
            mgr.staker_address(&bls),
            Some(staker_a),
            "original staker must retain ownership"
        );
    }

    #[test]
    fn test_monotonic_withdrawal_index() {
        let mut mgr = StakingManager::new();

        mgr.handle_stake(Address::with_last_byte(1), make_bls_pubkey(1), ether(32), 100);
        mgr.handle_stake(Address::with_last_byte(2), make_bls_pubkey(2), ether(32), 100);

        mgr.handle_unstake(Address::with_last_byte(1), 200);
        mgr.handle_unstake(Address::with_last_byte(2), 201);

        mgr.check_cooldowns(200 + UNSTAKE_COOLDOWN_BLOCKS + 1);
        let returns = mgr.take_pending_returns(500, 8);
        assert_eq!(returns.len(), 2);
        assert!(returns[0].index < returns[1].index, "withdrawal indices must be monotonically increasing");
        assert_ne!(returns[0].index, returns[1].index);
    }

    #[test]
    fn test_last_scanned_block() {
        let mut mgr = StakingManager::new();
        assert_eq!(mgr.last_scanned_block(), 0);

        // Simulate scanning with empty payload
        let empty_payload = b"{}";
        mgr.scan_committed_block(42, empty_payload);
        assert_eq!(mgr.last_scanned_block(), 42);

        mgr.scan_committed_block(100, empty_payload);
        assert_eq!(mgr.last_scanned_block(), 100);
    }

    // ── Registration tests ──

    #[test]
    fn test_register_and_is_registered() {
        let mut mgr = StakingManager::new();
        let sender = Address::with_last_byte(0x20);
        let bls = make_bls_pubkey(20);

        mgr.handle_register(sender, bls, 100);

        assert!(mgr.is_registered(&bls), "registered pubkey should pass is_registered");
        assert!(!mgr.is_staked(&bls), "registered-only pubkey should not pass is_staked");
        assert_eq!(mgr.staker_address(&bls), Some(sender));
        assert!(mgr.get_registration(&sender).is_some());
        assert_eq!(mgr.registration_count(), 1);
    }

    #[test]
    fn test_register_zero_bls_rejected() {
        let mut mgr = StakingManager::new();
        mgr.handle_register(Address::with_last_byte(0x21), [0u8; 48], 100);
        assert_eq!(mgr.registration_count(), 0);
    }

    #[test]
    fn test_register_bls_conflict_rejected() {
        let mut mgr = StakingManager::new();
        let bls = make_bls_pubkey(22);
        let addr_a = Address::with_last_byte(0x22);
        let addr_b = Address::with_last_byte(0x23);

        mgr.handle_register(addr_a, bls, 100);
        mgr.handle_register(addr_b, bls, 110); // conflict

        assert_eq!(mgr.registration_count(), 1);
        assert_eq!(mgr.staker_address(&bls), Some(addr_a));
    }

    #[test]
    fn test_register_upgrade_to_stake() {
        let mut mgr = StakingManager::new();
        let addr = Address::with_last_byte(0x24);
        let bls = make_bls_pubkey(24);

        // Register first
        mgr.handle_register(addr, bls, 100);
        assert!(mgr.is_registered(&bls));
        assert!(!mgr.is_staked(&bls));
        assert_eq!(mgr.registration_count(), 1);

        // Now stake — should upgrade
        mgr.handle_stake(addr, bls, ether(32), 200);
        assert!(mgr.is_staked(&bls));
        assert!(mgr.is_registered(&bls)); // staked also counts as registered
        assert_eq!(mgr.registration_count(), 0, "registration should be removed after stake upgrade");
        assert!(mgr.get_registration(&addr).is_none());
        assert!(mgr.get_stake(&addr).is_some());
    }

    #[test]
    fn test_register_noop_if_already_staked() {
        let mut mgr = StakingManager::new();
        let addr = Address::with_last_byte(0x25);
        let bls = make_bls_pubkey(25);

        mgr.handle_stake(addr, bls, ether(32), 100);
        mgr.handle_register(addr, bls, 200); // should be ignored

        assert_eq!(mgr.registration_count(), 0);
        assert!(mgr.is_staked(&bls));
    }

    #[test]
    fn test_staked_bls_pubkeys() {
        let mut mgr = StakingManager::new();
        let bls1 = make_bls_pubkey(30);
        let bls2 = make_bls_pubkey(31);
        let bls3 = make_bls_pubkey(32);

        mgr.handle_stake(Address::with_last_byte(0x30), bls1, ether(32), 100);
        mgr.handle_stake(Address::with_last_byte(0x31), bls2, ether(10), 100); // below minimum
        mgr.handle_register(Address::with_last_byte(0x32), bls3, 100); // registered only

        let staked = mgr.staked_bls_pubkeys();
        assert_eq!(staked.len(), 1);
        assert!(staked.contains(&bls1));
        assert!(!staked.contains(&bls2));
        assert!(!staked.contains(&bls3));
    }

    #[test]
    fn test_register_reward_address_resolution() {
        let mut mgr = StakingManager::new();
        let addr = Address::with_last_byte(0x33);
        let bls = make_bls_pubkey(33);

        mgr.handle_register(addr, bls, 100);

        let reward_addr = bls_pubkey_to_reward_address(&bls);
        assert_eq!(mgr.staker_address_by_reward(reward_addr), Some(addr));
    }

    #[test]
    fn test_register_upgrade_with_different_bls_key() {
        let mut mgr = StakingManager::new();
        let addr = Address::with_last_byte(0x36);
        let bls_a = make_bls_pubkey(36);
        let bls_b = make_bls_pubkey(37);

        // Register with BLS key A
        mgr.handle_register(addr, bls_a, 100);
        assert!(mgr.is_registered(&bls_a));

        // Stake with DIFFERENT BLS key B — should clean up key A mappings
        mgr.handle_stake(addr, bls_b, ether(32), 200);
        assert!(mgr.is_staked(&bls_b));
        assert!(!mgr.is_registered(&bls_a), "old BLS key must be cleaned up");
        assert!(mgr.staker_address(&bls_a).is_none(), "old BLS mapping must be removed");

        // Another user should be able to register the old BLS key A
        let addr2 = Address::with_last_byte(0x37);
        mgr.handle_register(addr2, bls_a, 300);
        assert!(mgr.is_registered(&bls_a));
        assert_eq!(mgr.staker_address(&bls_a), Some(addr2));
    }

    #[test]
    fn test_registration_persistence() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();

        {
            let mut mgr = StakingManager::new();
            mgr.state_file = Some(path.clone());
            mgr.handle_register(Address::with_last_byte(0x34), make_bls_pubkey(34), 100);
            mgr.handle_stake(Address::with_last_byte(0x35), make_bls_pubkey(35), ether(32), 100);
            mgr.save();
        }

        let mgr = StakingManager::load_or_new(&path);
        assert_eq!(mgr.registration_count(), 1);
        assert!(mgr.is_registered(&make_bls_pubkey(34)));
        assert!(mgr.is_staked(&make_bls_pubkey(35)));
    }
}
