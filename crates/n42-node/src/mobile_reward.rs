use alloy_eips::eip4895::Withdrawal;
use alloy_primitives::Address;
use std::collections::{HashMap, VecDeque};
use tracing::{debug, info, warn};

/// Maximum pending rewards in the queue to prevent unbounded memory growth.
/// At 32 rewards/block, 1M entries would take ~31,250 blocks (~35 hours) to drain.
const MAX_REWARD_QUEUE_SIZE: usize = 1_000_000;

/// Manages mobile verification rewards using EIP-4895 Withdrawals.
///
/// Tracks attestation counts per epoch (keyed by BLS pubkey hex), computes
/// rewards at epoch boundaries, and dispenses them in bounded batches via
/// the Withdrawal mechanism (direct balance increase, no transaction needed).
///
/// Design decisions:
/// - Uses EIP-4895 Withdrawals: no gas, no nonce, no signature required.
///   reth's block executor handles them as direct balance additions.
/// - BLS pubkey -> ETH address: `keccak256(pubkey_bytes)[12..]` (deterministic).
/// - Max 32 rewards per block to keep block size bounded at scale (1M+ verifiers).
/// - Logarithmic reward curve: reward(n) = R_max * ln(1 + k*n) / ln(1 + k*N)
///   where n = attestation count, N = blocks_per_epoch, k = curve steepness.
///   This means 5 minutes of verification yields ~50% of daily reward,
///   discouraging 24/7 operation on mobile devices.
pub struct MobileRewardManager {
    /// Current epoch's attestation counts: bls_pubkey_hex -> count.
    epoch_attestations: HashMap<String, u64>,
    /// Pending rewards queue (Withdrawal format).
    reward_queue: VecDeque<Withdrawal>,
    /// Block number of the last epoch boundary distribution.
    last_distribution_block: u64,
    /// Number of blocks per epoch.
    blocks_per_epoch: u64,
    /// Full epoch reward in Gwei (0.1 N42 = 100_000_000 Gwei for 24h continuous verification).
    daily_base_reward_gwei: u64,
    /// Logarithmic curve steepness parameter. Higher k = steeper early gains.
    curve_k: f64,
    /// Maximum rewards to inject per block.
    max_rewards_per_block: usize,
}

impl MobileRewardManager {
    pub fn new(
        blocks_per_epoch: u64,
        daily_base_reward_gwei: u64,
        curve_k: f64,
        max_per_block: usize,
    ) -> Self {
        Self {
            epoch_attestations: HashMap::new(),
            reward_queue: VecDeque::new(),
            last_distribution_block: 0,
            blocks_per_epoch,
            daily_base_reward_gwei,
            curve_k,
            max_rewards_per_block: max_per_block,
        }
    }

    /// Records a valid attestation from a mobile verifier.
    pub fn record_attestation(&mut self, pubkey_hex: &str) {
        *self.epoch_attestations.entry(pubkey_hex.to_string()).or_insert(0) += 1;
        let short_key = if pubkey_hex.len() >= 16 { &pubkey_hex[..16] } else { pubkey_hex };
        debug!(
            target: "n42::reward",
            pubkey = short_key,
            count = self.epoch_attestations[pubkey_hex],
            "attestation recorded"
        );
    }

    /// Checks if the current block is at an epoch boundary. If so, computes
    /// rewards for all attestors and enqueues them.
    pub fn check_epoch_boundary(&mut self, current_block: u64) {
        if self.blocks_per_epoch == 0 {
            return;
        }

        // Check if we've crossed an epoch boundary since last distribution
        let current_epoch = current_block / self.blocks_per_epoch;
        let last_epoch = self.last_distribution_block / self.blocks_per_epoch;

        if current_epoch <= last_epoch && self.last_distribution_block > 0 {
            return;
        }

        // Not yet reached the first epoch boundary
        if current_block < self.blocks_per_epoch {
            return;
        }

        if self.epoch_attestations.is_empty() {
            debug!(target: "n42::reward", current_block, "epoch boundary but no attestations");
            self.last_distribution_block = current_block;
            return;
        }

        // Check queue capacity before adding more rewards
        if self.reward_queue.len() >= MAX_REWARD_QUEUE_SIZE {
            warn!(
                target: "n42::reward",
                current_block,
                queue_size = self.reward_queue.len(),
                "reward queue at capacity, skipping epoch distribution until queue drains"
            );
            return;
        }

        let validator_count = self.epoch_attestations.len();
        let mut total_reward_gwei = 0u64;

        // Cache fields needed for log reward computation to avoid borrow conflict with drain().
        let blocks_per_epoch = self.blocks_per_epoch;
        let daily_base_reward_gwei = self.daily_base_reward_gwei;
        let curve_k = self.curve_k;

        // Compute rewards for each attestor (cap additions to stay within queue limit)
        let remaining_capacity = MAX_REWARD_QUEUE_SIZE - self.reward_queue.len();
        let mut added = 0usize;
        for (pubkey_hex, count) in self.epoch_attestations.drain() {
            if added >= remaining_capacity {
                warn!(
                    target: "n42::reward",
                    added,
                    remaining_capacity,
                    "reward queue capacity reached during epoch distribution"
                );
                break;
            }

            let reward_gwei = compute_log_reward_inner(
                count, blocks_per_epoch, daily_base_reward_gwei, curve_k,
            );
            total_reward_gwei = total_reward_gwei.saturating_add(reward_gwei);

            let address = bls_pubkey_hex_to_address(&pubkey_hex);
            let withdrawal = Withdrawal {
                index: 0, // placeholder; real index assigned in take_pending_rewards()
                validator_index: 0, // not used in our context
                address,
                amount: reward_gwei,
            };
            self.reward_queue.push_back(withdrawal);
            added += 1;
        }

        info!(
            target: "n42::reward",
            current_block,
            epoch = current_epoch,
            validator_count,
            total_reward_gwei,
            queue_size = self.reward_queue.len(),
            "epoch reward distribution computed"
        );

        self.last_distribution_block = current_block;
    }

    /// Takes up to `max_rewards_per_block` pending rewards for inclusion in the next block.
    ///
    /// Withdrawal indices are computed as `block_number * max_rewards_per_block + i`
    /// to guarantee global uniqueness across all blocks, regardless of which validator
    /// proposes the block. Each validator maintains its own MobileRewardManager, so
    /// using block_number (which is unique) avoids index collisions.
    pub fn take_pending_rewards(&mut self, block_number: u64) -> Vec<Withdrawal> {
        let count = self.reward_queue.len().min(self.max_rewards_per_block);
        let mut rewards: Vec<Withdrawal> = self.reward_queue.drain(..count).collect();

        // Assign globally unique withdrawal indices based on block number.
        let base_index = block_number.saturating_mul(self.max_rewards_per_block as u64);
        for (i, w) in rewards.iter_mut().enumerate() {
            w.index = base_index + i as u64;
        }

        if !rewards.is_empty() {
            debug!(
                target: "n42::reward",
                count = rewards.len(),
                remaining = self.reward_queue.len(),
                block_number,
                first_index = base_index,
                "dispensing rewards into block"
            );
        }

        rewards
    }

    /// Returns the number of pending rewards in the queue.
    pub fn pending_count(&self) -> usize {
        self.reward_queue.len()
    }

    /// Returns the number of unique attestors in the current epoch.
    pub fn epoch_attestation_count(&self) -> usize {
        self.epoch_attestations.len()
    }
}

/// Computes logarithmic curve reward.
///
/// `reward(n) = R_max * ln(1 + k*n) / ln(1 + k*N)`
///
/// where n = attestation_count, N = blocks_per_epoch, R_max = daily_base_reward_gwei.
/// The curve ensures 5 minutes (~75 blocks at 4s) yields ~50% of the daily reward,
/// while 24 hours yields 100%.
fn compute_log_reward_inner(
    attestation_count: u64,
    blocks_per_epoch: u64,
    daily_base_reward_gwei: u64,
    curve_k: f64,
) -> u64 {
    if attestation_count == 0 || blocks_per_epoch == 0 {
        return 0;
    }
    let n = attestation_count.min(blocks_per_epoch) as f64;
    let big_n = blocks_per_epoch as f64;
    let k = curve_k;

    let numerator = (1.0 + k * n).ln();
    let denominator = (1.0 + k * big_n).ln();

    let ratio = numerator / denominator;
    let reward = (daily_base_reward_gwei as f64) * ratio;

    (reward as u64).min(daily_base_reward_gwei)
}

/// Converts a BLS public key hex string to an ETH address.
///
/// Delegates to `n42_mobile::bls_pubkey_to_address` for the actual derivation.
/// Returns `Address::ZERO` if the hex string is invalid or not exactly 48 bytes.
fn bls_pubkey_hex_to_address(pubkey_hex: &str) -> Address {
    match hex::decode(pubkey_hex) {
        Ok(pubkey_bytes) if pubkey_bytes.len() == 48 => {
            let pubkey: [u8; 48] = pubkey_bytes.try_into().unwrap();
            n42_mobile::bls_pubkey_to_address(&pubkey)
        }
        _ => {
            tracing::warn!(
                target: "n42::reward",
                pubkey_hex_len = pubkey_hex.len(),
                "invalid BLS pubkey hex, returning zero address"
            );
            Address::ZERO
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_pubkey_hex(idx: u8) -> String {
        hex::encode([idx; 48])
    }

    /// Helper: compute expected log reward for assertions.
    fn expected_log_reward(n: u64, big_n: u64, r_max: u64, k: f64) -> u64 {
        if n == 0 || big_n == 0 {
            return 0;
        }
        let n = n.min(big_n) as f64;
        let big_n = big_n as f64;
        let ratio = (1.0 + k * n).ln() / (1.0 + k * big_n).ln();
        ((r_max as f64) * ratio) as u64
    }

    #[test]
    fn test_record_attestation() {
        let mut mgr = MobileRewardManager::new(100, 100_000_000, 4.0, 32);
        let pk = test_pubkey_hex(1);

        mgr.record_attestation(&pk);
        assert_eq!(mgr.epoch_attestation_count(), 1);
        assert_eq!(mgr.epoch_attestations[&pk], 1);

        mgr.record_attestation(&pk);
        assert_eq!(mgr.epoch_attestations[&pk], 2);
    }

    #[test]
    fn test_epoch_boundary_no_early_distribution() {
        let mut mgr = MobileRewardManager::new(100, 100_000_000, 4.0, 32);
        let pk = test_pubkey_hex(1);
        mgr.record_attestation(&pk);

        // Block 50: not at epoch boundary
        mgr.check_epoch_boundary(50);
        assert_eq!(mgr.pending_count(), 0);
        assert_eq!(mgr.epoch_attestation_count(), 1);
    }

    #[test]
    fn test_epoch_boundary_distribution() {
        let mut mgr = MobileRewardManager::new(100, 100_000_000, 4.0, 32);

        let pk1 = test_pubkey_hex(1);
        let pk2 = test_pubkey_hex(2);

        for _ in 0..10 {
            mgr.record_attestation(&pk1);
        }
        for _ in 0..5 {
            mgr.record_attestation(&pk2);
        }

        // Block 100: epoch boundary
        mgr.check_epoch_boundary(100);

        assert_eq!(mgr.pending_count(), 2);
        assert_eq!(mgr.epoch_attestation_count(), 0); // cleared after distribution

        let rewards = mgr.take_pending_rewards(100);
        assert_eq!(rewards.len(), 2);

        // Log curve: pk1 (10/100) and pk2 (5/100) rewards
        let expected_pk1 = expected_log_reward(10, 100, 100_000_000, 4.0);
        let expected_pk2 = expected_log_reward(5, 100, 100_000_000, 4.0);
        let total: u64 = rewards.iter().map(|w| w.amount).sum();
        assert_eq!(total, expected_pk1 + expected_pk2);

        // pk1 should earn more than pk2
        let amounts: Vec<u64> = rewards.iter().map(|w| w.amount).collect();
        assert!(amounts.contains(&expected_pk1));
        assert!(amounts.contains(&expected_pk2));

        // Indices should be based on block_number * max_per_block
        assert_eq!(rewards[0].index, 100 * 32);
        assert_eq!(rewards[1].index, 100 * 32 + 1);
    }

    #[test]
    fn test_take_pending_bounded() {
        let mut mgr = MobileRewardManager::new(100, 100_000_000, 4.0, 2);

        for i in 0..5u8 {
            let pk = test_pubkey_hex(i);
            mgr.record_attestation(&pk);
        }

        mgr.check_epoch_boundary(100);
        assert_eq!(mgr.pending_count(), 5);

        let batch1 = mgr.take_pending_rewards(100);
        assert_eq!(batch1.len(), 2);
        assert_eq!(mgr.pending_count(), 3);

        let batch2 = mgr.take_pending_rewards(101);
        assert_eq!(batch2.len(), 2);
        assert_eq!(mgr.pending_count(), 1);

        let batch3 = mgr.take_pending_rewards(102);
        assert_eq!(batch3.len(), 1);
        assert_eq!(mgr.pending_count(), 0);
    }

    #[test]
    fn test_no_double_distribution() {
        let mut mgr = MobileRewardManager::new(100, 100_000_000, 4.0, 32);

        let pk = test_pubkey_hex(1);
        mgr.record_attestation(&pk);

        mgr.check_epoch_boundary(100);
        assert_eq!(mgr.pending_count(), 1);

        // Same block shouldn't trigger again
        mgr.record_attestation(&test_pubkey_hex(2));
        mgr.check_epoch_boundary(100);
        assert_eq!(mgr.pending_count(), 1); // no new rewards added
    }

    #[test]
    fn test_withdrawal_index_block_based() {
        let mut mgr = MobileRewardManager::new(100, 100_000_000, 4.0, 32);

        for i in 0..3u8 {
            mgr.record_attestation(&test_pubkey_hex(i));
        }
        mgr.check_epoch_boundary(100);

        let rewards = mgr.take_pending_rewards(100);
        let indices: Vec<u64> = rewards.iter().map(|w| w.index).collect();
        // Indices should be block_number * max_per_block + offset
        assert_eq!(indices, vec![100 * 32, 100 * 32 + 1, 100 * 32 + 2]);

        // Next epoch - different block, different index range
        mgr.record_attestation(&test_pubkey_hex(10));
        mgr.check_epoch_boundary(200);

        let rewards2 = mgr.take_pending_rewards(200);
        assert_eq!(rewards2[0].index, 200 * 32); // based on block 200
    }

    #[test]
    fn test_bls_pubkey_to_address_deterministic() {
        let addr1 = bls_pubkey_hex_to_address(&test_pubkey_hex(1));
        let addr2 = bls_pubkey_hex_to_address(&test_pubkey_hex(1));
        assert_eq!(addr1, addr2);

        let addr3 = bls_pubkey_hex_to_address(&test_pubkey_hex(2));
        assert_ne!(addr1, addr3);
    }

    #[test]
    fn test_empty_epoch() {
        let mut mgr = MobileRewardManager::new(100, 100_000_000, 4.0, 32);
        mgr.check_epoch_boundary(100);
        assert_eq!(mgr.pending_count(), 0);
    }

    #[test]
    fn test_short_pubkey_hex_no_panic() {
        let mut mgr = MobileRewardManager::new(100, 100_000_000, 4.0, 32);
        // Short strings (< 16 chars) should not panic
        mgr.record_attestation("ab");
        mgr.record_attestation("");
        mgr.record_attestation("0123456789abcde"); // exactly 15 chars
        assert_eq!(mgr.epoch_attestation_count(), 3);
    }

    #[test]
    fn test_invalid_hex_address_returns_zero() {
        // Invalid hex should produce Address::ZERO
        let addr = bls_pubkey_hex_to_address("not_valid_hex!!!");
        assert_eq!(addr, Address::ZERO);

        // Empty string should produce Address::ZERO
        let addr2 = bls_pubkey_hex_to_address("");
        assert_eq!(addr2, Address::ZERO);
    }

    #[test]
    fn test_zero_blocks_per_epoch_no_distribution() {
        let mut mgr = MobileRewardManager::new(0, 100_000_000, 4.0, 32);
        let pk = test_pubkey_hex(1);
        mgr.record_attestation(&pk);

        // Should return early without distributing
        mgr.check_epoch_boundary(100);
        assert_eq!(mgr.pending_count(), 0);
        assert_eq!(mgr.epoch_attestation_count(), 1);
    }

    #[test]
    fn test_take_empty_queue_returns_empty() {
        let mut mgr = MobileRewardManager::new(100, 100_000_000, 4.0, 32);
        let rewards = mgr.take_pending_rewards(1);
        assert!(rewards.is_empty());
    }

    #[test]
    fn test_multi_epoch_drain_across_blocks() {
        let mut mgr = MobileRewardManager::new(10, 1_000_000, 4.0, 2);

        // Epoch 1: 3 validators
        for i in 0..3u8 {
            mgr.record_attestation(&test_pubkey_hex(i));
        }
        mgr.check_epoch_boundary(10);
        assert_eq!(mgr.pending_count(), 3);

        // Drain partial: take 2
        let batch1 = mgr.take_pending_rewards(10);
        assert_eq!(batch1.len(), 2);
        assert_eq!(mgr.pending_count(), 1);

        // Epoch 2: 2 more validators (queue should grow)
        for i in 10..12u8 {
            mgr.record_attestation(&test_pubkey_hex(i));
        }
        mgr.check_epoch_boundary(20);
        assert_eq!(mgr.pending_count(), 3); // 1 leftover + 2 new

        // Drain all
        let batch2 = mgr.take_pending_rewards(20);
        assert_eq!(batch2.len(), 2);
        let batch3 = mgr.take_pending_rewards(21);
        assert_eq!(batch3.len(), 1);
        assert_eq!(mgr.pending_count(), 0);
    }

    #[test]
    fn test_reward_address_matches_expected_derivation() {
        let pk = test_pubkey_hex(42);
        let addr = bls_pubkey_hex_to_address(&pk);

        // Manually compute expected address
        let bytes = hex::decode(&pk).unwrap();
        let hash = alloy_primitives::keccak256(&bytes);
        let expected = Address::from_slice(&hash[12..]);
        assert_eq!(addr, expected);
        assert_ne!(addr, Address::ZERO);
    }

    #[test]
    fn test_reward_no_overflow_with_max_base() {
        // With log curve, even u64::MAX base reward shouldn't overflow.
        // n=2, N=1 → n capped to 1, ratio = 1.0, reward = R_max
        let mut mgr = MobileRewardManager::new(1, u64::MAX, 4.0, 32);
        let pk = test_pubkey_hex(1);

        mgr.record_attestation(&pk);
        mgr.record_attestation(&pk);
        mgr.check_epoch_boundary(1);

        let rewards = mgr.take_pending_rewards(1);
        assert_eq!(rewards.len(), 1);
        // n capped to N=1, ratio=1.0, reward = R_max (capped by .min())
        assert_eq!(rewards[0].amount, u64::MAX);
    }

    #[test]
    fn test_many_unique_validators() {
        let mut mgr = MobileRewardManager::new(100, 1_000, 4.0, 32);

        for i in 0..100u8 {
            mgr.record_attestation(&test_pubkey_hex(i));
        }
        assert_eq!(mgr.epoch_attestation_count(), 100);

        mgr.check_epoch_boundary(100);
        assert_eq!(mgr.pending_count(), 100);

        // Drain in batches of 32
        let mut drained = 0;
        let mut block = 100u64;
        while mgr.pending_count() > 0 {
            let batch = mgr.take_pending_rewards(block);
            assert!(batch.len() <= 32);
            drained += batch.len();
            block += 1;
        }
        assert_eq!(drained, 100);
    }

    // --- Logarithmic curve specific tests ---

    #[test]
    fn test_log_reward_zero_attestations() {
        assert_eq!(compute_log_reward_inner(0, 21600, 100_000_000, 4.0), 0);
    }

    #[test]
    fn test_log_reward_full_epoch() {
        // n == N → ratio = 1.0 → full reward
        assert_eq!(compute_log_reward_inner(21600, 21600, 100_000_000, 4.0), 100_000_000);
    }

    #[test]
    fn test_log_reward_capped_at_max() {
        // n > N should be capped to N, yielding exactly R_max
        assert_eq!(compute_log_reward_inner(50000, 21600, 100_000_000, 4.0), 100_000_000);
        assert_eq!(compute_log_reward_inner(u64::MAX, 21600, 100_000_000, 4.0), 100_000_000);
    }

    #[test]
    fn test_log_reward_five_minutes() {
        // 5 minutes at 4s blocks = 75 blocks, epoch = 21600 (24h)
        let reward = compute_log_reward_inner(75, 21600, 100_000_000, 4.0);

        // Expected: ~50.2% of 100M Gwei ≈ 50,200,000
        let expected_min = 49_000_000u64;
        let expected_max = 52_000_000u64;
        assert!(
            reward >= expected_min && reward <= expected_max,
            "5-min reward {} should be ~50% of 100M (between {} and {})",
            reward, expected_min, expected_max
        );
    }

    #[test]
    fn test_log_reward_one_hour() {
        // 1 hour at 4s blocks = 900 blocks
        let reward = compute_log_reward_inner(900, 21600, 100_000_000, 4.0);

        // Expected: ~72% of 100M Gwei
        let expected_min = 70_000_000u64;
        let expected_max = 74_000_000u64;
        assert!(
            reward >= expected_min && reward <= expected_max,
            "1-hour reward {} should be ~72% of 100M (between {} and {})",
            reward, expected_min, expected_max
        );
    }

    #[test]
    fn test_log_reward_monotonically_increasing() {
        let mut prev = 0u64;
        for n in [1, 10, 75, 300, 900, 5400, 10800, 21600] {
            let reward = compute_log_reward_inner(n, 21600, 100_000_000, 4.0);
            assert!(
                reward > prev,
                "reward({}) = {} should be > reward at previous step = {}",
                n, reward, prev
            );
            prev = reward;
        }
    }

    #[test]
    fn test_log_reward_diminishing_returns() {
        // The marginal gain from 0→75 blocks should be larger than 75→150
        let r_75 = compute_log_reward_inner(75, 21600, 100_000_000, 4.0);
        let r_150 = compute_log_reward_inner(150, 21600, 100_000_000, 4.0);
        let first_segment = r_75; // gain from 0 to 75
        let second_segment = r_150 - r_75; // gain from 75 to 150
        assert!(
            first_segment > second_segment,
            "first 75 blocks gain ({}) should exceed next 75 blocks gain ({})",
            first_segment, second_segment
        );
    }
}
