use alloy_primitives::B256;
use n42_mobile::{ReceiptAggregator, VerificationReceipt};
use n42_network::mobile::HubEvent;
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
const MIN_ATTESTATION_THRESHOLD: u32 = 10;

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
    attestation_tx: Option<mpsc::UnboundedSender<AttestationEvent>>,
    /// Optional sender for notifying when a new phone connects.
    /// Used by `mobile_packet_loop` to trigger CacheSyncMessage broadcast.
    phone_connected_tx: Option<mpsc::UnboundedSender<()>>,
    /// Number of currently connected mobile verifiers.
    /// Used to dynamically adjust the attestation threshold.
    connected_count: u32,
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
            connected_count: 0,
        }
    }

    /// Sets the attestation notification channel.
    ///
    /// When a block reaches the attestation threshold, an `AttestationEvent`
    /// will be sent on this channel. The orchestrator can listen for these
    /// events to track mobile verification status.
    pub fn with_attestation_tx(
        mut self,
        tx: mpsc::UnboundedSender<AttestationEvent>,
    ) -> Self {
        self.attestation_tx = Some(tx);
        self
    }

    /// Sets the phone-connected notification channel.
    ///
    /// When a new phone connects, a `()` notification is sent on this channel.
    /// The `mobile_packet_loop` listens for these notifications to trigger
    /// CacheSyncMessage broadcasts (sending cached bytecodes to new phones).
    pub fn with_phone_connected_tx(
        mut self,
        tx: mpsc::UnboundedSender<()>,
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
                    self.connected_count += 1;
                    self.update_dynamic_threshold();
                    debug!(
                        target: "n42::mobile",
                        session_id,
                        connected = self.connected_count,
                        "mobile verifier connected"
                    );
                    // Notify mobile_packet_loop to send CacheSyncMessage.
                    if let Some(ref tx) = self.phone_connected_tx {
                        let _ = tx.send(());
                    }
                }
                HubEvent::PhoneDisconnected { session_id } => {
                    self.connected_count = self.connected_count.saturating_sub(1);
                    self.update_dynamic_threshold();
                    debug!(
                        target: "n42::mobile",
                        session_id,
                        connected = self.connected_count,
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
    /// Formula: `max(MIN_ATTESTATION_THRESHOLD, connected_count * 2 / 3)`
    /// This ensures BFT-grade security (2/3 threshold) when enough phones are connected,
    /// while maintaining a minimum of 10 during early deployment.
    fn update_dynamic_threshold(&mut self) {
        let dynamic = self.connected_count * 2 / 3;
        let threshold = dynamic.max(MIN_ATTESTATION_THRESHOLD);
        self.receipt_aggregator.set_default_threshold(threshold);
        debug!(
            target: "n42::mobile",
            connected = self.connected_count,
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
                    let _ = tx.send(AttestationEvent {
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
        let (attest_tx, mut attest_rx) = mpsc::unbounded_channel();
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
