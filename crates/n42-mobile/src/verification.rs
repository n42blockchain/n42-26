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
            self.valid_count += 1;
        } else {
            self.invalid_count += 1;
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
