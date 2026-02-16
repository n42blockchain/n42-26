use alloy_primitives::B256;
use arc_swap::ArcSwap;
use n42_consensus::ValidatorSet;
use n42_primitives::QuorumCertificate;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;
use tracing::info;

/// A verification task pushed to mobile subscribers when a block is committed.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerificationTask {
    pub block_hash: B256,
    pub block_number: u64,
}

/// Per-block mobile attestation tracking.
pub struct BlockAttestations {
    pub block_number: u64,
    pub attesters: HashSet<String>, // hex-encoded pubkeys for dedup
    pub threshold: u32,
    pub reached_threshold: bool,
}

/// Tracks mobile phone attestations across recent blocks.
pub struct AttestationState {
    blocks: HashMap<B256, BlockAttestations>,
    threshold: u32,
    max_blocks: usize,
}

impl std::fmt::Debug for AttestationState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AttestationState")
            .field("blocks_tracked", &self.blocks.len())
            .field("threshold", &self.threshold)
            .finish()
    }
}

impl AttestationState {
    pub fn new(threshold: u32, max_blocks: usize) -> Self {
        Self {
            blocks: HashMap::new(),
            threshold,
            max_blocks,
        }
    }

    /// Register a new block for attestation tracking.
    pub fn register_block(&mut self, block_hash: B256, block_number: u64) {
        // Evict oldest block if at capacity.
        if self.blocks.len() >= self.max_blocks {
            if let Some(oldest_hash) = self
                .blocks
                .iter()
                .min_by_key(|(_, v)| v.block_number)
                .map(|(k, _)| *k)
            {
                self.blocks.remove(&oldest_hash);
            }
        }

        self.blocks.entry(block_hash).or_insert(BlockAttestations {
            block_number,
            attesters: HashSet::new(),
            threshold: self.threshold,
            reached_threshold: false,
        });
    }

    /// Record an attestation. Returns (current_count, reached_threshold).
    pub fn record_attestation(
        &mut self,
        block_hash: B256,
        pubkey_hex: String,
    ) -> Option<(u32, bool)> {
        let entry = self.blocks.get_mut(&block_hash)?;
        entry.attesters.insert(pubkey_hex);
        let count = entry.attesters.len() as u32;
        if count >= entry.threshold && !entry.reached_threshold {
            entry.reached_threshold = true;
        }
        Some((count, entry.reached_threshold))
    }

    /// Get attestation count for a block.
    pub fn get_attestation_count(&self, block_hash: &B256) -> Option<u32> {
        self.blocks
            .get(block_hash)
            .map(|b| b.attesters.len() as u32)
    }
}

/// Shared consensus state between the Orchestrator, PayloadBuilder, and RPC.
///
/// The Orchestrator writes to this state (low frequency, on block commit),
/// while the PayloadBuilder reads from it (on every payload build attempt).
/// `ArcSwap` provides lock-free reads for the hot path.
#[derive(Debug)]
pub struct SharedConsensusState {
    /// The latest committed QC, updated by the Orchestrator on each BlockCommitted event.
    /// `None` at genesis before any block is committed.
    pub latest_committed_qc: ArcSwap<Option<QuorumCertificate>>,
    /// The current validator set, initialized from chainspec on startup.
    pub validator_set: Arc<ValidatorSet>,
    /// Mobile attestation state tracking.
    pub attestation_state: Mutex<AttestationState>,
    /// Broadcast sender for notifying RPC subscribers of committed blocks.
    pub block_committed_tx: broadcast::Sender<VerificationTask>,
}

impl SharedConsensusState {
    /// Creates a new shared state with the given validator set and no initial QC.
    pub fn new(validator_set: ValidatorSet) -> Self {
        let (block_committed_tx, _) = broadcast::channel(64);
        Self {
            latest_committed_qc: ArcSwap::from_pointee(None),
            validator_set: Arc::new(validator_set),
            attestation_state: Mutex::new(AttestationState::new(10, 100)),
            block_committed_tx,
        }
    }

    /// Updates the latest committed QC (called by the Orchestrator).
    pub fn update_committed_qc(&self, qc: QuorumCertificate) {
        self.latest_committed_qc.store(Arc::new(Some(qc)));
    }

    /// Reads the latest committed QC (called by the PayloadBuilder).
    pub fn load_committed_qc(&self) -> Arc<Option<QuorumCertificate>> {
        self.latest_committed_qc.load_full()
    }

    /// Notify RPC subscribers and register block for attestation tracking.
    pub fn notify_block_committed(&self, block_hash: B256, view: u64) {
        // Register block for attestation tracking.
        if let Ok(mut att_state) = self.attestation_state.lock() {
            att_state.register_block(block_hash, view);
        }

        let task = VerificationTask {
            block_hash,
            block_number: view,
        };
        let receivers = self.block_committed_tx.receiver_count();
        if receivers > 0 {
            match self.block_committed_tx.send(task) {
                Ok(n) => {
                    info!(
                        %block_hash,
                        view,
                        receivers = n,
                        "verification task broadcast to mobile subscribers"
                    );
                }
                Err(_) => {
                    // No active receivers; this is fine.
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use n42_consensus::ValidatorSet;

    fn make_state() -> SharedConsensusState {
        let vs = ValidatorSet::new(&[], 0);
        SharedConsensusState::new(vs)
    }

    #[test]
    fn test_attestation_state_register_and_record() {
        let mut att = AttestationState::new(3, 100);
        let hash = B256::repeat_byte(0xAA);
        att.register_block(hash, 1);

        let (count, reached) = att.record_attestation(hash, "pk1".into()).unwrap();
        assert_eq!(count, 1);
        assert!(!reached);

        let (count, reached) = att.record_attestation(hash, "pk2".into()).unwrap();
        assert_eq!(count, 2);
        assert!(!reached);

        let (count, reached) = att.record_attestation(hash, "pk3".into()).unwrap();
        assert_eq!(count, 3);
        assert!(reached, "threshold of 3 should be reached");
    }

    #[test]
    fn test_attestation_dedup() {
        let mut att = AttestationState::new(3, 100);
        let hash = B256::repeat_byte(0xBB);
        att.register_block(hash, 1);

        att.record_attestation(hash, "pk1".into());
        let (count, _) = att.record_attestation(hash, "pk1".into()).unwrap();
        assert_eq!(count, 1, "duplicate pubkey should not increase count");
    }

    #[test]
    fn test_attestation_unknown_block() {
        let mut att = AttestationState::new(3, 100);
        let hash = B256::repeat_byte(0xCC);
        assert!(att.record_attestation(hash, "pk1".into()).is_none());
    }

    #[test]
    fn test_attestation_eviction() {
        let mut att = AttestationState::new(3, 2);
        let h1 = B256::repeat_byte(0x01);
        let h2 = B256::repeat_byte(0x02);
        let h3 = B256::repeat_byte(0x03);

        att.register_block(h1, 1);
        att.register_block(h2, 2);
        assert_eq!(att.blocks.len(), 2);

        // Adding a third should evict the oldest (h1).
        att.register_block(h3, 3);
        assert_eq!(att.blocks.len(), 2);
        assert!(att.get_attestation_count(&h1).is_none(), "h1 should be evicted");
        assert!(att.get_attestation_count(&h2).is_some());
        assert!(att.get_attestation_count(&h3).is_some());
    }

    #[test]
    fn test_notify_block_committed() {
        let state = make_state();
        let mut rx = state.block_committed_tx.subscribe();

        let hash = B256::repeat_byte(0xDD);
        state.notify_block_committed(hash, 42);

        let task = rx.try_recv().unwrap();
        assert_eq!(task.block_hash, hash);
        assert_eq!(task.block_number, 42);

        // Block should also be registered for attestation.
        let att = state.attestation_state.lock().unwrap();
        assert_eq!(att.get_attestation_count(&hash), Some(0));
    }
}
