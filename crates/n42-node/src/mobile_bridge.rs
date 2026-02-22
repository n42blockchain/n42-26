use alloy_primitives::B256;
use metrics::{counter, gauge};
use n42_mobile::{ReceiptAggregator, VerificationReceipt};
use n42_network::mobile::HubEvent;
use std::collections::{HashMap, HashSet, VecDeque};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Notification sent when a block reaches the mobile attestation threshold.
#[derive(Debug, Clone)]
pub struct AttestationEvent {
    pub block_hash: B256,
    pub block_number: u64,
    pub valid_count: u32,
}

/// Minimum attestation threshold regardless of connected phone count.
/// Configurable via `N42_MIN_ATTESTATION_THRESHOLD` environment variable.
fn min_attestation_threshold() -> u32 {
    std::env::var("N42_MIN_ATTESTATION_THRESHOLD")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(10)
}

/// Bridges StarHub mobile verification events with the ReceiptAggregator.
///
/// Processes HubEvents and aggregates verification receipts to determine block
/// attestation status. When a block reaches the threshold, an `AttestationEvent`
/// is sent on the optional notification channel.
pub struct MobileVerificationBridge {
    hub_event_rx: mpsc::UnboundedReceiver<HubEvent>,
    receipt_aggregator: ReceiptAggregator,
    attestation_tx: Option<mpsc::Sender<AttestationEvent>>,
    /// Sends session_id when a new phone connects so CacheSync can be targeted.
    phone_connected_tx: Option<mpsc::Sender<u64>>,
    /// Tracks connected session IDs to prevent count drift from duplicate events.
    connected_sessions: HashSet<u64>,
    /// Per-block invalid receipt counts for divergence detection (FIFO-bounded).
    invalid_receipt_counts: HashMap<B256, u32>,
    invalid_receipt_order: VecDeque<B256>,
    /// Per-block first-receipt timestamps for attestation latency measurement (FIFO-bounded).
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
        Self {
            hub_event_rx,
            receipt_aggregator: ReceiptAggregator::new(default_threshold, max_tracked_blocks),
            attestation_tx: None,
            phone_connected_tx: None,
            connected_sessions: HashSet::new(),
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

    /// Runs the bridge event loop until the channel closes.
    pub async fn run(mut self) {
        info!(target: "n42::mobile", "mobile verification bridge started");

        while let Some(event) = self.hub_event_rx.recv().await {
            match event {
                HubEvent::PhoneConnected { session_id, .. } => {
                    self.connected_sessions.insert(session_id);
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
                    if !self.connected_sessions.remove(&session_id) {
                        warn!(
                            target: "n42::mobile",
                            session_id,
                            "disconnect for unknown session (duplicate event?)"
                        );
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

        info!(target: "n42::mobile", "mobile verification bridge shutting down");
    }

    /// Recalculates the attestation threshold: `max(min_threshold, connected * 2 / 3)`.
    fn update_dynamic_threshold(&mut self) {
        let connected = self.connected_sessions.len() as u32;
        let threshold = (connected * 2 / 3).max(min_attestation_threshold());
        self.receipt_aggregator.set_default_threshold(threshold);
        debug!(target: "n42::mobile", connected, threshold, "attestation threshold updated");
    }

    /// Processes a verification receipt through the aggregator.
    ///
    /// Public for testing; production code calls this via the event loop.
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

        if receipt.is_valid() {
            counter!("n42_mobile_valid_receipts_total").increment(1);
        } else {
            counter!("n42_mobile_invalid_receipts_total").increment(1);
            warn!(
                target: "n42::mobile",
                block_number = receipt.block_number,
                %receipt.block_hash,
                state_root_match = receipt.state_root_match,
                receipts_root_match = receipt.receipts_root_match,
                "mobile verifier reported INVALID block execution"
            );

            // Track invalid count per block for divergence detection (FIFO-evict at capacity).
            if !self.invalid_receipt_counts.contains_key(&receipt.block_hash) {
                if self.invalid_receipt_counts.len() >= self.max_tracked {
                    if let Some(oldest) = self.invalid_receipt_order.pop_front() {
                        self.invalid_receipt_counts.remove(&oldest);
                    }
                }
                self.invalid_receipt_order.push_back(receipt.block_hash);
            }
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
        }

        self.receipt_aggregator.register_block(receipt.block_hash, receipt.block_number);

        // Track first-receipt time for attestation latency (FIFO-evict at capacity).
        if !self.block_first_receipt_at.contains_key(&receipt.block_hash) {
            if self.block_first_receipt_at.len() >= self.max_tracked {
                if let Some(oldest) = self.block_first_receipt_order.pop_front() {
                    self.block_first_receipt_at.remove(&oldest);
                }
            }
            self.block_first_receipt_at.insert(receipt.block_hash, std::time::Instant::now());
            self.block_first_receipt_order.push_back(receipt.block_hash);
        }

        match self.receipt_aggregator.process_receipt(receipt) {
            Some(true) => {
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
                debug!(
                    target: "n42::mobile",
                    block_number = receipt.block_number,
                    "receipt processed, threshold not yet reached"
                );
            }
            None => {
                warn!(
                    target: "n42::mobile",
                    block_number = receipt.block_number,
                    "receipt for untracked block"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::B256;
    use n42_mobile::receipt::sign_receipt;
    use n42_primitives::BlsSecretKey;

    fn make_receipt(block_hash: B256, block_number: u64) -> VerificationReceipt {
        let key = BlsSecretKey::random().expect("BLS key gen");
        sign_receipt(block_hash, block_number, true, true, 1_000_000, &key)
    }

    #[test]
    fn test_bridge_processes_receipts() {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut bridge = MobileVerificationBridge::new(rx, 2, 100);

        let block_hash = B256::with_last_byte(0x01);
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
        bridge.process_receipt(&make_receipt(block_hash, 30));

        assert!(bridge.receipt_aggregator.get_status(&block_hash).unwrap().is_attested());
    }

    #[test]
    fn test_bridge_dynamic_threshold() {
        let (_tx, rx) = mpsc::unbounded_channel();
        let mut bridge = MobileVerificationBridge::new(rx, 2, 100);

        for i in 0..30u64 {
            bridge.connected_sessions.insert(i);
        }
        bridge.update_dynamic_threshold();

        let block_hash = B256::with_last_byte(0xF1);
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
        tx.send(HubEvent::ReceiptReceived(receipt)).unwrap();
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
        bridge.process_receipt(&make_receipt(block_hash, 60));
        assert!(bridge.receipt_aggregator.get_status(&block_hash).unwrap().is_attested());
    }
}
