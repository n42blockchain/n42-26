use alloy_primitives::B256;
use metrics::{counter, gauge};
use n42_mobile::{AttestationBuilder, ReceiptAggregator, VerificationReceipt};
use n42_network::mobile::HubEvent;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info, warn};

use crate::attestation_store::AttestationStore;
use crate::consensus_state::{SharedConsensusState, VerificationTask};
use crate::staking::StakingManager;

/// Notification sent when a block reaches the mobile attestation threshold.
#[derive(Debug, Clone)]
pub struct AttestationEvent {
    pub block_hash: B256,
    pub block_number: u64,
    pub valid_count: u32,
}

#[derive(Debug, Clone)]
struct FinalizedAttestation {
    event: AttestationEvent,
    reward_pubkeys: Vec<[u8; 48]>,
}

fn min_attestation_threshold() -> u32 {
    std::env::var("N42_MIN_ATTESTATION_THRESHOLD")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(10)
}

/// Bridges StarHub mobile verification events with the ReceiptAggregator.
pub struct MobileVerificationBridge {
    hub_event_rx: mpsc::Receiver<HubEvent>,
    receipt_aggregator: ReceiptAggregator,
    attestation_tx: Option<mpsc::Sender<AttestationEvent>>,
    phone_connected_tx: Option<mpsc::Sender<u64>>,
    reward_tx: Option<mpsc::Sender<[u8; 48]>>,
    /// Maps session_id → verifier BLS pubkey for deauthorization on disconnect.
    connected_sessions: HashMap<u64, [u8; 48]>,
    /// Optional shared consensus state; when set, verifier pubkeys are
    /// authorized/deauthorized in `SharedConsensusState` on connect/disconnect.
    consensus_state: Option<Arc<SharedConsensusState>>,
    /// Optional staking manager; when set, only staked verifiers are authorized.
    staking_manager: Option<Arc<Mutex<StakingManager>>>,
    /// Subscription to committed-block notifications for registering blocks that
    /// are eligible to receive mobile receipts.
    verification_task_rx: Option<broadcast::Receiver<VerificationTask>>,
    /// Per-block invalid receipt counts for divergence detection (FIFO-bounded).
    invalid_receipt_counts: HashMap<B256, u32>,
    invalid_receipt_order: VecDeque<B256>,
    /// Per-block first-receipt timestamps for attestation latency (FIFO-bounded).
    block_first_receipt_at: HashMap<B256, std::time::Instant>,
    block_first_receipt_order: VecDeque<B256>,
    max_tracked: usize,
    /// Per-block BLS aggregate signature builders.
    /// Created when a block is registered with `expected_receipts_root = Some(...)`.
    attestation_builders: HashMap<B256, AttestationBuilder>,
    /// Persistent store for completed aggregated attestations and reward tracking.
    attestation_store: Option<Arc<Mutex<AttestationStore>>>,
}

impl MobileVerificationBridge {
    pub fn new(
        hub_event_rx: mpsc::Receiver<HubEvent>,
        default_threshold: u32,
        max_tracked_blocks: usize,
    ) -> Self {
        let default_threshold = if default_threshold == 0 {
            warn!(target: "n42::mobile", "default_threshold is 0, clamping to 1");
            1
        } else {
            default_threshold
        };
        Self {
            hub_event_rx,
            receipt_aggregator: ReceiptAggregator::new(default_threshold, max_tracked_blocks),
            attestation_tx: None,
            phone_connected_tx: None,
            reward_tx: None,
            connected_sessions: HashMap::new(),
            consensus_state: None,
            staking_manager: None,
            verification_task_rx: None,
            invalid_receipt_counts: HashMap::new(),
            invalid_receipt_order: VecDeque::new(),
            block_first_receipt_at: HashMap::new(),
            block_first_receipt_order: VecDeque::new(),
            max_tracked: max_tracked_blocks,
            attestation_builders: HashMap::new(),
            attestation_store: None,
        }
    }

    pub fn with_attestation_tx(mut self, tx: mpsc::Sender<AttestationEvent>) -> Self {
        self.attestation_tx = Some(tx);
        self
    }

    pub fn with_phone_connected_tx(mut self, tx: mpsc::Sender<u64>) -> Self {
        self.phone_connected_tx = Some(tx);
        self
    }

    pub fn with_reward_tx(mut self, tx: mpsc::Sender<[u8; 48]>) -> Self {
        self.reward_tx = Some(tx);
        self
    }

    /// Attaches a staking manager for verifier authorization checks.
    /// When set, only registered (staked or registered) verifiers are authorized.
    pub fn with_staking_manager(mut self, mgr: Arc<Mutex<StakingManager>>) -> Self {
        self.staking_manager = Some(mgr);
        self
    }

    /// Attaches an attestation store for BLS aggregate signature persistence.
    /// When set, completed attestations are persisted to disk and verifier
    /// reward points are tracked across restarts.
    pub fn with_attestation_store(mut self, store: Arc<Mutex<AttestationStore>>) -> Self {
        self.attestation_store = Some(store);
        self
    }

    /// Attaches shared consensus state so that verifier pubkeys are authorized/
    /// deauthorized in `SharedConsensusState` as phones connect and disconnect.
    pub fn with_consensus_state(mut self, state: Arc<SharedConsensusState>) -> Self {
        self.verification_task_rx = Some(state.block_committed_tx.subscribe());
        self.consensus_state = Some(state);
        self
    }

    /// Registers a block dispatched to mobile verifiers for tracking.
    ///
    /// Must be called by the node when it sends a verification task to phones.
    /// Receipts for blocks not registered here are silently dropped.
    pub fn register_dispatched_block(
        &mut self,
        block_hash: B256,
        block_number: u64,
        expected_receipts_root: Option<B256>,
    ) {
        self.receipt_aggregator
            .register_block(block_hash, block_number, expected_receipts_root);

        // Create an AttestationBuilder for BLS aggregation when we know the expected root.
        if let Some(receipts_root) = expected_receipts_root {
            self.attestation_builders
                .entry(block_hash)
                .or_insert_with(|| {
                    AttestationBuilder::new(block_hash, block_number, receipts_root)
                });
        }
    }

    /// Evicts the oldest entry from a FIFO-bounded map when at capacity, then inserts the key.
    fn fifo_ensure_entry<V>(
        map: &mut HashMap<B256, V>,
        order: &mut VecDeque<B256>,
        key: B256,
        max: usize,
    ) -> bool {
        if map.contains_key(&key) {
            return false;
        }
        if map.len() >= max
            && let Some(oldest) = order.pop_front()
        {
            map.remove(&oldest);
        }
        order.push_back(key);
        true
    }

    pub async fn run(mut self) {
        info!(target: "n42::mobile", "mobile verification bridge started");

        loop {
            tokio::select! {
                event = self.hub_event_rx.recv() => {
                    let Some(event) = event else { break };
                    match event {
                        HubEvent::PhoneConnected { session_id, verifier_pubkey } => {
                            // Check registration status before authorizing
                            let open_verification = std::env::var("N42_OPEN_VERIFICATION")
                                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                                .unwrap_or(false);
                            let registered = if open_verification {
                                true // Open verification mode: accept all verifiers
                            } else if let Some(ref staking_mgr) = self.staking_manager {
                                let mgr = staking_mgr.lock().unwrap_or_else(|e| e.into_inner());
                                mgr.is_registered(&verifier_pubkey)
                            } else {
                                true // Backward compatible: no StakingManager = allow all (dev mode)
                            };

                            if !registered {
                                warn!(
                                    target: "n42::mobile",
                                    session_id,
                                    pubkey = hex::encode(&verifier_pubkey[..8]),
                                    "phone verifier not registered, rejecting"
                                );
                                continue;
                            }

                            self.connected_sessions.insert(session_id, verifier_pubkey);
                            if let Some(ref state) = self.consensus_state {
                                state.authorize_verifier(verifier_pubkey);
                            }
                            // Register verifier in the attestation store's registry
                            // so it gets a bitfield index for aggregate signatures.
                            if let Some(ref store) = self.attestation_store {
                                let mut s = store.lock().unwrap_or_else(|e| e.into_inner());
                                s.registry_mut().register(verifier_pubkey);
                            }
                            gauge!("n42_mobile_connected_phones").set(self.connected_sessions.len() as f64);
                            self.update_dynamic_threshold();
                            debug!(
                                target: "n42::mobile",
                                session_id,
                                connected = self.connected_sessions.len(),
                                "mobile verifier connected"
                            );
                            if let Some(ref tx) = self.phone_connected_tx {
                                if let Err(error) = tx.send(session_id).await {
                                    warn!(
                                        target: "n42::mobile",
                                        session_id,
                                        error = %error,
                                        "failed to deliver phone_connected notification"
                                    );
                                }
                            }
                        }
                        HubEvent::PhoneDisconnected { session_id } => {
                            match self.connected_sessions.remove(&session_id) {
                                Some(pubkey) => {
                                    if let Some(ref state) = self.consensus_state {
                                        state.deauthorize_verifier(&pubkey);
                                    }
                                }
                                None => {
                                    warn!(
                                        target: "n42::mobile",
                                        session_id,
                                        "disconnect for unknown session (duplicate event?)"
                                    );
                                }
                            }
                            gauge!("n42_mobile_connected_phones").set(self.connected_sessions.len() as f64);
                            self.update_dynamic_threshold();
                            debug!(
                                target: "n42::mobile",
                                session_id,
                                connected = self.connected_sessions.len(),
                                "mobile verifier disconnected"
                            );
                        }
                        HubEvent::ReceiptReceived(receipt) => {
                            info!(
                                target: "n42::mobile",
                                block_number = receipt.block_number,
                                %receipt.block_hash,
                                "verification receipt received from mobile verifier"
                            );
                            self.process_receipt_async(&receipt).await;
                        }
                        HubEvent::CacheInventoryReceived { session_id, code_hashes } => {
                            debug!(
                                target: "n42::mobile",
                                session_id,
                                count = code_hashes.len(),
                                "cache inventory received"
                            );
                        }
                    }
                }
                task = async {
                    // When verification_task_rx is None, yield a never-completing
                    // future so tokio::select! does not busy-loop on a ready None.
                    match self.verification_task_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    match task {
                        Ok(task) => {
                            self.register_dispatched_block(task.block_hash, task.block_number, None);
                            debug!(
                                target: "n42::mobile",
                                block_hash = %task.block_hash,
                                block_number = task.block_number,
                                "registered dispatched block for mobile receipt tracking"
                            );
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            warn!(
                                target: "n42::mobile",
                                skipped = n,
                                "mobile bridge lagged on committed-block notifications"
                            );
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            self.verification_task_rx = None;
                        }
                    }
                }
            }
        }

        info!(target: "n42::mobile", "mobile verification bridge shutting down");
    }

    fn update_dynamic_threshold(&mut self) {
        let connected = self.connected_sessions.len() as u32;
        let threshold = (connected * 2 / 3).max(min_attestation_threshold());
        self.receipt_aggregator.set_default_threshold(threshold);
        debug!(target: "n42::mobile", connected, threshold, "attestation threshold updated");
    }

    /// Builds a BLS aggregate attestation from collected signatures and persists it.
    ///
    /// Called when the attestation threshold is reached. The builder is consumed
    /// (removed from the map) to produce the final aggregate.
    fn finalize_attestation(
        &mut self,
        block_hash: B256,
        block_number: u64,
        valid_count: u32,
    ) -> Vec<[u8; 48]> {
        let Some(builder) = self.attestation_builders.remove(&block_hash) else {
            return Vec::new();
        };

        let sig_count = builder.count();
        if sig_count == 0 {
            return Vec::new();
        }

        match builder.build() {
            Ok(attestation) => {
                info!(
                    target: "n42::mobile",
                    block_number,
                    %block_hash,
                    participant_count = attestation.participant_count,
                    valid_count,
                    "BLS aggregate attestation built"
                );
                counter!("n42_mobile_aggregate_attestations_total").increment(1);

                let reward_pubkeys = if let Some(ref store) = self.attestation_store {
                    let s = store.lock().unwrap_or_else(|e| e.into_inner());
                    let mut reward_pubkeys = Vec::new();
                    for (byte_idx, &byte) in attestation.participant_bitfield.iter().enumerate() {
                        for bit in 0..8u32 {
                            if byte & (1 << bit) != 0 {
                                let index = byte_idx as u32 * 8 + bit;
                                if let Some(pubkey) = s.registry().pubkey_at(index) {
                                    reward_pubkeys.push(*pubkey);
                                }
                            }
                        }
                    }
                    reward_pubkeys
                } else {
                    Vec::new()
                };

                if let Some(ref store) = self.attestation_store {
                    let mut s = store.lock().unwrap_or_else(|e| e.into_inner());
                    s.record_attestation(&attestation);
                    if let Err(e) = s.save() {
                        warn!(
                            target: "n42::mobile",
                            block_number,
                            error = %e,
                            "failed to persist attestation store"
                        );
                    }
                }

                debug!(
                    target: "n42::mobile",
                    block_number,
                    rewarded = reward_pubkeys.len(),
                    "reward pubkeys prepared for finalized attestation"
                );

                return reward_pubkeys;
            }
            Err(e) => {
                warn!(
                    target: "n42::mobile",
                    block_number,
                    %block_hash,
                    sig_count,
                    error = %e,
                    "failed to build BLS aggregate attestation"
                );
            }
        }

        Vec::new()
    }

    fn process_receipt_inner(
        &mut self,
        receipt: &VerificationReceipt,
    ) -> Option<FinalizedAttestation> {
        if let Err(e) = receipt.verify_signature() {
            warn!(
                target: "n42::mobile",
                block_number = receipt.block_number,
                error = %e,
                "receipt signature invalid, dropping"
            );
            return None;
        }

        // Check validity: compare computed_receipts_root against expected.
        let is_valid = self
            .receipt_aggregator
            .get_status(&receipt.block_hash)
            .and_then(|s| {
                s.expected_receipts_root
                    .as_ref()
                    .map(|expected| receipt.matches_expected(expected))
            })
            .unwrap_or(true); // No expected root or untracked block → defer to aggregator

        if !is_valid {
            counter!("n42_mobile_invalid_receipts_total").increment(1);
            warn!(
                target: "n42::mobile",
                block_number = receipt.block_number,
                %receipt.block_hash,
                computed = %receipt.computed_receipts_root,
                "mobile verifier reported MISMATCHED receipts root"
            );

            Self::fifo_ensure_entry(
                &mut self.invalid_receipt_counts,
                &mut self.invalid_receipt_order,
                receipt.block_hash,
                self.max_tracked,
            );
            let count = self
                .invalid_receipt_counts
                .entry(receipt.block_hash)
                .or_insert(0);
            *count += 1;

            let connected = self.connected_sessions.len() as u32;
            let alert_threshold = if connected > 0 { connected / 3 } else { 1 };
            if *count >= alert_threshold {
                tracing::error!(
                    target: "n42::mobile",
                    block_number = receipt.block_number,
                    %receipt.block_hash,
                    invalid_count = *count,
                    connected,
                    "CRITICAL: potential state divergence — invalid receipts exceeded 1/3 of connected verifiers"
                );
            }
        } else {
            counter!("n42_mobile_valid_receipts_total").increment(1);
        }

        // `register_block` is intentionally NOT called here. Blocks must be
        // registered via `register_dispatched_block` when the node dispatches
        // a verification task. Any receipt for a block not in the aggregator
        // returns `None` and is silently discarded without counting towards the
        // attestation threshold or triggering a reward.

        if Self::fifo_ensure_entry(
            &mut self.block_first_receipt_at,
            &mut self.block_first_receipt_order,
            receipt.block_hash,
            self.max_tracked,
        ) {
            self.block_first_receipt_at
                .insert(receipt.block_hash, std::time::Instant::now());
        }

        // Feed valid receipts into the BLS attestation builder.
        if is_valid && let Some(ref store) = self.attestation_store {
            let store_guard = store.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(builder) = self.attestation_builders.get_mut(&receipt.block_hash) {
                builder.add_receipt(receipt, store_guard.registry());
            }
        }

        match self.receipt_aggregator.process_receipt(receipt) {
            Some(true) => {
                // New unique receipt for a dispatched block; threshold just crossed.
                let valid_count = self
                    .receipt_aggregator
                    .get_status(&receipt.block_hash)
                    .map(|s| s.valid_count)
                    .unwrap_or(0);

                if let Some(first_at) = self.block_first_receipt_at.get(&receipt.block_hash) {
                    let latency_ms = first_at.elapsed().as_millis() as u64;
                    metrics::histogram!("n42_mobile_attestation_latency_ms")
                        .record(latency_ms as f64);
                    info!(
                        target: "n42::mobile",
                        block_number = receipt.block_number,
                        latency_ms,
                        "attestation latency: first receipt → threshold"
                    );
                }

                // Build BLS aggregate signature and persist.
                let reward_pubkeys = self.finalize_attestation(
                    receipt.block_hash,
                    receipt.block_number,
                    valid_count,
                );

                info!(
                    target: "n42::mobile",
                    block_number = receipt.block_number,
                    %receipt.block_hash,
                    valid_count,
                    "block reached attestation threshold"
                );

                return Some(FinalizedAttestation {
                    event: AttestationEvent {
                        block_hash: receipt.block_hash,
                        block_number: receipt.block_number,
                        valid_count,
                    },
                    reward_pubkeys,
                });
            }
            Some(false) => {
                // New unique receipt for a dispatched block; threshold not yet reached.
                debug!(
                    target: "n42::mobile",
                    block_number = receipt.block_number,
                    "receipt processed, threshold not yet reached"
                );
            }
            None => {
                // Block not dispatched by this node, or duplicate receipt from same verifier.
                warn!(
                    target: "n42::mobile",
                    block_number = receipt.block_number,
                    "receipt for untracked block or duplicate verifier, dropping"
                );
            }
        }

        None
    }

    fn try_emit_reward_pubkeys(
        &self,
        block_hash: B256,
        block_number: u64,
        reward_pubkeys: &[[u8; 48]],
    ) {
        let Some(ref tx) = self.reward_tx else {
            return;
        };

        let mut rewarded = 0usize;
        for pubkey in reward_pubkeys {
            match tx.try_send(*pubkey) {
                Ok(()) => {
                    rewarded += 1;
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    warn!(
                        target: "n42::mobile",
                        block_number,
                        %block_hash,
                        "reward channel closed while emitting attestation participants"
                    );
                    break;
                }
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    warn!(
                        target: "n42::mobile",
                        block_number,
                        %block_hash,
                        "reward channel full in synchronous path, dropping remaining participants"
                    );
                    break;
                }
            }
        }

        debug!(
            target: "n42::mobile",
            block_number,
            rewarded,
            "reward pubkeys sent for finalized attestation"
        );
    }

    async fn emit_reward_pubkeys(
        &self,
        block_hash: B256,
        block_number: u64,
        reward_pubkeys: Vec<[u8; 48]>,
    ) {
        let Some(ref tx) = self.reward_tx else {
            return;
        };

        let mut rewarded = 0usize;
        for pubkey in reward_pubkeys {
            match tx.try_send(pubkey) {
                Ok(()) => {
                    rewarded += 1;
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    warn!(
                        target: "n42::mobile",
                        block_number,
                        %block_hash,
                        "reward channel closed while emitting attestation participants"
                    );
                    break;
                }
                Err(tokio::sync::mpsc::error::TrySendError::Full(pubkey)) => {
                    warn!(
                        target: "n42::mobile",
                        block_number,
                        %block_hash,
                        "reward channel full, waiting to enqueue"
                    );
                    if let Err(error) = tx.send(pubkey).await {
                        warn!(
                            target: "n42::mobile",
                            block_number,
                            %block_hash,
                            error = %error,
                            "failed to deliver reward participant after backpressure wait"
                        );
                        break;
                    }
                    rewarded += 1;
                }
            }
        }

        debug!(
            target: "n42::mobile",
            block_number,
            rewarded,
            "reward pubkeys sent for finalized attestation"
        );
    }

    fn try_emit_attestation_event(&self, event: AttestationEvent) {
        let Some(ref tx) = self.attestation_tx else {
            return;
        };

        let block_hash = event.block_hash;
        let block_number = event.block_number;
        match tx.try_send(event) {
            Ok(()) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                warn!(
                    target: "n42::mobile",
                    block_number,
                    %block_hash,
                    "failed to deliver attestation event: channel closed"
                );
            }
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                warn!(
                    target: "n42::mobile",
                    block_number,
                    %block_hash,
                    "attestation event channel full in synchronous path, dropping notification"
                );
            }
        }
    }

    async fn emit_attestation_event(&self, event: AttestationEvent) {
        let Some(ref tx) = self.attestation_tx else {
            return;
        };

        let block_hash = event.block_hash;
        let block_number = event.block_number;
        match tx.try_send(event) {
            Ok(()) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                warn!(
                    target: "n42::mobile",
                    block_number,
                    %block_hash,
                    "failed to deliver attestation event: channel closed"
                );
            }
            Err(tokio::sync::mpsc::error::TrySendError::Full(event)) => {
                warn!(
                    target: "n42::mobile",
                    block_number,
                    %block_hash,
                    "attestation event channel full, waiting to enqueue"
                );
                if let Err(error) = tx.send(event).await {
                    warn!(
                        target: "n42::mobile",
                        block_number,
                        %block_hash,
                        error = %error,
                        "failed to deliver attestation event after backpressure wait"
                    );
                }
            }
        }
    }

    fn try_emit_finalized_attestation(&self, finalized: FinalizedAttestation) {
        self.try_emit_reward_pubkeys(
            finalized.event.block_hash,
            finalized.event.block_number,
            &finalized.reward_pubkeys,
        );
        self.try_emit_attestation_event(finalized.event);
    }

    async fn emit_finalized_attestation(&self, finalized: FinalizedAttestation) {
        self.emit_reward_pubkeys(
            finalized.event.block_hash,
            finalized.event.block_number,
            finalized.reward_pubkeys,
        )
        .await;
        self.emit_attestation_event(finalized.event).await;
    }

    /// Public for testing; production code calls this via the async event loop.
    ///
    /// Receipts for blocks not registered via `register_dispatched_block` are
    /// silently dropped (aggregator returns `None`). This prevents arbitrary
    /// block hashes from being injected by a malicious verifier.
    pub fn process_receipt(&mut self, receipt: &VerificationReceipt) {
        if let Some(finalized) = self.process_receipt_inner(receipt) {
            self.try_emit_finalized_attestation(finalized);
        }
    }

    async fn process_receipt_async(&mut self, receipt: &VerificationReceipt) {
        if let Some(finalized) = self.process_receipt_inner(receipt) {
            self.emit_finalized_attestation(finalized).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::B256;
    use n42_consensus::ValidatorSet;
    use n42_mobile::receipt::sign_receipt;
    use n42_primitives::BlsSecretKey;

    /// The expected receipts root used in test receipts.
    const TEST_RR: B256 = B256::new([0xAA; 32]);

    fn make_receipt(block_hash: B256, block_number: u64) -> VerificationReceipt {
        let key = BlsSecretKey::random().expect("BLS key gen");
        sign_receipt(block_hash, block_number, TEST_RR, 1_000_000, &key)
    }

    fn make_consensus_state() -> Arc<SharedConsensusState> {
        Arc::new(SharedConsensusState::new(ValidatorSet::new(&[], 0)))
    }

    fn make_hub_channel() -> (mpsc::Sender<HubEvent>, mpsc::Receiver<HubEvent>) {
        mpsc::channel(32)
    }

    fn make_reward_channel() -> (mpsc::Sender<[u8; 48]>, mpsc::Receiver<[u8; 48]>) {
        mpsc::channel(32)
    }

    #[test]
    fn test_bridge_processes_receipts() {
        let (tx, rx) = make_hub_channel();
        let mut bridge = MobileVerificationBridge::new(rx, 2, 100);

        let block_hash = B256::with_last_byte(0x01);
        bridge.register_dispatched_block(block_hash, 1, Some(TEST_RR));
        bridge.process_receipt(&make_receipt(block_hash, 1));

        let status = bridge.receipt_aggregator.get_status(&block_hash);
        assert!(status.is_some());
        assert_eq!(status.unwrap().total_receipts(), 1);

        bridge.process_receipt(&make_receipt(block_hash, 1));

        let status = bridge.receipt_aggregator.get_status(&block_hash).unwrap();
        assert_eq!(status.total_receipts(), 2);
        assert!(status.is_attested());

        drop(tx);
    }

    #[test]
    fn test_bridge_attestation_notification() {
        let (tx, rx) = make_hub_channel();
        let (attest_tx, mut attest_rx) = mpsc::channel(256);
        let mut bridge = MobileVerificationBridge::new(rx, 1, 100).with_attestation_tx(attest_tx);

        let block_hash = B256::with_last_byte(0x03);
        bridge.register_dispatched_block(block_hash, 10, Some(TEST_RR));
        bridge.process_receipt(&make_receipt(block_hash, 10));

        let event = attest_rx
            .try_recv()
            .expect("attestation event should be sent");
        assert_eq!(event.block_hash, block_hash);
        assert_eq!(event.block_number, 10);
        assert_eq!(event.valid_count, 1);

        drop(tx);
    }

    #[test]
    fn test_bridge_multiple_blocks() {
        let (_tx, rx) = make_hub_channel();
        let mut bridge = MobileVerificationBridge::new(rx, 2, 100);

        let hash_a = B256::with_last_byte(0x0A);
        let hash_b = B256::with_last_byte(0x0B);

        bridge.register_dispatched_block(hash_a, 10, Some(TEST_RR));
        bridge.register_dispatched_block(hash_b, 11, Some(TEST_RR));
        bridge.process_receipt(&make_receipt(hash_a, 10));
        bridge.process_receipt(&make_receipt(hash_a, 10));
        bridge.process_receipt(&make_receipt(hash_b, 11));

        let status_a = bridge.receipt_aggregator.get_status(&hash_a).unwrap();
        assert_eq!(status_a.total_receipts(), 2);
        assert!(status_a.is_attested());

        let status_b = bridge.receipt_aggregator.get_status(&hash_b).unwrap();
        assert_eq!(status_b.total_receipts(), 1);
        assert!(!status_b.is_attested());
    }

    #[test]
    fn test_bridge_duplicate_receipt_ignored() {
        let (_tx, rx) = make_hub_channel();
        let mut bridge = MobileVerificationBridge::new(rx, 2, 100);

        let block_hash = B256::with_last_byte(0x0C);
        bridge.register_dispatched_block(block_hash, 20, Some(TEST_RR));
        let receipt = make_receipt(block_hash, 20);
        bridge.process_receipt(&receipt);
        bridge.process_receipt(&receipt);

        let status = bridge.receipt_aggregator.get_status(&block_hash).unwrap();
        assert_eq!(
            status.total_receipts(),
            1,
            "duplicate receipt from same verifier should be ignored"
        );
    }

    #[test]
    fn test_bridge_no_attestation_tx() {
        let (_tx, rx) = make_hub_channel();
        let mut bridge = MobileVerificationBridge::new(rx, 1, 100);

        let block_hash = B256::with_last_byte(0x0D);
        bridge.register_dispatched_block(block_hash, 30, Some(TEST_RR));
        bridge.process_receipt(&make_receipt(block_hash, 30));

        assert!(
            bridge
                .receipt_aggregator
                .get_status(&block_hash)
                .unwrap()
                .is_attested()
        );
    }

    #[test]
    fn test_bridge_dynamic_threshold() {
        let (_tx, rx) = make_hub_channel();
        let mut bridge = MobileVerificationBridge::new(rx, 2, 100);

        for i in 0..30u64 {
            bridge.connected_sessions.insert(i, [0u8; 48]);
        }
        bridge.update_dynamic_threshold();

        let block_hash = B256::with_last_byte(0xF1);
        bridge.register_dispatched_block(block_hash, 100, Some(TEST_RR));
        bridge.process_receipt(&make_receipt(block_hash, 100));
        assert!(
            !bridge
                .receipt_aggregator
                .get_status(&block_hash)
                .unwrap()
                .is_attested(),
            "with ~20 threshold, 1 receipt should not attest"
        );
    }

    #[test]
    fn test_bridge_invalid_receipt_counts_bounded() {
        let (_tx, rx) = make_hub_channel();
        let max_tracked = 3;
        let mut bridge = MobileVerificationBridge::new(rx, 100, max_tracked);

        let wrong_rr = B256::from([0xFF; 32]); // Mismatched receipts root
        for i in 0..4u8 {
            let block_hash = B256::with_last_byte(i);
            // Register with expected TEST_RR, but send receipt with wrong computed root.
            bridge.register_dispatched_block(block_hash, i as u64, Some(TEST_RR));
            let key = BlsSecretKey::random().expect("BLS key gen");
            let receipt = sign_receipt(block_hash, i as u64, wrong_rr, 1_000_000, &key);
            bridge.process_receipt(&receipt);
        }

        assert_eq!(bridge.invalid_receipt_counts.len(), max_tracked);
        assert!(
            !bridge
                .invalid_receipt_counts
                .contains_key(&B256::with_last_byte(0))
        );
        assert!(
            bridge
                .invalid_receipt_counts
                .contains_key(&B256::with_last_byte(1))
        );
        assert!(
            bridge
                .invalid_receipt_counts
                .contains_key(&B256::with_last_byte(2))
        );
        assert!(
            bridge
                .invalid_receipt_counts
                .contains_key(&B256::with_last_byte(3))
        );
    }

    #[test]
    fn test_bridge_handles_all_event_types() {
        let (tx, rx) = make_hub_channel();
        let mut bridge = MobileVerificationBridge::new(rx, 2, 100);

        let block_hash = B256::with_last_byte(0x02);
        let receipt = make_receipt(block_hash, 42);

        tx.blocking_send(HubEvent::PhoneConnected {
            session_id: 1,
            verifier_pubkey: [0u8; 48],
        })
        .unwrap();
        tx.blocking_send(HubEvent::ReceiptReceived(Box::new(receipt)))
            .unwrap();
        tx.blocking_send(HubEvent::CacheInventoryReceived {
            session_id: 1,
            code_hashes: vec![[0xAA; 32]],
        })
        .unwrap();
        tx.blocking_send(HubEvent::PhoneDisconnected { session_id: 1 })
            .unwrap();

        while let Ok(event) = bridge.hub_event_rx.try_recv() {
            if let HubEvent::ReceiptReceived(ref r) = event {
                bridge.process_receipt(r);
            }
        }
    }

    #[test]
    fn test_attestation_latency_recorded() {
        let (_tx, rx) = make_hub_channel();
        let mut bridge = MobileVerificationBridge::new(rx, 2, 100);

        let block_hash = B256::with_last_byte(0xE1);
        bridge.register_dispatched_block(block_hash, 50, Some(TEST_RR));
        bridge.process_receipt(&make_receipt(block_hash, 50));
        assert!(bridge.block_first_receipt_at.contains_key(&block_hash));

        bridge.process_receipt(&make_receipt(block_hash, 50));
        assert!(
            bridge
                .receipt_aggregator
                .get_status(&block_hash)
                .unwrap()
                .is_attested()
        );
        assert!(bridge.block_first_receipt_at.contains_key(&block_hash));
    }

    #[test]
    fn test_first_receipt_time_bounded() {
        let (_tx, rx) = make_hub_channel();
        let max_tracked = 3;
        let mut bridge = MobileVerificationBridge::new(rx, 100, max_tracked);

        for i in 0..4u8 {
            bridge.process_receipt(&make_receipt(B256::with_last_byte(0xF0 + i), i as u64));
        }

        assert_eq!(bridge.block_first_receipt_at.len(), max_tracked);
        assert!(
            !bridge
                .block_first_receipt_at
                .contains_key(&B256::with_last_byte(0xF0))
        );
    }

    #[test]
    fn test_attestation_latency_no_panic_without_first_receipt() {
        let (_tx, rx) = make_hub_channel();
        let mut bridge = MobileVerificationBridge::new(rx, 1, 100);

        let block_hash = B256::with_last_byte(0xE2);
        bridge.register_dispatched_block(block_hash, 60, Some(TEST_RR));
        bridge.process_receipt(&make_receipt(block_hash, 60));
        assert!(
            bridge
                .receipt_aggregator
                .get_status(&block_hash)
                .unwrap()
                .is_attested()
        );
    }

    #[test]
    fn test_reward_tx_sends_pubkeys_on_finalized_attestation() {
        use crate::attestation_store::AttestationStore;

        let store_path = std::env::temp_dir().join("n42-test-reward-finalized.json");
        let _ = std::fs::remove_file(&store_path);
        let store = Arc::new(Mutex::new(
            AttestationStore::new(store_path.clone()).unwrap(),
        ));

        let (_hub_tx, hub_rx) = make_hub_channel();
        let (reward_tx, mut reward_rx) = make_reward_channel();
        // threshold = 2: need 2 receipts to finalize
        let mut bridge = MobileVerificationBridge::new(hub_rx, 2, 100)
            .with_reward_tx(reward_tx)
            .with_attestation_store(store.clone());

        let block_hash = B256::with_last_byte(0xA1);

        // Create 2 verifiers with real BLS keys and register them.
        let keys: Vec<_> = (0..2u64)
            .map(|i| {
                let mut seed = [0u8; 32];
                seed[0..8].copy_from_slice(&i.to_le_bytes());
                BlsSecretKey::key_gen(&seed).unwrap()
            })
            .collect();
        {
            let mut s = store.lock().unwrap();
            for key in &keys {
                s.registry_mut().register(key.public_key().to_bytes());
            }
        }

        bridge.register_dispatched_block(block_hash, 100, Some(TEST_RR));

        // First receipt: threshold not reached, no reward yet.
        let receipt1 = sign_receipt(block_hash, 100, TEST_RR, 1_000_000, &keys[0]);
        bridge.process_receipt(&receipt1);
        assert!(
            reward_rx.try_recv().is_err(),
            "no reward before finalization"
        );

        // Second receipt: threshold reached → finalize → batch reward.
        let receipt2 = sign_receipt(block_hash, 100, TEST_RR, 1_000_000, &keys[1]);
        bridge.process_receipt(&receipt2);

        // Both participants should receive reward pubkeys.
        let pk1 = reward_rx.try_recv().expect("first participant reward");
        let pk2 = reward_rx.try_recv().expect("second participant reward");
        assert_ne!(
            pk1, pk2,
            "different participants should have different pubkeys"
        );
        assert!(reward_rx.try_recv().is_err(), "only 2 participants");

        let _ = std::fs::remove_file(&store_path);
    }

    #[test]
    fn test_reward_tx_not_sent_on_invalid_receipt() {
        use crate::attestation_store::AttestationStore;

        let store_path = std::env::temp_dir().join("n42-test-reward-invalid.json");
        let _ = std::fs::remove_file(&store_path);
        let store = Arc::new(Mutex::new(
            AttestationStore::new(store_path.clone()).unwrap(),
        ));

        let (_hub_tx, hub_rx) = make_hub_channel();
        let (reward_tx, mut reward_rx) = make_reward_channel();
        let mut bridge = MobileVerificationBridge::new(hub_rx, 1, 100)
            .with_reward_tx(reward_tx)
            .with_attestation_store(store);

        let block_hash = B256::with_last_byte(0xA2);
        let key = BlsSecretKey::random().expect("BLS key gen");
        let wrong_rr = B256::from([0xFF; 32]);
        let invalid_receipt = sign_receipt(block_hash, 200, wrong_rr, 1_000_000, &key);

        bridge.register_dispatched_block(block_hash, 200, Some(TEST_RR));
        bridge.process_receipt(&invalid_receipt);

        // Invalid receipt should NOT trigger reward (attestation won't finalize with valid sigs)
        assert!(
            reward_rx.try_recv().is_err(),
            "invalid receipt should not send to reward_tx"
        );

        let _ = std::fs::remove_file(&store_path);
    }

    #[test]
    fn test_reward_tx_not_sent_before_threshold() {
        use crate::attestation_store::AttestationStore;

        let store_path = std::env::temp_dir().join("n42-test-reward-below-threshold.json");
        let _ = std::fs::remove_file(&store_path);
        let store = Arc::new(Mutex::new(
            AttestationStore::new(store_path.clone()).unwrap(),
        ));

        let (_hub_tx, hub_rx) = make_hub_channel();
        let (reward_tx, mut reward_rx) = make_reward_channel();
        // threshold = 10, we only send 2
        let mut bridge = MobileVerificationBridge::new(hub_rx, 10, 100)
            .with_reward_tx(reward_tx)
            .with_attestation_store(store);

        let block_hash = B256::with_last_byte(0xA3);
        bridge.register_dispatched_block(block_hash, 300, Some(TEST_RR));
        bridge.process_receipt(&make_receipt(block_hash, 300));
        bridge.process_receipt(&make_receipt(block_hash, 300));

        // Below threshold: no finalization, no rewards
        assert!(
            reward_rx.try_recv().is_err(),
            "no reward before threshold reached"
        );

        let _ = std::fs::remove_file(&store_path);
    }

    #[test]
    fn test_no_reward_tx_configured_no_panic() {
        let (_hub_tx, hub_rx) = make_hub_channel();
        let mut bridge = MobileVerificationBridge::new(hub_rx, 1, 100);
        // No reward_tx configured — should not panic
        let block_hash = B256::with_last_byte(0xA4);
        bridge.register_dispatched_block(block_hash, 400, Some(TEST_RR));
        bridge.process_receipt(&make_receipt(block_hash, 400));
    }

    #[test]
    fn test_untracked_block_receipt_dropped() {
        // Receipts for blocks not registered via register_dispatched_block must be
        // dropped without affecting attestation state or triggering a reward.
        let (_hub_tx, hub_rx) = make_hub_channel();
        let (reward_tx, mut reward_rx) = make_reward_channel();
        let mut bridge = MobileVerificationBridge::new(hub_rx, 1, 100).with_reward_tx(reward_tx);

        let block_hash = B256::with_last_byte(0xB1);
        // Deliberately NOT calling register_dispatched_block.
        bridge.process_receipt(&make_receipt(block_hash, 500));

        // Aggregator must not have a status entry for the block.
        assert!(
            bridge.receipt_aggregator.get_status(&block_hash).is_none(),
            "unregistered block must not appear in aggregator"
        );
        // No reward should be sent.
        assert!(
            reward_rx.try_recv().is_err(),
            "receipt for untracked block must not trigger reward"
        );
    }

    #[test]
    fn test_bridge_builds_aggregate_attestation() {
        use crate::attestation_store::AttestationStore;

        let store_path = std::env::temp_dir().join("n42-test-bridge-aggregate.json");
        let _ = std::fs::remove_file(&store_path);
        let store = Arc::new(Mutex::new(
            AttestationStore::new(store_path.clone()).unwrap(),
        ));

        let (_hub_tx, hub_rx) = make_hub_channel();
        let mut bridge =
            MobileVerificationBridge::new(hub_rx, 2, 100).with_attestation_store(store.clone());

        let block_hash = B256::with_last_byte(0xD1);
        let block_number = 42u64;

        // Create 3 verifiers with real BLS keys.
        let keys: Vec<_> = (0..3u64)
            .map(|i| {
                let mut seed = [0u8; 32];
                seed[0..8].copy_from_slice(&i.to_le_bytes());
                BlsSecretKey::key_gen(&seed).unwrap()
            })
            .collect();

        // Register verifiers in the store's registry (simulating PhoneConnected).
        {
            let mut s = store.lock().unwrap();
            for key in &keys {
                s.registry_mut().register(key.public_key().to_bytes());
            }
        }

        // Register block for tracking.
        bridge.register_dispatched_block(block_hash, block_number, Some(TEST_RR));

        // Send 2 valid receipts (threshold = 2).
        for key in &keys[..2] {
            let receipt = sign_receipt(block_hash, block_number, TEST_RR, 1_000_000, key);
            bridge.process_receipt(&receipt);
        }

        // Threshold should have been reached; aggregate should be built and stored.
        {
            let s = store.lock().unwrap();
            assert_eq!(
                s.total_attestations(),
                1,
                "one aggregate attestation should be stored"
            );

            let att = s
                .latest_attestation()
                .expect("should have latest attestation");
            assert_eq!(att.block_hash, block_hash);
            assert_eq!(att.block_number, block_number);
            assert_eq!(att.participant_count, 2);
            assert_eq!(att.receipts_root, TEST_RR);

            // Verify the aggregate signature.
            att.verify(s.registry())
                .expect("aggregate signature should verify against registry");

            // Check reward points.
            let pk0_hex = hex::encode(keys[0].public_key().to_bytes());
            let pk1_hex = hex::encode(keys[1].public_key().to_bytes());
            let pk2_hex = hex::encode(keys[2].public_key().to_bytes());
            assert_eq!(s.get_reward_points(&pk0_hex).unwrap().blocks_attested, 1);
            assert_eq!(s.get_reward_points(&pk1_hex).unwrap().blocks_attested, 1);
            assert!(
                s.get_reward_points(&pk2_hex).is_none(),
                "verifier 2 did not participate, should have no points"
            );
        }

        // The builder should have been consumed (removed from the map).
        assert!(
            !bridge.attestation_builders.contains_key(&block_hash),
            "builder should be removed after finalization"
        );

        let _ = std::fs::remove_file(&store_path);
    }

    #[test]
    fn test_bridge_no_aggregate_without_store() {
        let (_hub_tx, hub_rx) = make_hub_channel();
        let mut bridge = MobileVerificationBridge::new(hub_rx, 1, 100);
        // No attestation_store configured.

        let block_hash = B256::with_last_byte(0xD2);
        bridge.register_dispatched_block(block_hash, 1, Some(TEST_RR));

        // A builder is created even without a store, but no signatures will be
        // added because the builder.add_receipt needs a registry from the store.
        let receipt = make_receipt(block_hash, 1);
        bridge.process_receipt(&receipt);

        // Should still work (threshold reached), just no aggregate stored.
        assert!(
            bridge
                .receipt_aggregator
                .get_status(&block_hash)
                .unwrap()
                .is_attested()
        );
    }

    #[test]
    fn test_bridge_no_aggregate_without_expected_root() {
        use crate::attestation_store::AttestationStore;

        let store_path = std::env::temp_dir().join("n42-test-bridge-no-root.json");
        let _ = std::fs::remove_file(&store_path);
        let store = Arc::new(Mutex::new(
            AttestationStore::new(store_path.clone()).unwrap(),
        ));

        let (_hub_tx, hub_rx) = make_hub_channel();
        let mut bridge =
            MobileVerificationBridge::new(hub_rx, 1, 100).with_attestation_store(store.clone());

        let block_hash = B256::with_last_byte(0xD3);
        // Register without expected_receipts_root → no builder should be created.
        bridge.register_dispatched_block(block_hash, 1, None);

        assert!(
            !bridge.attestation_builders.contains_key(&block_hash),
            "no builder should exist when expected_receipts_root is None"
        );

        let _ = std::fs::remove_file(&store_path);
    }

    #[tokio::test]
    async fn test_bridge_registers_committed_blocks_from_consensus_state() {
        let (_hub_tx, hub_rx) = make_hub_channel();
        let state = make_consensus_state();
        let mut bridge =
            MobileVerificationBridge::new(hub_rx, 1, 100).with_consensus_state(state.clone());

        state.notify_block_committed(B256::with_last_byte(0xC1), 77);

        let run_once = tokio::time::timeout(std::time::Duration::from_millis(50), async {
            tokio::select! {
                task = async {
                    match bridge.verification_task_rx.as_mut() {
                        Some(rx) => rx.recv().await.ok(),
                        None => None,
                    }
                } => task,
            }
        })
        .await
        .expect("should receive committed-block notification");

        let task = run_once.expect("verification task should be present");
        bridge.register_dispatched_block(task.block_hash, task.block_number, None);

        let status = bridge.receipt_aggregator.get_status(&task.block_hash);
        assert!(
            status.is_some(),
            "committed block should be registered for receipt tracking"
        );
        assert_eq!(status.unwrap().block_number, 77);
    }
}
