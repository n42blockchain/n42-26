use alloy_primitives::B256;
use std::collections::HashMap;

use crate::receipt::VerificationReceipt;

/// Aggregated verification status for a single block.
///
/// Tracks mobile verification progress and determines when
/// sufficient attestation has been collected.
#[derive(Debug)]
pub struct BlockVerificationStatus {
    /// Block hash being verified.
    pub block_hash: B256,
    /// Block number.
    pub block_number: u64,
    /// Total number of valid receipts received.
    pub valid_count: u32,
    /// Total number of invalid receipts (state root mismatch).
    pub invalid_count: u32,
    /// Required threshold: number of valid receipts needed for attestation.
    /// Typically 2/3 of connected phones.
    pub threshold: u32,
    /// Verifier pubkeys that have submitted receipts (for deduplication).
    seen_verifiers: HashMap<[u8; 32], bool>,
}

impl BlockVerificationStatus {
    /// Creates a new verification status for a block.
    pub fn new(block_hash: B256, block_number: u64, threshold: u32) -> Self {
        Self {
            block_hash,
            block_number,
            valid_count: 0,
            invalid_count: 0,
            threshold,
            seen_verifiers: HashMap::new(),
        }
    }

    /// Adds a receipt to the verification status.
    ///
    /// Returns `true` if the receipt was new (not a duplicate).
    /// Duplicate receipts from the same verifier are ignored.
    pub fn add_receipt(&mut self, receipt: &VerificationReceipt) -> bool {
        // Skip duplicates
        if self.seen_verifiers.contains_key(&receipt.verifier_pubkey) {
            return false;
        }

        self.seen_verifiers
            .insert(receipt.verifier_pubkey, receipt.is_valid());

        if receipt.is_valid() {
            self.valid_count = self.valid_count.saturating_add(1);
        } else {
            self.invalid_count = self.invalid_count.saturating_add(1);
        }

        true
    }

    /// Returns true if the attestation threshold has been reached.
    pub fn is_attested(&self) -> bool {
        self.valid_count >= self.threshold
    }

    /// Returns the total number of receipts received.
    pub fn total_receipts(&self) -> u32 {
        self.valid_count + self.invalid_count
    }

    /// Returns the fraction of valid receipts (0.0 - 1.0).
    pub fn validity_ratio(&self) -> f64 {
        let total = self.total_receipts();
        if total == 0 {
            return 0.0;
        }
        self.valid_count as f64 / total as f64
    }
}

/// Aggregates verification receipts across multiple blocks.
///
/// Maintains verification status for recent blocks and determines
/// when attestation thresholds are met.
pub struct ReceiptAggregator {
    /// Per-block verification status.
    blocks: HashMap<B256, BlockVerificationStatus>,
    /// Default threshold for new blocks (2/3 of connected phones).
    default_threshold: u32,
    /// Maximum number of blocks to track (older blocks are evicted).
    max_tracked_blocks: usize,
}

impl ReceiptAggregator {
    /// Creates a new receipt aggregator.
    ///
    /// `default_threshold` is the number of valid receipts required
    /// for attestation (typically 2/3 of connected phones).
    pub fn new(default_threshold: u32, max_tracked_blocks: usize) -> Self {
        Self {
            blocks: HashMap::new(),
            default_threshold,
            max_tracked_blocks,
        }
    }

    /// Registers a new block for verification tracking.
    pub fn register_block(&mut self, block_hash: B256, block_number: u64) {
        // Evict oldest blocks if at capacity
        if self.blocks.len() >= self.max_tracked_blocks {
            // Find the block with the lowest block number
            if let Some(oldest_hash) = self
                .blocks
                .iter()
                .min_by_key(|(_, status)| status.block_number)
                .map(|(hash, _)| *hash)
            {
                self.blocks.remove(&oldest_hash);
            }
        }

        self.blocks.insert(
            block_hash,
            BlockVerificationStatus::new(block_hash, block_number, self.default_threshold),
        );
    }

    /// Processes a verification receipt.
    ///
    /// Returns `Some(true)` if this receipt caused the attestation threshold
    /// to be reached, `Some(false)` if the receipt was accepted but threshold
    /// not yet met, or `None` if the block is not tracked or receipt is duplicate.
    pub fn process_receipt(&mut self, receipt: &VerificationReceipt) -> Option<bool> {
        let status = self.blocks.get_mut(&receipt.block_hash)?;
        let was_attested = status.is_attested();
        let is_new = status.add_receipt(receipt);

        if !is_new {
            return None;
        }

        if !was_attested && status.is_attested() {
            tracing::info!(
                block_hash = %receipt.block_hash,
                block_number = receipt.block_number,
                valid = status.valid_count,
                threshold = status.threshold,
                "block attestation threshold reached"
            );
            Some(true)
        } else {
            Some(false)
        }
    }

    /// Returns the verification status for a block, if tracked.
    pub fn get_status(&self, block_hash: &B256) -> Option<&BlockVerificationStatus> {
        self.blocks.get(block_hash)
    }

    /// Returns the number of blocks currently being tracked.
    pub fn tracked_blocks(&self) -> usize {
        self.blocks.len()
    }

    /// Updates the default threshold (e.g., when phone count changes).
    pub fn set_default_threshold(&mut self, threshold: u32) {
        self.default_threshold = threshold;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::Signature;

    /// Helper: creates a mock VerificationReceipt with the given fields.
    /// Uses a dummy signature (not cryptographically valid, but sufficient
    /// for aggregator-level tests that don't verify signatures).
    fn mock_receipt(
        block_hash: B256,
        block_number: u64,
        state_root_match: bool,
        receipts_root_match: bool,
        verifier_pubkey: [u8; 32],
    ) -> VerificationReceipt {
        VerificationReceipt {
            block_hash,
            block_number,
            state_root_match,
            receipts_root_match,
            verifier_pubkey,
            signature: Signature::from_bytes(&[0u8; 64]),
            timestamp_ms: 1_000_000,
        }
    }

    /// Helper: creates a B256 from a single byte.
    fn hash(b: u8) -> B256 {
        B256::from([b; 32])
    }

    /// Helper: creates a unique verifier pubkey from a single byte.
    fn pubkey(b: u8) -> [u8; 32] {
        let mut pk = [0u8; 32];
        pk[0] = b;
        pk
    }

    #[test]
    fn test_block_verification_status() {
        let bh = hash(1);
        let mut status = BlockVerificationStatus::new(bh, 100, 3);

        assert_eq!(status.valid_count, 0);
        assert_eq!(status.invalid_count, 0);
        assert!(!status.is_attested());
        assert_eq!(status.total_receipts(), 0);

        // Add two valid receipts.
        let r1 = mock_receipt(bh, 100, true, true, pubkey(1));
        let r2 = mock_receipt(bh, 100, true, true, pubkey(2));
        assert!(status.add_receipt(&r1));
        assert!(status.add_receipt(&r2));

        assert_eq!(status.valid_count, 2);
        assert_eq!(status.invalid_count, 0);
        assert_eq!(status.total_receipts(), 2);
        assert!(!status.is_attested(), "threshold is 3, only 2 valid");

        // Add one invalid receipt.
        let r3 = mock_receipt(bh, 100, false, true, pubkey(3));
        assert!(status.add_receipt(&r3));
        assert_eq!(status.invalid_count, 1);
        assert!(!status.is_attested(), "only 2 valid, threshold 3");

        // Add a third valid receipt → threshold reached.
        let r4 = mock_receipt(bh, 100, true, true, pubkey(4));
        assert!(status.add_receipt(&r4));
        assert_eq!(status.valid_count, 3);
        assert!(status.is_attested());
    }

    #[test]
    fn test_block_verification_dedup() {
        let bh = hash(2);
        let mut status = BlockVerificationStatus::new(bh, 200, 5);

        let pk = pubkey(10);
        let r1 = mock_receipt(bh, 200, true, true, pk);
        let r2 = mock_receipt(bh, 200, true, true, pk); // same verifier

        assert!(status.add_receipt(&r1), "first receipt should be accepted");
        assert!(!status.add_receipt(&r2), "duplicate verifier should be rejected");

        assert_eq!(status.valid_count, 1, "dedup: only one receipt should be counted");
        assert_eq!(status.total_receipts(), 1);
    }

    #[test]
    fn test_receipt_aggregator_register_and_process() {
        let mut agg = ReceiptAggregator::new(2, 100); // threshold = 2

        let bh = hash(10);
        agg.register_block(bh, 1000);

        assert_eq!(agg.tracked_blocks(), 1);

        // Process first valid receipt — not yet attested.
        let r1 = mock_receipt(bh, 1000, true, true, pubkey(1));
        let result1 = agg.process_receipt(&r1);
        assert_eq!(result1, Some(false), "first receipt should not reach threshold");

        // Process second valid receipt — threshold reached.
        let r2 = mock_receipt(bh, 1000, true, true, pubkey(2));
        let result2 = agg.process_receipt(&r2);
        assert_eq!(result2, Some(true), "second receipt should reach threshold");

        // Process a receipt for an untracked block.
        let untracked_bh = hash(99);
        let r3 = mock_receipt(untracked_bh, 999, true, true, pubkey(3));
        let result3 = agg.process_receipt(&r3);
        assert_eq!(result3, None, "untracked block should return None");

        // Process a duplicate receipt.
        let result4 = agg.process_receipt(&r1);
        assert_eq!(result4, None, "duplicate receipt should return None");

        // Verify status.
        let status = agg.get_status(&bh).expect("block should be tracked");
        assert_eq!(status.valid_count, 2);
        assert!(status.is_attested());
    }

    #[test]
    fn test_receipt_aggregator_eviction() {
        let max_tracked = 3;
        let mut agg = ReceiptAggregator::new(2, max_tracked);

        // Register exactly max_tracked blocks.
        for i in 0..max_tracked {
            agg.register_block(hash(i as u8), i as u64);
        }
        assert_eq!(agg.tracked_blocks(), max_tracked);

        // Register one more. The block with the lowest block_number (0) should be evicted.
        agg.register_block(hash(100), 100);
        assert_eq!(agg.tracked_blocks(), max_tracked);

        // hash(0) at block_number 0 should have been evicted.
        assert!(
            agg.get_status(&hash(0)).is_none(),
            "oldest block (block_number 0) should be evicted"
        );

        // The others should still be present.
        assert!(agg.get_status(&hash(1)).is_some());
        assert!(agg.get_status(&hash(2)).is_some());
        assert!(agg.get_status(&hash(100)).is_some());
    }

    #[test]
    fn test_validity_ratio() {
        let bh = hash(50);
        let mut status = BlockVerificationStatus::new(bh, 500, 10);

        // No receipts → ratio 0.0.
        assert_eq!(status.validity_ratio(), 0.0);

        // 3 valid, 1 invalid → 3/4 = 0.75.
        status.add_receipt(&mock_receipt(bh, 500, true, true, pubkey(1)));
        status.add_receipt(&mock_receipt(bh, 500, true, true, pubkey(2)));
        status.add_receipt(&mock_receipt(bh, 500, true, true, pubkey(3)));
        status.add_receipt(&mock_receipt(bh, 500, false, true, pubkey(4)));

        let ratio = status.validity_ratio();
        assert!((ratio - 0.75).abs() < f64::EPSILON, "expected 0.75, got {ratio}");
    }
}
