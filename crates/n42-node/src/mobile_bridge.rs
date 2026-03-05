use alloy_primitives::B256;
use metrics::{counter, gauge};
use n42_mobile::{ReceiptAggregator, VerificationReceipt};
use n42_network::mobile::HubEvent;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info, warn};

use crate::consensus_state::{SharedConsensusState, VerificationTask};
use crate::staking::StakingManager;

/// Notification sent when a block reaches the mobile attestation threshold.
#[derive(Debug, Clone)]
pub struct AttestationEvent {
    pub block_hash: B256,
    pub block_number: u64,
    pub valid_count: u32,
}

fn min_attestation_threshold() -> u32 {
    std::env::var("N42_MIN_ATTESTATION_THRESHOLD")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(10)
}

/// Bridges StarHub mobile verification events with the ReceiptAggregator.
pub struct MobileVerificationBridge {
    hub_event_rx: mpsc::UnboundedReceiver<HubEvent>,
    receipt_aggregator: ReceiptAggregator,
    attestation_tx: Option<mpsc::Sender<AttestationEvent>>,
    phone_connected_tx: Option<mpsc::Sender<u64>>,
    reward_tx: Option<mpsc::UnboundedSender<[u8; 48]>>,
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
}

impl MobileVerificationBridge {
    pub fn new(
        hub_event_rx: mpsc::UnboundedReceiver<HubEvent>,
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

    pub fn with_reward_tx(mut self, tx: mpsc::UnboundedSender<[u8; 48]>) -> Self {
        self.reward_tx = Some(tx);
        self
    }

    /// Attaches a staking manager for verifier authorization checks.
    /// When set, only registered (staked or registered) verifiers are authorized.
    pub fn with_staking_manager(mut self, mgr: Arc<Mutex<StakingManager>>) -> Self {
        self.staking_manager = Some(mgr);
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
    pub fn register_dispatched_block(&mut self, block_hash: B256, block_number: u64) {
        self.receipt_aggregator.register_block(block_hash, block_number);
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
                            let registered = if let Some(ref staking_mgr) = self.staking_manager {
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
                            gauge!("n42_mobile_connected_phones").set(self.connected_sessions.len() as f64);
                            self.update_dynamic_threshold();
                            debug!(
                                target: "n42::mobile",
                                session_id,
                                connected = self.connected_sessions.len(),
                                "mobile verifier connected"
                            );
                            if let Some(ref tx) = self.phone_connected_tx {
                                let _ = tx.try_send(session_id);
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
                            self.process_receipt(&receipt);
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
                            self.register_dispatched_block(task.block_hash, task.view);
                            debug!(
                                target: "n42::mobile",
                                block_hash = %task.block_hash,
                                view = task.view,
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

    /// Public for testing; production code calls this via the event loop.
    ///
    /// Receipts for blocks not registered via `register_dispatched_block` are
    /// silently dropped (aggregator returns `None`). This prevents arbitrary
    /// block hashes from being injected by a malicious verifier.
    pub fn process_receipt(&mut self, receipt: &VerificationReceipt) {
        if let Err(e) = receipt.verify_signature() {
            warn!(
                target: "n42::mobile",
                block_number = receipt.block_number,
                error = %e,
                "receipt signature invalid, dropping"
            );
            return;
        }

        if !receipt.is_valid() {
            counter!("n42_mobile_invalid_receipts_total").increment(1);
            warn!(
                target: "n42::mobile",
                block_number = receipt.block_number,
                %receipt.block_hash,
                state_root_match = receipt.state_root_match,
                receipts_root_match = receipt.receipts_root_match,
                "mobile verifier reported INVALID block execution"
            );

            Self::fifo_ensure_entry(
                &mut self.invalid_receipt_counts,
                &mut self.invalid_receipt_order,
                receipt.block_hash,
                self.max_tracked,
            );
            let count = self.invalid_receipt_counts.entry(receipt.block_hash).or_insert(0);
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
            self.block_first_receipt_at.insert(receipt.block_hash, std::time::Instant::now());
        }

        match self.receipt_aggregator.process_receipt(receipt) {
            Some(true) => {
                // New unique receipt for a dispatched block; threshold just crossed.
                if receipt.is_valid()
                    && let Some(ref tx) = self.reward_tx
                {
                    let _ = tx.send(receipt.verifier_pubkey);
                }

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

                info!(
                    target: "n42::mobile",
                    block_number = receipt.block_number,
                    %receipt.block_hash,
                    valid_count,
                    "block reached attestation threshold"
                );

                if let Some(ref tx) = self.attestation_tx {
                    let _ = tx.try_send(AttestationEvent {
                        block_hash: receipt.block_hash,
                        block_number: receipt.block_number,
                        valid_count,
                    });
                }
            }
            Some(false) => {
                // New unique receipt for a dispatched block; threshold not yet reached.
                if receipt.is_valid()
                    && let Some(ref tx) = self.reward_tx
                {
                    let _ = tx.send(receipt.verifier_pubkey);
                }
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::B256;
    use n42_consensus::ValidatorSet;
    use n42_mobile::receipt::sign_receipt;
    use n42_primitives::BlsSecretKey;

    fn make_receipt(block_hash: B256, block_number: u64) -> VerificationReceipt {
        let key = BlsSecretKey::random().expect("BLS key gen");
        sign_receipt(block_hash, block_number, true, true, 1_000_000, &key)
    }

    fn make_consensus_state() -> Arc<SharedConsensusState> {
        Arc::new(SharedConsensusState::new(ValidatorSet::new(&[], 0)))
    }

    #[test]
    fn test_bridge_processes_receipts() {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut bridge = MobileVerificationBridge::new(rx, 2, 100);

        let block_hash = B256::with_last_byte(0x01);
        bridge.register_dispatched_block(block_hash, 1);
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
        let (tx, rx) = mpsc::unbounded_channel();
        let (attest_tx, mut attest_rx) = mpsc::channel(256);
        let mut bridge = MobileVerificationBridge::new(rx, 1, 100).with_attestation_tx(attest_tx);

        let block_hash = B256::with_last_byte(0x03);
        bridge.register_dispatched_block(block_hash, 10);
        bridge.process_receipt(&make_receipt(block_hash, 10));

        let event = attest_rx.try_recv().expect("attestation event should be sent");
        assert_eq!(event.block_hash, block_hash);
        assert_eq!(event.block_number, 10);
        assert_eq!(event.valid_count, 1);

        drop(tx);
    }

    #[test]
    fn test_bridge_multiple_blocks() {
        let (_tx, rx) = mpsc::unbounded_channel();
        let mut bridge = MobileVerificationBridge::new(rx, 2, 100);

        let hash_a = B256::with_last_byte(0x0A);
        let hash_b = B256::with_last_byte(0x0B);

        bridge.register_dispatched_block(hash_a, 10);
        bridge.register_dispatched_block(hash_b, 11);
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
        let (_tx, rx) = mpsc::unbounded_channel();
        let mut bridge = MobileVerificationBridge::new(rx, 2, 100);

        let block_hash = B256::with_last_byte(0x0C);
        bridge.register_dispatched_block(block_hash, 20);
        let receipt = make_receipt(block_hash, 20);
        bridge.process_receipt(&receipt);
        bridge.process_receipt(&receipt);

        let status = bridge.receipt_aggregator.get_status(&block_hash).unwrap();
        assert_eq!(status.total_receipts(), 1, "duplicate receipt from same verifier should be ignored");
    }

    #[test]
    fn test_bridge_no_attestation_tx() {
        let (_tx, rx) = mpsc::unbounded_channel();
        let mut bridge = MobileVerificationBridge::new(rx, 1, 100);

        let block_hash = B256::with_last_byte(0x0D);
        bridge.register_dispatched_block(block_hash, 30);
        bridge.process_receipt(&make_receipt(block_hash, 30));

        assert!(bridge.receipt_aggregator.get_status(&block_hash).unwrap().is_attested());
    }

    #[test]
    fn test_bridge_dynamic_threshold() {
        let (_tx, rx) = mpsc::unbounded_channel();
        let mut bridge = MobileVerificationBridge::new(rx, 2, 100);

        for i in 0..30u64 {
            bridge.connected_sessions.insert(i, [0u8; 48]);
        }
        bridge.update_dynamic_threshold();

        let block_hash = B256::with_last_byte(0xF1);
        bridge.register_dispatched_block(block_hash, 100);
        bridge.process_receipt(&make_receipt(block_hash, 100));
        assert!(
            !bridge.receipt_aggregator.get_status(&block_hash).unwrap().is_attested(),
            "with ~20 threshold, 1 receipt should not attest"
        );
    }

    #[test]
    fn test_bridge_invalid_receipt_counts_bounded() {
        let (_tx, rx) = mpsc::unbounded_channel();
        let max_tracked = 3;
        let mut bridge = MobileVerificationBridge::new(rx, 100, max_tracked);

        for i in 0..4u8 {
            let block_hash = B256::with_last_byte(i);
            let key = BlsSecretKey::random().expect("BLS key gen");
            let receipt = sign_receipt(block_hash, i as u64, false, true, 1_000_000, &key);
            bridge.process_receipt(&receipt);
        }

        assert_eq!(bridge.invalid_receipt_counts.len(), max_tracked);
        assert!(!bridge.invalid_receipt_counts.contains_key(&B256::with_last_byte(0)));
        assert!(bridge.invalid_receipt_counts.contains_key(&B256::with_last_byte(1)));
        assert!(bridge.invalid_receipt_counts.contains_key(&B256::with_last_byte(2)));
        assert!(bridge.invalid_receipt_counts.contains_key(&B256::with_last_byte(3)));
    }

    #[test]
    fn test_bridge_handles_all_event_types() {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut bridge = MobileVerificationBridge::new(rx, 2, 100);

        let block_hash = B256::with_last_byte(0x02);
        let receipt = make_receipt(block_hash, 42);

        tx.send(HubEvent::PhoneConnected { session_id: 1, verifier_pubkey: [0u8; 48] }).unwrap();
        tx.send(HubEvent::ReceiptReceived(Box::new(receipt))).unwrap();
        tx.send(HubEvent::CacheInventoryReceived { session_id: 1, code_hashes: vec![[0xAA; 32]] })
            .unwrap();
        tx.send(HubEvent::PhoneDisconnected { session_id: 1 }).unwrap();

        while let Ok(event) = bridge.hub_event_rx.try_recv() {
            if let HubEvent::ReceiptReceived(ref r) = event {
                bridge.process_receipt(r);
            }
        }
    }

    #[test]
    fn test_attestation_latency_recorded() {
        let (_tx, rx) = mpsc::unbounded_channel();
        let mut bridge = MobileVerificationBridge::new(rx, 2, 100);

        let block_hash = B256::with_last_byte(0xE1);
        bridge.register_dispatched_block(block_hash, 50);
        bridge.process_receipt(&make_receipt(block_hash, 50));
        assert!(bridge.block_first_receipt_at.contains_key(&block_hash));

        bridge.process_receipt(&make_receipt(block_hash, 50));
        assert!(bridge.receipt_aggregator.get_status(&block_hash).unwrap().is_attested());
        assert!(bridge.block_first_receipt_at.contains_key(&block_hash));
    }

    #[test]
    fn test_first_receipt_time_bounded() {
        let (_tx, rx) = mpsc::unbounded_channel();
        let max_tracked = 3;
        let mut bridge = MobileVerificationBridge::new(rx, 100, max_tracked);

        for i in 0..4u8 {
            bridge.process_receipt(&make_receipt(B256::with_last_byte(0xF0 + i), i as u64));
        }

        assert_eq!(bridge.block_first_receipt_at.len(), max_tracked);
        assert!(!bridge.block_first_receipt_at.contains_key(&B256::with_last_byte(0xF0)));
    }

    #[test]
    fn test_attestation_latency_no_panic_without_first_receipt() {
        let (_tx, rx) = mpsc::unbounded_channel();
        let mut bridge = MobileVerificationBridge::new(rx, 1, 100);

        let block_hash = B256::with_last_byte(0xE2);
        bridge.register_dispatched_block(block_hash, 60);
        bridge.process_receipt(&make_receipt(block_hash, 60));
        assert!(bridge.receipt_aggregator.get_status(&block_hash).unwrap().is_attested());
    }

    #[test]
    fn test_reward_tx_sends_pubkey_on_valid_receipt() {
        let (_hub_tx, hub_rx) = mpsc::unbounded_channel();
        let (reward_tx, mut reward_rx) = mpsc::unbounded_channel();
        let mut bridge = MobileVerificationBridge::new(hub_rx, 1, 100)
            .with_reward_tx(reward_tx);

        let block_hash = B256::with_last_byte(0xA1);
        let receipt = make_receipt(block_hash, 100);
        let expected_pubkey = receipt.verifier_pubkey;

        // Block must be registered (dispatched) before receipts are accepted.
        bridge.register_dispatched_block(block_hash, 100);
        bridge.process_receipt(&receipt);

        let received = reward_rx.try_recv().expect("reward_tx should receive pubkey");
        assert_eq!(received, expected_pubkey);
    }

    #[test]
    fn test_reward_tx_not_sent_on_invalid_receipt() {
        let (_hub_tx, hub_rx) = mpsc::unbounded_channel();
        let (reward_tx, mut reward_rx) = mpsc::unbounded_channel();
        let mut bridge = MobileVerificationBridge::new(hub_rx, 1, 100)
            .with_reward_tx(reward_tx);

        let block_hash = B256::with_last_byte(0xA2);
        let key = BlsSecretKey::random().expect("BLS key gen");
        // Create invalid receipt: state_root_match = false
        let invalid_receipt = sign_receipt(block_hash, 200, false, true, 1_000_000, &key);

        bridge.register_dispatched_block(block_hash, 200);
        bridge.process_receipt(&invalid_receipt);

        // Invalid receipt should NOT trigger reward
        assert!(reward_rx.try_recv().is_err(), "invalid receipt should not send to reward_tx");
    }

    #[test]
    fn test_reward_tx_multiple_valid_receipts() {
        let (_hub_tx, hub_rx) = mpsc::unbounded_channel();
        let (reward_tx, mut reward_rx) = mpsc::unbounded_channel();
        let mut bridge = MobileVerificationBridge::new(hub_rx, 10, 100)
            .with_reward_tx(reward_tx);

        let block_hash = B256::with_last_byte(0xA3);
        let receipt1 = make_receipt(block_hash, 300);
        let receipt2 = make_receipt(block_hash, 300);

        bridge.register_dispatched_block(block_hash, 300);
        bridge.process_receipt(&receipt1);
        bridge.process_receipt(&receipt2);

        // Both valid receipts should trigger reward_tx
        let pk1 = reward_rx.try_recv().expect("first reward");
        let pk2 = reward_rx.try_recv().expect("second reward");
        assert_ne!(pk1, pk2, "different BLS keys should produce different pubkeys");
    }

    #[test]
    fn test_no_reward_tx_configured_no_panic() {
        let (_hub_tx, hub_rx) = mpsc::unbounded_channel();
        let mut bridge = MobileVerificationBridge::new(hub_rx, 1, 100);
        // No reward_tx configured — should not panic
        let block_hash = B256::with_last_byte(0xA4);
        bridge.register_dispatched_block(block_hash, 400);
        bridge.process_receipt(&make_receipt(block_hash, 400));
    }

    #[test]
    fn test_untracked_block_receipt_dropped() {
        // Receipts for blocks not registered via register_dispatched_block must be
        // dropped without affecting attestation state or triggering a reward.
        let (_hub_tx, hub_rx) = mpsc::unbounded_channel();
        let (reward_tx, mut reward_rx) = mpsc::unbounded_channel();
        let mut bridge = MobileVerificationBridge::new(hub_rx, 1, 100)
            .with_reward_tx(reward_tx);

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

    #[tokio::test]
    async fn test_bridge_registers_committed_blocks_from_consensus_state() {
        let (_hub_tx, hub_rx) = mpsc::unbounded_channel();
        let state = make_consensus_state();
        let mut bridge = MobileVerificationBridge::new(hub_rx, 1, 100)
            .with_consensus_state(state.clone());

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
        }).await.expect("should receive committed-block notification");

        let task = run_once.expect("verification task should be present");
        bridge.register_dispatched_block(task.block_hash, task.view);

        let status = bridge.receipt_aggregator.get_status(&task.block_hash);
        assert!(status.is_some(), "committed block should be registered for receipt tracking");
        assert_eq!(status.unwrap().block_number, 77);
    }
}
