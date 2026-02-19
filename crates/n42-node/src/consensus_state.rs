use alloy_primitives::B256;
use arc_swap::ArcSwap;
use n42_consensus::ValidatorSet;
use n42_primitives::QuorumCertificate;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;
use tracing::{info, warn};

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

/// Record of a mobile attestation event (block reaching threshold).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttestationRecord {
    pub block_hash: B256,
    pub block_number: u64,
    pub valid_count: u32,
    pub timestamp: u64,
}

/// Evidence of equivocation (double-voting) by a validator.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EquivocationEvidence {
    pub view: u64,
    pub validator_index: u32,
    pub hash1: B256,
    pub hash2: B256,
    pub detected_at: u64,
}

/// Maximum number of attestation records kept in history.
const MAX_ATTESTATION_HISTORY: usize = 1000;
/// Maximum number of equivocation evidence entries kept.
const MAX_EQUIVOCATION_LOG: usize = 500;

/// Default attestation threshold for SharedConsensusState.
/// Configurable via `N42_MIN_ATTESTATION_THRESHOLD` environment variable.
fn default_attestation_threshold() -> u32 {
    std::env::var("N42_MIN_ATTESTATION_THRESHOLD")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(10)
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
    /// History of attestation events (blocks that reached threshold).
    pub attestation_history: Mutex<VecDeque<AttestationRecord>>,
    /// Log of detected equivocation evidence for accountability.
    pub equivocation_log: Mutex<VecDeque<EquivocationEvidence>>,
}

impl SharedConsensusState {
    /// Creates a new shared state with the given validator set and no initial QC.
    ///
    /// The attestation threshold is read from `N42_MIN_ATTESTATION_THRESHOLD`
    /// environment variable (default: 10). A threshold of 0 is clamped to 1
    /// to prevent blocks from being attested with zero receipts.
    pub fn new(validator_set: ValidatorSet) -> Self {
        let threshold = default_attestation_threshold();
        let threshold = if threshold == 0 {
            warn!("N42_MIN_ATTESTATION_THRESHOLD=0 is invalid, clamping to 1");
            1
        } else {
            threshold
        };
        let (block_committed_tx, _) = broadcast::channel(64);
        Self {
            latest_committed_qc: ArcSwap::from_pointee(None),
            validator_set: Arc::new(validator_set),
            attestation_state: Mutex::new(AttestationState::new(threshold, 100)),
            block_committed_tx,
            attestation_history: Mutex::new(VecDeque::new()),
            equivocation_log: Mutex::new(VecDeque::new()),
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

    /// Records a mobile attestation event (block reached verification threshold).
    pub fn record_attestation(&self, block_hash: B256, block_number: u64, valid_count: u32) {
        let record = AttestationRecord {
            block_hash,
            block_number,
            valid_count,
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        };
        if let Ok(mut history) = self.attestation_history.lock() {
            if history.len() >= MAX_ATTESTATION_HISTORY {
                history.pop_front();
            }
            history.push_back(record);
        }
    }

    /// Returns attestation record for a specific block hash, if available.
    pub fn get_block_attestation(&self, block_hash: &B256) -> Option<AttestationRecord> {
        self.attestation_history.lock().ok()?
            .iter()
            .find(|r| r.block_hash == *block_hash)
            .cloned()
    }

    /// Returns summary statistics about attestation history.
    pub fn attestation_stats(&self) -> (usize, Option<u64>, Option<u64>) {
        let history = match self.attestation_history.lock() {
            Ok(h) => h,
            Err(_) => return (0, None, None),
        };
        let total = history.len();
        let latest_block = history.back().map(|r| r.block_number);
        let earliest_block = history.front().map(|r| r.block_number);
        (total, earliest_block, latest_block)
    }

    /// Records equivocation evidence for accountability.
    pub fn record_equivocation(&self, view: u64, validator_index: u32, hash1: B256, hash2: B256) {
        let evidence = EquivocationEvidence {
            view,
            validator_index,
            hash1,
            hash2,
            detected_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        };
        if let Ok(mut log) = self.equivocation_log.lock() {
            if log.len() >= MAX_EQUIVOCATION_LOG {
                log.pop_front();
            }
            log.push_back(evidence);
        }
    }

    /// Returns all recorded equivocation evidence.
    pub fn get_equivocations(&self) -> Vec<EquivocationEvidence> {
        self.equivocation_log.lock()
            .map(|log| log.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Notify RPC subscribers and register block for attestation tracking.
    pub fn notify_block_committed(&self, block_hash: B256, view: u64) {
        // Register block for attestation tracking.
        // A poisoned mutex here means a prior panic during attestation processing;
        // we log and skip rather than propagate the panic.
        match self.attestation_state.lock() {
            Ok(mut att_state) => att_state.register_block(block_hash, view),
            Err(e) => tracing::error!("attestation_state mutex poisoned: {e}"),
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

    #[test]
    fn test_record_attestation() {
        let state = make_state();
        let hash = B256::repeat_byte(0xAA);
        state.record_attestation(hash, 10, 5);

        let record = state.get_block_attestation(&hash);
        assert!(record.is_some());
        let r = record.unwrap();
        assert_eq!(r.block_number, 10);
        assert_eq!(r.valid_count, 5);
    }

    #[test]
    fn test_attestation_stats() {
        let state = make_state();
        let (total, _, _) = state.attestation_stats();
        assert_eq!(total, 0);

        state.record_attestation(B256::repeat_byte(0x01), 1, 3);
        state.record_attestation(B256::repeat_byte(0x02), 2, 5);

        let (total, earliest, latest) = state.attestation_stats();
        assert_eq!(total, 2);
        assert_eq!(earliest, Some(1));
        assert_eq!(latest, Some(2));
    }

    #[test]
    fn test_record_equivocation() {
        let state = make_state();
        let h1 = B256::repeat_byte(0xAA);
        let h2 = B256::repeat_byte(0xBB);
        state.record_equivocation(5, 2, h1, h2);

        let evs = state.get_equivocations();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].view, 5);
        assert_eq!(evs[0].validator_index, 2);
        assert_eq!(evs[0].hash1, h1);
        assert_eq!(evs[0].hash2, h2);
    }

    #[test]
    fn test_equivocation_log_bounded() {
        let state = make_state();
        // Fill beyond MAX_EQUIVOCATION_LOG (500)
        for i in 0..510u64 {
            state.record_equivocation(i, 0, B256::repeat_byte(0xAA), B256::repeat_byte(0xBB));
        }
        let evs = state.get_equivocations();
        assert_eq!(evs.len(), 500, "equivocation log should be bounded at 500");
        // Oldest entries (views 0..9) should have been evicted
        assert_eq!(evs[0].view, 10, "oldest 10 entries should be evicted");
        assert_eq!(evs[499].view, 509, "newest entry should be view 509");
    }

    #[test]
    fn test_attestation_state_threshold() {
        // Verify that AttestationState respects threshold
        let mut att = AttestationState::new(1, 100);
        let hash = B256::repeat_byte(0xEE);
        att.register_block(hash, 1);

        let (count, reached) = att.record_attestation(hash, "pk1".into()).unwrap();
        assert_eq!(count, 1);
        assert!(reached, "threshold=1, first attestation should reach it");
    }

    #[test]
    fn test_attestation_history_bounded() {
        let state = make_state();
        // Fill beyond MAX_ATTESTATION_HISTORY (1000)
        for i in 0..1010u64 {
            let mut hash_bytes = [0u8; 32];
            hash_bytes[0..8].copy_from_slice(&i.to_le_bytes());
            state.record_attestation(B256::from(hash_bytes), i, 1);
        }
        let (total, earliest, _) = state.attestation_stats();
        assert_eq!(total, 1000);
        // Earliest should be 10 (first 10 were evicted)
        assert_eq!(earliest, Some(10));
    }
}
