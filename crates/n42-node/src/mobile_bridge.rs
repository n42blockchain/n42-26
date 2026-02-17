use n42_mobile::{ReceiptAggregator, VerificationReceipt};
use n42_network::mobile::HubEvent;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Bridges the StarHub mobile verification events with the ReceiptAggregator.
///
/// Consumes HubEvents from the StarHub and processes verification receipts
/// through the aggregator to determine block attestation status.
pub struct MobileVerificationBridge {
    /// Receiver for events from the StarHub.
    hub_event_rx: mpsc::UnboundedReceiver<HubEvent>,
    /// Aggregates verification receipts from mobile verifiers.
    receipt_aggregator: ReceiptAggregator,
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
        }
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
                    debug!(
                        target: "n42::mobile",
                        session_id,
                        "mobile verifier connected"
                    );
                }
                HubEvent::PhoneDisconnected { session_id } => {
                    debug!(
                        target: "n42::mobile",
                        session_id,
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

    fn process_receipt(&mut self, receipt: &VerificationReceipt) {
        // Register block if not yet tracked.
        self.receipt_aggregator.register_block(
            receipt.block_hash,
            receipt.block_number,
        );

        // Process the receipt through the aggregator.
        match self.receipt_aggregator.process_receipt(receipt) {
            Some(true) => {
                info!(
                    target: "n42::mobile",
                    block_number = receipt.block_number,
                    %receipt.block_hash,
                    "block reached attestation threshold"
                );
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
