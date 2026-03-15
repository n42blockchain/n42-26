use alloy_primitives::B256;
use arc_swap::{ArcSwap, ArcSwapOption};
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
    /// Monotonic committed block number used by mobile verification and rewards.
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

    /// Register a new block for attestation tracking, evicting the oldest if at capacity.
    pub fn register_block(&mut self, block_hash: B256, block_number: u64) {
        if self.blocks.len() >= self.max_blocks
            && let Some(oldest_hash) = self
                .blocks
                .iter()
                .min_by_key(|(_, v)| v.block_number)
                .map(|(k, _)| *k)
        {
            self.blocks.remove(&oldest_hash);
        }

        self.blocks.entry(block_hash).or_insert(BlockAttestations {
            block_number,
            attesters: HashSet::new(),
            threshold: self.threshold,
            reached_threshold: false,
        });
    }

    /// Records an attestation. Returns `(current_count, threshold_reached)`.
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

const MAX_ATTESTATION_HISTORY: usize = 1000;
const MAX_EQUIVOCATION_LOG: usize = 500;
/// Maximum number of recent blocks tracked for attestation.
const MAX_TRACKED_ATTESTATION_BLOCKS: usize = 100;

fn default_attestation_threshold() -> u32 {
    std::env::var("N42_MIN_ATTESTATION_THRESHOLD")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(10)
}

fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Shared consensus state between the Orchestrator, PayloadBuilder, and RPC.
///
/// The Orchestrator writes (low frequency, on block commit); the PayloadBuilder
/// reads (on every payload build attempt). `ArcSwap` provides lock-free reads.
#[derive(Debug)]
pub struct SharedConsensusState {
    /// Latest committed QC; `None` before any block is committed.
    pub(crate) latest_committed_qc: ArcSwap<Option<QuorumCertificate>>,
    pub(crate) validator_set: Arc<ArcSwapOption<ValidatorSet>>,
    pub(crate) attestation_state: Mutex<AttestationState>,
    pub(crate) block_committed_tx: broadcast::Sender<VerificationTask>,
    attestation_history: Mutex<VecDeque<AttestationRecord>>,
    equivocation_log: Mutex<VecDeque<EquivocationEvidence>>,
    /// BLS pubkeys of verifiers that have completed a QUIC handshake with StarHub.
    /// Only keys in this set are accepted by `submit_attestation` (RPC path).
    authorized_verifiers: Mutex<HashSet<[u8; 48]>>,
    /// Latest JMT (Jellyfish Merkle Tree) root and version.
    /// Updated asynchronously after each committed block's state diff is applied.
    pub(crate) jmt_root: ArcSwap<Option<(u64, B256)>>,
    /// Latest ZK proof block number and hash (updated by ProofScheduler).
    pub(crate) zk_latest_proof: ArcSwap<Option<(u64, B256)>>,
}

impl SharedConsensusState {
    /// Creates a new shared state. Threshold is read from `N42_MIN_ATTESTATION_THRESHOLD`
    /// (default 10); a value of 0 is clamped to 1.
    pub fn new(validator_set: ValidatorSet) -> Self {
        let threshold = default_attestation_threshold();
        let threshold = if threshold == 0 {
            warn!("N42_MIN_ATTESTATION_THRESHOLD=0 is invalid, clamping to 1");
            1
        } else {
            threshold
        };
        let (block_committed_tx, _) = broadcast::channel(512);
        Self {
            latest_committed_qc: ArcSwap::from_pointee(None),
            validator_set: Arc::new(ArcSwapOption::from_pointee(validator_set)),
            attestation_state: Mutex::new(AttestationState::new(
                threshold,
                MAX_TRACKED_ATTESTATION_BLOCKS,
            )),
            block_committed_tx,
            attestation_history: Mutex::new(VecDeque::new()),
            equivocation_log: Mutex::new(VecDeque::new()),
            authorized_verifiers: Mutex::new(HashSet::new()),
            jmt_root: ArcSwap::from_pointee(None),
            zk_latest_proof: ArcSwap::from_pointee(None),
        }
    }

    /// Returns the number of validators in the current set.
    pub fn validator_count(&self) -> u32 {
        self.validator_set
            .load_full()
            .map(|vs| vs.len())
            .unwrap_or_default()
    }

    /// Loads the current validator set snapshot, if initialized.
    pub fn try_load_validator_set(&self) -> Option<Arc<ValidatorSet>> {
        self.validator_set.load_full()
    }

    /// Loads the current validator set snapshot.
    pub fn load_validator_set(&self) -> Arc<ValidatorSet> {
        self.try_load_validator_set().unwrap_or_else(|| {
            tracing::error!("validator_set unexpectedly uninitialized");
            Arc::new(
                ValidatorSet::try_new(&[], 0)
                    .expect("empty validator set should always be valid"),
            )
        })
    }

    /// Replaces the current validator set snapshot.
    pub fn update_validator_set(&self, validator_set: ValidatorSet) {
        self.validator_set.store(Some(Arc::new(validator_set)));
    }

    /// Creates a new broadcast subscriber for block-committed events.
    pub fn subscribe_block_committed(&self) -> broadcast::Receiver<VerificationTask> {
        self.block_committed_tx.subscribe()
    }

    pub fn update_committed_qc(&self, qc: QuorumCertificate) {
        self.latest_committed_qc.store(Arc::new(Some(qc)));
    }

    pub fn load_committed_qc(&self) -> Arc<Option<QuorumCertificate>> {
        self.latest_committed_qc.load_full()
    }

    pub fn record_attestation(&self, block_hash: B256, block_number: u64, valid_count: u32) {
        let record = AttestationRecord {
            block_hash,
            block_number,
            valid_count,
            timestamp: unix_now_secs(),
        };
        let mut history = self.attestation_history.lock().unwrap_or_else(|e| {
            tracing::error!("attestation_history mutex poisoned: {e}");
            e.into_inner()
        });
        if history.len() >= MAX_ATTESTATION_HISTORY {
            history.pop_front();
        }
        history.push_back(record);
    }

    pub fn get_block_attestation(&self, block_hash: &B256) -> Option<AttestationRecord> {
        let history = self.attestation_history.lock().unwrap_or_else(|e| {
            tracing::error!("attestation_history mutex poisoned: {e}");
            e.into_inner()
        });
        history
            .iter()
            .find(|r| r.block_hash == *block_hash)
            .cloned()
    }

    /// Returns `(total, earliest_block_number, latest_block_number)`.
    pub fn attestation_stats(&self) -> (usize, Option<u64>, Option<u64>) {
        let history = self.attestation_history.lock().unwrap_or_else(|e| {
            tracing::error!("attestation_history mutex poisoned: {e}");
            e.into_inner()
        });
        (
            history.len(),
            history.front().map(|r| r.block_number),
            history.back().map(|r| r.block_number),
        )
    }

    pub fn record_equivocation(&self, view: u64, validator_index: u32, hash1: B256, hash2: B256) {
        let evidence = EquivocationEvidence {
            view,
            validator_index,
            hash1,
            hash2,
            detected_at: unix_now_secs(),
        };
        let mut log = self.equivocation_log.lock().unwrap_or_else(|e| {
            tracing::error!("equivocation_log mutex poisoned: {e}");
            e.into_inner()
        });
        if log.len() >= MAX_EQUIVOCATION_LOG {
            log.pop_front();
        }
        log.push_back(evidence);
    }

    pub fn get_equivocations(&self) -> Vec<EquivocationEvidence> {
        let log = self.equivocation_log.lock().unwrap_or_else(|e| {
            tracing::error!("equivocation_log mutex poisoned: {e}");
            e.into_inner()
        });
        log.iter().cloned().collect()
    }

    /// Marks a verifier pubkey as authorized (called when a QUIC handshake completes).
    pub fn authorize_verifier(&self, pubkey: [u8; 48]) {
        let mut set = self.authorized_verifiers.lock().unwrap_or_else(|e| {
            tracing::error!("authorized_verifiers mutex poisoned: {e}");
            e.into_inner()
        });
        set.insert(pubkey);
    }

    /// Removes a verifier pubkey from the authorized set (called on QUIC disconnect).
    pub fn deauthorize_verifier(&self, pubkey: &[u8; 48]) {
        let mut set = self.authorized_verifiers.lock().unwrap_or_else(|e| {
            tracing::error!("authorized_verifiers mutex poisoned: {e}");
            e.into_inner()
        });
        set.remove(pubkey);
    }

    /// Returns `true` if the pubkey has been authorized via a QUIC handshake.
    pub fn is_authorized_verifier(&self, pubkey: &[u8; 48]) -> bool {
        let set = self.authorized_verifiers.lock().unwrap_or_else(|e| {
            tracing::error!("authorized_verifiers mutex poisoned: {e}");
            e.into_inner()
        });
        set.contains(pubkey)
    }

    /// Updates the latest JMT root hash and version (called from background JMT update task).
    pub fn update_jmt_root(&self, version: u64, root: B256) {
        self.jmt_root.store(Arc::new(Some((version, root))));
    }

    /// Loads the latest JMT root hash and version, if available.
    pub fn load_jmt_root(&self) -> Arc<Option<(u64, B256)>> {
        self.jmt_root.load_full()
    }

    /// Updates the latest ZK proof block number and hash.
    pub fn update_zk_proof(&self, block_number: u64, block_hash: B256) {
        self.zk_latest_proof
            .store(Arc::new(Some((block_number, block_hash))));
    }

    /// Loads the latest ZK proof info (block_number, block_hash), if available.
    pub fn load_zk_latest_proof(&self) -> Arc<Option<(u64, B256)>> {
        self.zk_latest_proof.load_full()
    }

    /// Registers the block for attestation tracking and notifies RPC subscribers.
    pub fn notify_block_committed(&self, block_hash: B256, block_number: u64) {
        let mut att_state = self.attestation_state.lock().unwrap_or_else(|e| {
            tracing::error!("attestation_state mutex poisoned: {e}");
            e.into_inner()
        });
        att_state.register_block(block_hash, block_number);

        let task = VerificationTask {
            block_hash,
            block_number,
        };
        if self.block_committed_tx.receiver_count() > 0 {
            match self.block_committed_tx.send(task) {
                Ok(n) => {
                    info!(
                        %block_hash,
                        block_number,
                        receivers = n,
                        "verification task broadcast to mobile subscribers"
                    );
                }
                Err(error) => {
                    warn!(
                        %block_hash,
                        block_number,
                        error = %error,
                        "verification task broadcast had subscribers but delivery failed"
                    );
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
        SharedConsensusState::new(ValidatorSet::new(&[], 0))
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
        assert!(reached);
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
        assert!(
            att.record_attestation(B256::repeat_byte(0xCC), "pk1".into())
                .is_none()
        );
    }

    #[test]
    fn test_attestation_eviction() {
        let mut att = AttestationState::new(3, 2);
        let h1 = B256::repeat_byte(0x01);
        let h2 = B256::repeat_byte(0x02);
        let h3 = B256::repeat_byte(0x03);

        att.register_block(h1, 1);
        att.register_block(h2, 2);
        att.register_block(h3, 3);

        assert_eq!(att.blocks.len(), 2);
        assert!(
            att.get_attestation_count(&h1).is_none(),
            "h1 should be evicted"
        );
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

        let att = state.attestation_state.lock().unwrap();
        assert_eq!(att.get_attestation_count(&hash), Some(0));
    }

    #[test]
    fn test_record_attestation() {
        let state = make_state();
        let hash = B256::repeat_byte(0xAA);
        state.record_attestation(hash, 10, 5);

        let record = state.get_block_attestation(&hash).unwrap();
        assert_eq!(record.block_number, 10);
        assert_eq!(record.valid_count, 5);
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
        for i in 0..510u64 {
            state.record_equivocation(i, 0, B256::repeat_byte(0xAA), B256::repeat_byte(0xBB));
        }
        let evs = state.get_equivocations();
        assert_eq!(evs.len(), 500);
        assert_eq!(evs[0].view, 10, "oldest 10 entries should be evicted");
        assert_eq!(evs[499].view, 509);
    }

    #[test]
    fn test_attestation_state_threshold() {
        let mut att = AttestationState::new(1, 100);
        let hash = B256::repeat_byte(0xEE);
        att.register_block(hash, 1);

        let (count, reached) = att.record_attestation(hash, "pk1".into()).unwrap();
        assert_eq!(count, 1);
        assert!(reached, "threshold=1, first attestation should reach it");
    }

    #[test]
    fn test_authorized_verifiers() {
        let state = make_state();
        let pubkey = [0x42u8; 48];

        // Initially not authorized.
        assert!(!state.is_authorized_verifier(&pubkey));

        // After authorize, should be authorized.
        state.authorize_verifier(pubkey);
        assert!(state.is_authorized_verifier(&pubkey));

        // After deauthorize, should no longer be authorized.
        state.deauthorize_verifier(&pubkey);
        assert!(!state.is_authorized_verifier(&pubkey));
    }

    #[test]
    fn test_attestation_history_bounded() {
        let state = make_state();
        for i in 0..1010u64 {
            let mut hash_bytes = [0u8; 32];
            hash_bytes[0..8].copy_from_slice(&i.to_le_bytes());
            state.record_attestation(B256::from(hash_bytes), i, 1);
        }
        let (total, earliest, _) = state.attestation_stats();
        assert_eq!(total, 1000);
        assert_eq!(earliest, Some(10));
    }
}
