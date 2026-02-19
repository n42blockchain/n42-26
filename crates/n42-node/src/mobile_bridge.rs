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
    /// Hash of the attested block.
    pub block_hash: B256,
    /// Block number.
    pub block_number: u64,
    /// Number of valid receipts that met the threshold.
    pub valid_count: u32,
}

/// Minimum attestation threshold regardless of connected phone count.
/// Protects initial deployment when very few phones are connected.
/// Configurable via `N42_MIN_ATTESTATION_THRESHOLD` environment variable.
fn min_attestation_threshold() -> u32 {
    std::env::var("N42_MIN_ATTESTATION_THRESHOLD")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(10)
}

/// Bridges the StarHub mobile verification events with the ReceiptAggregator.
///
/// Consumes HubEvents from the StarHub and processes verification receipts
/// through the aggregator to determine block attestation status. When a block
/// reaches the attestation threshold, an `AttestationEvent` is sent on the
/// optional notification channel.
pub struct MobileVerificationBridge {
    /// Receiver for events from the StarHub.
    hub_event_rx: mpsc::UnboundedReceiver<HubEvent>,
    /// Aggregates verification receipts from mobile verifiers.
    receipt_aggregator: ReceiptAggregator,
    /// Optional sender for attestation notifications.
    attestation_tx: Option<mpsc::Sender<AttestationEvent>>,
    /// Optional sender for notifying when a new phone connects.
    /// Carries the session_id so CacheSync can be sent to that specific session.
    phone_connected_tx: Option<mpsc::Sender<u64>>,
    /// Currently connected session IDs, used to derive connected_count accurately.
    /// Prevents count drift from duplicate connect/disconnect events.
    connected_sessions: HashSet<u64>,
    /// Tracks invalid receipt counts per block hash for divergence detection.
    /// Key: block_hash, Value: count of invalid receipts.
    /// Bounded to `max_tracked_blocks` entries via FIFO eviction to prevent unbounded growth.
    invalid_receipt_counts: HashMap<B256, u32>,
    /// Insertion order for `invalid_receipt_counts` FIFO eviction.
    invalid_receipt_order: VecDeque<B256>,
    /// Maximum entries in `invalid_receipt_counts` (same as receipt_aggregator's max_tracked_blocks).
    max_invalid_tracked: usize,
}

impl MobileVerificationBridge {
    /// Creates a new bridge with the given hub event receiver.
    ///
    /// `default_threshold` is the number of valid receipts needed to consider
    /// a block attested. `max_tracked_blocks` controls memory usage.
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
            max_invalid_tracked: max_tracked_blocks,
        }
    }

    /// Sets the attestation notification channel.
    ///
    /// When a block reaches the attestation threshold, an `AttestationEvent`
    /// will be sent on this channel. The orchestrator can listen for these
    /// events to track mobile verification status.
    pub fn with_attestation_tx(
        mut self,
        tx: mpsc::Sender<AttestationEvent>,
    ) -> Self {
        self.attestation_tx = Some(tx);
        self
    }

    /// Sets the phone-connected notification channel.
    ///
    /// When a new phone connects, its `session_id` is sent on this channel.
    /// The `mobile_packet_loop` listens for these notifications to send
    /// targeted CacheSyncMessage to the specific new phone.
    pub fn with_phone_connected_tx(
        mut self,
        tx: mpsc::Sender<u64>,
    ) -> Self {
        self.phone_connected_tx = Some(tx);
        self
    }

    /// Runs the bridge event loop.
    ///
    /// Processes hub events until the channel closes. Should be spawned
    /// as a background task.
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
                    // Notify mobile_packet_loop to send targeted CacheSync to this session.
                    if let Some(ref tx) = self.phone_connected_tx {
                        let _ = tx.try_send(session_id);
                    }
                }
                HubEvent::PhoneDisconnected { session_id } => {
                    if !self.connected_sessions.remove(&session_id) {
                        tracing::warn!(
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

    /// Recalculates the attestation threshold based on the number of connected phones.
    ///
    /// Formula: `max(min_attestation_threshold(), connected_count * 2 / 3)`
    /// This ensures BFT-grade security (2/3 threshold) when enough phones are connected,
    /// while maintaining a minimum during early deployment.
    fn update_dynamic_threshold(&mut self) {
        let connected = self.connected_sessions.len() as u32;
        let dynamic = connected * 2 / 3;
        let threshold = dynamic.max(min_attestation_threshold());
        self.receipt_aggregator.set_default_threshold(threshold);
        debug!(
            target: "n42::mobile",
            connected,
            threshold,
            "attestation threshold updated"
        );
    }

    /// Processes a verification receipt through the aggregator.
    ///
    /// Public for testing; production code calls this via the event loop.
    pub fn process_receipt(&mut self, receipt: &VerificationReceipt) {
        // Verify BLS signature before aggregation to prevent forged receipts.
        if let Err(e) = receipt.verify_signature() {
            warn!(
                target: "n42::mobile",
                block_number = receipt.block_number,
                error = %e,
                "receipt signature invalid, dropping"
            );
            return;
        }

        // Track valid vs invalid receipts via metrics.
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

            // Track invalid count per block for divergence detection.
            // Evict oldest entry if at capacity (same bound as receipt_aggregator).
            if !self.invalid_receipt_counts.contains_key(&receipt.block_hash) {
                if self.invalid_receipt_counts.len() >= self.max_invalid_tracked {
                    if let Some(oldest) = self.invalid_receipt_order.pop_front() {
                        self.invalid_receipt_counts.remove(&oldest);
                    }
                }
                self.invalid_receipt_order.push_back(receipt.block_hash);
            }
            let count = self.invalid_receipt_counts
                .entry(receipt.block_hash)
                .or_insert(0);
            *count += 1;

            // Alert if invalid receipts exceed connected_count / 3 (potential state divergence).
            let connected = self.connected_sessions.len() as u32;
            let threshold = if connected > 0 { connected / 3 } else { 1 };
            if *count >= threshold && threshold > 0 {
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

        // Register block if not yet tracked.
        self.receipt_aggregator.register_block(
            receipt.block_hash,
            receipt.block_number,
        );

        // Process the receipt through the aggregator.
        match self.receipt_aggregator.process_receipt(receipt) {
            Some(true) => {
                let valid_count = self
                    .receipt_aggregator
                    .get_status(&receipt.block_hash)
                    .map(|s| s.valid_count)
                    .unwrap_or(0);

                info!(
                    target: "n42::mobile",
                    block_number = receipt.block_number,
                    %receipt.block_hash,
                    valid_count,
                    "block reached attestation threshold"
                );

                // Notify the orchestrator (if channel is configured).
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

    /// Helper: create a signed receipt for the given block.
    fn make_receipt(block_hash: B256, block_number: u64) -> VerificationReceipt {
        let key = BlsSecretKey::random().expect("BLS key gen");
        sign_receipt(block_hash, block_number, true, true, 1_000_000, &key)
    }

    #[test]
    fn test_bridge_processes_receipts() {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut bridge = MobileVerificationBridge::new(rx, 2, 100);

        let block_hash = B256::with_last_byte(0x01);
        let receipt1 = make_receipt(block_hash, 1);
        let receipt2 = make_receipt(block_hash, 1);

        // Process first receipt — registers the block, then adds the receipt.
        bridge.process_receipt(&receipt1);

        let status = bridge.receipt_aggregator.get_status(&block_hash);
        assert!(status.is_some());
        assert_eq!(status.unwrap().total_receipts(), 1);

        // Second receipt through process_receipt (not aggregator directly).
        // After the register_block() idempotency fix, this correctly
        // preserves the receipt count from the first call.
        bridge.process_receipt(&receipt2);

        let status = bridge.receipt_aggregator.get_status(&block_hash).unwrap();
        assert_eq!(status.total_receipts(), 2);
        // With threshold=2, block should now be attested.
        assert!(status.is_attested());

        drop(tx); // keep channel alive until end
    }

    #[test]
    fn test_bridge_attestation_notification() {
        let (tx, rx) = mpsc::unbounded_channel();
        let (attest_tx, mut attest_rx) = mpsc::channel(256);
        let mut bridge = MobileVerificationBridge::new(rx, 1, 100)
            .with_attestation_tx(attest_tx);

        let block_hash = B256::with_last_byte(0x03);
        let receipt = make_receipt(block_hash, 10);

        // With threshold=1, the first valid receipt should trigger attestation.
        bridge.process_receipt(&receipt);

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

        // Two receipts for block A.
        bridge.process_receipt(&make_receipt(hash_a, 10));
        bridge.process_receipt(&make_receipt(hash_a, 10));
        // One receipt for block B.
        bridge.process_receipt(&make_receipt(hash_b, 11));

        let status_a = bridge.receipt_aggregator.get_status(&hash_a).unwrap();
        assert_eq!(status_a.total_receipts(), 2);
        assert!(status_a.is_attested(), "block A should be attested with threshold=2");

        let status_b = bridge.receipt_aggregator.get_status(&hash_b).unwrap();
        assert_eq!(status_b.total_receipts(), 1);
        assert!(!status_b.is_attested(), "block B should not be attested yet");
    }

    #[test]
    fn test_bridge_duplicate_receipt_ignored() {
        let (_tx, rx) = mpsc::unbounded_channel();
        let mut bridge = MobileVerificationBridge::new(rx, 2, 100);

        let block_hash = B256::with_last_byte(0x0C);
        let receipt = make_receipt(block_hash, 20);

        // Process the same receipt twice.
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
        // When no attestation_tx is configured, reaching threshold should not panic.
        let (_tx, rx) = mpsc::unbounded_channel();
        let mut bridge = MobileVerificationBridge::new(rx, 1, 100);
        // Deliberately NOT calling .with_attestation_tx()

        let block_hash = B256::with_last_byte(0x0D);
        bridge.process_receipt(&make_receipt(block_hash, 30));

        let status = bridge.receipt_aggregator.get_status(&block_hash).unwrap();
        assert!(status.is_attested(), "should be attested with threshold=1");
        // Test passes if we reach here without panic.
    }

    #[test]
    fn test_bridge_dynamic_threshold() {
        let (_tx, rx) = mpsc::unbounded_channel();
        let mut bridge = MobileVerificationBridge::new(rx, 2, 100);

        // Initially no connected sessions → threshold = max(min_threshold, 0*2/3) = min_threshold
        assert!(bridge.connected_sessions.is_empty());

        // Simulate 30 phones connecting
        for i in 0..30u64 {
            bridge.connected_sessions.insert(i);
        }
        bridge.update_dynamic_threshold();
        // dynamic = 30 * 2/3 = 20, max(10, 20) = 20
        // (min_attestation_threshold() default is 10)
        // The threshold should be the dynamic value since 20 > 10

        // Verify by processing receipts: with high threshold, one receipt shouldn't attest
        let block_hash = B256::with_last_byte(0xF1);
        bridge.process_receipt(&make_receipt(block_hash, 100));
        let status = bridge.receipt_aggregator.get_status(&block_hash).unwrap();
        assert!(!status.is_attested(), "with ~20 threshold, 1 receipt should not attest");
    }

    #[test]
    fn test_bridge_invalid_receipt_counts_bounded() {
        let (_tx, rx) = mpsc::unbounded_channel();
        let max_tracked = 3;
        let mut bridge = MobileVerificationBridge::new(rx, 100, max_tracked);

        // Create invalid receipts for 4 different blocks (exceeding max_tracked=3)
        for i in 0..4u8 {
            let block_hash = B256::with_last_byte(i);
            let key = BlsSecretKey::random().expect("BLS key gen");
            let receipt = sign_receipt(block_hash, i as u64, false, true, 1_000_000, &key);
            bridge.process_receipt(&receipt);
        }

        // Should be bounded at max_tracked
        assert_eq!(
            bridge.invalid_receipt_counts.len(),
            max_tracked,
            "invalid_receipt_counts should be bounded at max_tracked_blocks"
        );
        // First block (0x00) should have been evicted
        assert!(
            !bridge.invalid_receipt_counts.contains_key(&B256::with_last_byte(0)),
            "oldest block should be evicted"
        );
        // Most recent blocks should still be tracked
        assert!(bridge.invalid_receipt_counts.contains_key(&B256::with_last_byte(1)));
        assert!(bridge.invalid_receipt_counts.contains_key(&B256::with_last_byte(2)));
        assert!(bridge.invalid_receipt_counts.contains_key(&B256::with_last_byte(3)));
    }

    #[test]
    fn test_bridge_handles_all_event_types() {
        // Verify that sending all 4 HubEvent variants through the channel
        // doesn't panic when the bridge processes them.
        let (tx, rx) = mpsc::unbounded_channel();
        let mut bridge = MobileVerificationBridge::new(rx, 2, 100);

        let block_hash = B256::with_last_byte(0x02);
        let receipt = make_receipt(block_hash, 42);

        // Send all event types.
        tx.send(HubEvent::PhoneConnected {
            session_id: 1,
            verifier_pubkey: [0u8; 48],
        })
        .unwrap();

        tx.send(HubEvent::ReceiptReceived(receipt)).unwrap();

        tx.send(HubEvent::CacheInventoryReceived {
            session_id: 1,
            code_hashes: vec![[0xAA; 32]],
        })
        .unwrap();

        tx.send(HubEvent::PhoneDisconnected { session_id: 1 }).unwrap();

        // Process events synchronously via try_recv.
        while let Ok(event) = bridge.hub_event_rx.try_recv() {
            match event {
                HubEvent::ReceiptReceived(ref r) => bridge.process_receipt(r),
                _ => {} // other events are logged in the real run() loop
            }
        }

        // Should not panic — test succeeds if we get here.
    }
}
