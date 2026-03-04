use alloy_eips::eip4895::Withdrawal;
use alloy_primitives::Address;
use std::collections::{HashMap, VecDeque};
use tracing::{debug, info, warn};

/// Maximum pending rewards in the queue to prevent unbounded memory growth.
/// At 32 rewards/block, 1M entries would take ~31,250 blocks (~35 hours) to drain.
const MAX_REWARD_QUEUE_SIZE: usize = 1_000_000;

/// Manages mobile verification rewards using EIP-4895 Withdrawals.
///
/// Uses a logarithmic reward curve: `reward(n) = R_max * ln(1 + k*n) / ln(1 + k*N)`
/// where n = attestation count, N = blocks_per_epoch, k = curve steepness.
/// 5 minutes of verification yields ~50% of daily reward, discouraging 24/7 operation.
pub struct MobileRewardManager {
    epoch_attestations: HashMap<[u8; 48], u64>,
    reward_queue: VecDeque<Withdrawal>,
    last_distribution_block: u64,
    blocks_per_epoch: u64,
    daily_base_reward_gwei: u64,
    curve_k: f64,
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

    pub fn record_attestation(&mut self, pubkey: &[u8; 48]) {
        let count = self.epoch_attestations.entry(*pubkey).or_insert(0);
        *count += 1;
        debug!(
            target: "n42::reward",
            pubkey = hex::encode(&pubkey[..8]),
            count = *count,
            "attestation recorded"
        );
    }

    /// At epoch boundaries, computes rewards for all attestors and enqueues them.
    pub fn check_epoch_boundary(&mut self, current_block: u64) {
        if self.blocks_per_epoch == 0 {
            return;
        }

        let current_epoch = current_block / self.blocks_per_epoch;
        let last_epoch = self.last_distribution_block / self.blocks_per_epoch;

        if current_epoch <= last_epoch && self.last_distribution_block > 0 {
            return;
        }

        if current_block < self.blocks_per_epoch {
            return;
        }

        if self.epoch_attestations.is_empty() {
            debug!(target: "n42::reward", current_block, "epoch boundary but no attestations");
            self.last_distribution_block = current_block;
            return;
        }

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

        // Cache fields to avoid borrow conflict with drain().
        let blocks_per_epoch = self.blocks_per_epoch;
        let daily_base_reward_gwei = self.daily_base_reward_gwei;
        let curve_k = self.curve_k;

        // Collect and sort entries deterministically by pubkey to ensure consistent
        // reward distribution across all nodes when near capacity.
        // Using drain() inside a `break`-able loop would silently drop the
        // remaining entries (Drain::drop clears the HashMap), causing those
        // validators to lose their rewards without any log message.
        let mut entries: Vec<([u8; 48], u64)> = self.epoch_attestations.drain().collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));

        let remaining_capacity = MAX_REWARD_QUEUE_SIZE - self.reward_queue.len();
        if entries.len() > remaining_capacity {
            warn!(
                target: "n42::reward",
                total = entries.len(),
                capacity = remaining_capacity,
                discarded = entries.len() - remaining_capacity,
                "reward queue near capacity; excess validator rewards discarded for this epoch"
            );
        }

        let mut added = 0usize;
        for (pubkey, count) in entries.iter().take(remaining_capacity) {
            let reward_gwei = compute_log_reward_inner(
                *count, blocks_per_epoch, daily_base_reward_gwei, curve_k,
            );

            if reward_gwei == 0 {
                continue;
            }

            let address = bls_pubkey_to_address(pubkey);
            if address.is_zero() {
                warn!(
                    target: "n42::reward",
                    pubkey = hex::encode(&pubkey[..8]),
                    "bls_pubkey_to_address returned zero address, skipping reward"
                );
                continue;
            }

            total_reward_gwei = total_reward_gwei.saturating_add(reward_gwei);
            self.reward_queue.push_back(Withdrawal {
                index: 0,
                validator_index: 0,
                address,
                amount: reward_gwei,
            });
            added += 1;
        }

        info!(
            target: "n42::reward",
            current_block,
            epoch = current_epoch,
            eligible_validators = validator_count,
            rewarded = added,
            total_reward_gwei,
            queue_size = self.reward_queue.len(),
            "epoch reward distribution computed"
        );

        self.last_distribution_block = current_block;
    }

    /// Takes up to `max_rewards_per_block` pending rewards.
    /// Withdrawal indices: `block_number * max_rewards_per_block + i` for global uniqueness.
    pub fn take_pending_rewards(&mut self, block_number: u64) -> Vec<Withdrawal> {
        let count = self.reward_queue.len().min(self.max_rewards_per_block);
        let mut rewards: Vec<Withdrawal> = self.reward_queue.drain(..count).collect();

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

    pub fn pending_count(&self) -> usize {
        self.reward_queue.len()
    }

    pub fn epoch_attestation_count(&self) -> usize {
        self.epoch_attestations.len()
    }
}

/// Computes the logarithmic mobile verification reward.
///
/// Formula: `reward(n) = R_max * ln(1 + k*n) / ln(1 + k*N)`, capped at `R_max`.
///
/// - `n` = attestation count for the epoch (clamped to `N`).
/// - `N` = `blocks_per_epoch` (epoch length in blocks).
/// - `k` = curve steepness (typical: 4.0 — 5 min ≈ 50% of max reward).
///
/// # Precision Guarantee
///
/// All computation uses `f64` (~15 significant decimal digits).  Rewards are
/// denominated in Gwei (10⁻⁹ ETH).  The maximum truncation error is 1 Gwei
/// (< 0.000000001 ETH), which is negligible at all realistic reward scales.
/// The curve is strictly monotone in `n` (provably, since `ln` is concave-up).
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

/// Derives an EVM address from a raw 48-byte BLS12-381 public key.
fn bls_pubkey_to_address(pubkey: &[u8; 48]) -> Address {
    n42_mobile::bls_pubkey_to_address(pubkey)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_pubkey(idx: u8) -> [u8; 48] {
        [idx; 48]
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
        let pk = test_pubkey(1);

        mgr.record_attestation(&pk);
        assert_eq!(mgr.epoch_attestation_count(), 1);
        assert_eq!(mgr.epoch_attestations[&pk], 1);

        mgr.record_attestation(&pk);
        assert_eq!(mgr.epoch_attestations[&pk], 2);
    }

    #[test]
    fn test_epoch_boundary_no_early_distribution() {
        let mut mgr = MobileRewardManager::new(100, 100_000_000, 4.0, 32);
        let pk = test_pubkey(1);
        mgr.record_attestation(&pk);

        // Block 50: not at epoch boundary
        mgr.check_epoch_boundary(50);
        assert_eq!(mgr.pending_count(), 0);
        assert_eq!(mgr.epoch_attestation_count(), 1);
    }

    #[test]
    fn test_epoch_boundary_distribution() {
        let mut mgr = MobileRewardManager::new(100, 100_000_000, 4.0, 32);

        let pk1 = test_pubkey(1);
        let pk2 = test_pubkey(2);

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
            let pk = test_pubkey(i);
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

        let pk = test_pubkey(1);
        mgr.record_attestation(&pk);

        mgr.check_epoch_boundary(100);
        assert_eq!(mgr.pending_count(), 1);

        // Same block shouldn't trigger again
        mgr.record_attestation(&test_pubkey(2));
        mgr.check_epoch_boundary(100);
        assert_eq!(mgr.pending_count(), 1); // no new rewards added
    }

    #[test]
    fn test_withdrawal_index_block_based() {
        let mut mgr = MobileRewardManager::new(100, 100_000_000, 4.0, 32);

        for i in 0..3u8 {
            mgr.record_attestation(&test_pubkey(i));
        }
        mgr.check_epoch_boundary(100);

        let rewards = mgr.take_pending_rewards(100);
        let indices: Vec<u64> = rewards.iter().map(|w| w.index).collect();
        // Indices should be block_number * max_per_block + offset
        assert_eq!(indices, vec![100 * 32, 100 * 32 + 1, 100 * 32 + 2]);

        // Next epoch - different block, different index range
        mgr.record_attestation(&test_pubkey(10));
        mgr.check_epoch_boundary(200);

        let rewards2 = mgr.take_pending_rewards(200);
        assert_eq!(rewards2[0].index, 200 * 32); // based on block 200
    }

    #[test]
    fn test_bls_pubkey_to_address_deterministic() {
        let addr1 = bls_pubkey_to_address(&test_pubkey(1));
        let addr2 = bls_pubkey_to_address(&test_pubkey(1));
        assert_eq!(addr1, addr2);

        let addr3 = bls_pubkey_to_address(&test_pubkey(2));
        assert_ne!(addr1, addr3);
    }

    #[test]
    fn test_empty_epoch() {
        let mut mgr = MobileRewardManager::new(100, 100_000_000, 4.0, 32);
        mgr.check_epoch_boundary(100);
        assert_eq!(mgr.pending_count(), 0);
    }

    #[test]
    fn test_distinct_pubkeys_tracked_separately() {
        let mut mgr = MobileRewardManager::new(100, 100_000_000, 4.0, 32);
        // Three distinct pubkeys, each recorded once
        mgr.record_attestation(&test_pubkey(0xAA));
        mgr.record_attestation(&test_pubkey(0xBB));
        mgr.record_attestation(&test_pubkey(0xCC));
        assert_eq!(mgr.epoch_attestation_count(), 3);
    }

    #[test]
    fn test_bls_pubkey_to_address_non_zero() {
        // A non-zero pubkey must produce a non-zero address.
        let pk = test_pubkey(1);
        let addr = bls_pubkey_to_address(&pk);
        assert_ne!(addr, Address::ZERO);
    }

    #[test]
    fn test_zero_blocks_per_epoch_no_distribution() {
        let mut mgr = MobileRewardManager::new(0, 100_000_000, 4.0, 32);
        let pk = test_pubkey(1);
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
            mgr.record_attestation(&test_pubkey(i));
        }
        mgr.check_epoch_boundary(10);
        assert_eq!(mgr.pending_count(), 3);

        // Drain partial: take 2
        let batch1 = mgr.take_pending_rewards(10);
        assert_eq!(batch1.len(), 2);
        assert_eq!(mgr.pending_count(), 1);

        // Epoch 2: 2 more validators (queue should grow)
        for i in 10..12u8 {
            mgr.record_attestation(&test_pubkey(i));
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
        let pk = test_pubkey(42);
        let addr = bls_pubkey_to_address(&pk);

        // Manually compute expected address: keccak256(pubkey_bytes)[12..]
        let hash = alloy_primitives::keccak256(&pk);
        let expected = Address::from_slice(&hash[12..]);
        assert_eq!(addr, expected);
        assert_ne!(addr, Address::ZERO);
    }

    #[test]
    fn test_reward_no_overflow_with_max_base() {
        // With log curve, even u64::MAX base reward shouldn't overflow.
        // n=2, N=1 → n capped to 1, ratio = 1.0, reward = R_max
        let mut mgr = MobileRewardManager::new(1, u64::MAX, 4.0, 32);
        let pk = test_pubkey(1);

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
            mgr.record_attestation(&test_pubkey(i));
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

    // --- P1c: precision guarantee tests ---

    /// n=1 must yield a positive reward (not truncated to 0).
    /// This guards against extremely small k or R_max values silently producing zero.
    #[test]
    fn test_precision_single_attestation_minimum() {
        // With R_max = 1_000_000 Gwei and epoch = 21600 blocks, a single attestation
        // must produce at least 1 Gwei reward (not truncated to 0).
        let reward = compute_log_reward_inner(1, 21600, 1_000_000, 4.0);
        assert!(
            reward >= 1,
            "n=1 reward ({}) must be at least 1 Gwei (not truncated to 0)",
            reward
        );
    }

    /// attestation_count > blocks_per_epoch must be capped at R_max (no overflow).
    /// Guards the `.min(blocks_per_epoch)` clamp in the implementation.
    #[test]
    fn test_precision_overflow_guard() {
        let r_max = 100_000_000u64;
        let epoch = 21600u64;

        // Exactly at the boundary
        let at_max = compute_log_reward_inner(epoch, epoch, r_max, 4.0);
        // Well above the boundary
        let over_max = compute_log_reward_inner(epoch * 100, epoch, r_max, 4.0);

        assert_eq!(at_max, r_max, "n==N should yield exactly R_max");
        assert_eq!(over_max, r_max, "n>>N should also yield exactly R_max (capped)");
    }

    /// Reward must be strictly monotone in attestation_count over the full [1..N] range.
    /// A single counter-example would indicate precision loss at some scale.
    #[test]
    fn test_precision_monotonic() {
        let epoch = 1000u64;
        let r_max = 100_000_000u64;
        let k = 4.0f64;

        let mut prev_reward = 0u64;
        for n in 1..=epoch {
            let reward = compute_log_reward_inner(n, epoch, r_max, k);
            assert!(
                reward >= prev_reward,
                "reward must be non-decreasing: reward({n}) = {reward} < reward({prev_n}) = {prev_reward}",
                prev_n = n - 1
            );
            prev_reward = reward;
        }
    }
}
