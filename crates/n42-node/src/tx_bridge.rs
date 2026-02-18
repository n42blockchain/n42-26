use alloy_primitives::B256;
use reth_ethereum_primitives::TransactionSigned;
use reth_primitives_traits::SignedTransaction;
use reth_transaction_pool::{TransactionPool, PoolTransaction, EthPooledTransaction};
use std::collections::{HashSet, VecDeque};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Maximum number of recently-seen transaction hashes to track for dedup.
const RECENT_TX_CAPACITY: usize = 10_000;

/// Bridges the local transaction pool with the P2P network.
///
/// Two responsibilities:
/// 1. **Outbound**: Monitors the local pool for new transactions and sends them
///    to the orchestrator for broadcasting via GossipSub `/n42/mempool/1`.
/// 2. **Inbound**: Receives transactions from the network (via orchestrator)
///    and imports them into the local pool as external transactions.
///
/// Uses a simple LRU-style dedup ring buffer to prevent echo broadcasting
/// (re-broadcasting a transaction we just received from the network).
pub struct TxPoolBridge<Pool> {
    /// The local transaction pool.
    pool: Pool,
    /// Receives raw transaction bytes from the network (via orchestrator).
    import_rx: mpsc::Receiver<Vec<u8>>,
    /// Sends raw transaction bytes to the orchestrator for broadcasting.
    broadcast_tx: mpsc::Sender<Vec<u8>>,
    /// Recently imported transaction hashes — VecDeque maintains insertion order
    /// for FIFO eviction, HashSet provides O(1) lookups (replaces O(n) VecDeque::contains).
    recently_imported_order: VecDeque<B256>,
    recently_imported_set: HashSet<B256>,
}

impl<Pool> TxPoolBridge<Pool>
where
    Pool: TransactionPool<Transaction = EthPooledTransaction> + Clone + 'static,
{
    /// Creates a new TxPoolBridge.
    pub fn new(
        pool: Pool,
        import_rx: mpsc::Receiver<Vec<u8>>,
        broadcast_tx: mpsc::Sender<Vec<u8>>,
    ) -> Self {
        Self {
            pool,
            import_rx,
            broadcast_tx,
            recently_imported_order: VecDeque::with_capacity(RECENT_TX_CAPACITY),
            recently_imported_set: HashSet::with_capacity(RECENT_TX_CAPACITY),
        }
    }

    /// Runs the bridge as a background task.
    pub async fn run(mut self) {
        info!("TxPoolBridge started");

        // Listen for new pending transactions from the local pool.
        // Returns Receiver<TxHash> where TxHash = B256.
        let mut pending_listener = self.pool.pending_transactions_listener();

        loop {
            tokio::select! {
                // Outbound: local pool has a new pending transaction -> broadcast
                tx_hash = pending_listener.recv() => {
                    match tx_hash {
                        Some(tx_hash) => {
                            // Skip if this transaction was recently imported from network
                            // (prevents echo broadcasting).
                            if self.is_recently_imported(&tx_hash) {
                                continue;
                            }

                            // Get the full transaction from the pool and encode it.
                            if let Some(pooled_tx) = self.pool.get(&tx_hash) {
                                let tx: &TransactionSigned = pooled_tx.transaction.transaction();
                                let encoded = alloy_rlp::encode(tx);
                                debug!(%tx_hash, bytes = encoded.len(), "broadcasting local transaction");
                                let _ = self.broadcast_tx.try_send(encoded);
                            }
                        }
                        None => {
                            info!("pending transaction listener closed, shutting down TxPoolBridge");
                            break;
                        }
                    }
                }

                // Inbound: received transaction from network -> import to local pool
                data = self.import_rx.recv() => {
                    match data {
                        Some(tx_bytes) => {
                            self.handle_network_transaction(tx_bytes).await;
                        }
                        None => {
                            info!("import channel closed, shutting down TxPoolBridge");
                            break;
                        }
                    }
                }
            }
        }
    }

    /// Handles a transaction received from the network.
    async fn handle_network_transaction(&mut self, tx_bytes: Vec<u8>) {
        // Decode the transaction from RLP.
        let tx: TransactionSigned = match alloy_rlp::Decodable::decode(&mut tx_bytes.as_slice()) {
            Ok(tx) => tx,
            Err(e) => {
                warn!(error = %e, "failed to decode network transaction");
                return;
            }
        };

        let tx_hash = *tx.tx_hash();

        // Check if we already have this transaction.
        if self.pool.contains(&tx_hash) {
            return;
        }

        // Track as recently imported to prevent echo broadcasting.
        self.mark_imported(tx_hash);

        // Recover sender and convert to pool transaction type.
        let recovered = match tx.try_into_recovered() {
            Ok(r) => r,
            Err(_) => {
                debug!(%tx_hash, "failed to recover sender from network transaction");
                return;
            }
        };

        let pooled = match EthPooledTransaction::try_from_consensus(recovered) {
            Ok(pooled) => pooled,
            Err(e) => {
                debug!(error = %e, "failed to convert network transaction for pool");
                return;
            }
        };

        match self.pool.add_external_transaction(pooled).await {
            Ok(outcome) => {
                debug!(%tx_hash, "imported network transaction into pool");
                let _ = outcome;
            }
            Err(e) => {
                debug!(error = %e, "failed to import network transaction");
            }
        }
    }

    /// Checks if a transaction hash was recently imported from the network.
    /// O(1) via HashSet lookup (previously O(n) via VecDeque::contains).
    fn is_recently_imported(&self, hash: &B256) -> bool {
        self.recently_imported_set.contains(hash)
    }

    /// Marks a transaction hash as recently imported.
    fn mark_imported(&mut self, hash: B256) {
        if self.recently_imported_order.len() >= RECENT_TX_CAPACITY {
            if let Some(evicted) = self.recently_imported_order.pop_front() {
                self.recently_imported_set.remove(&evicted);
            }
        }
        self.recently_imported_order.push_back(hash);
        self.recently_imported_set.insert(hash);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Standalone dedup tracker that mirrors TxPoolBridge's internal logic,
    /// allowing us to test the dedup mechanism without a full TransactionPool.
    struct DedupTracker {
        order: VecDeque<B256>,
        set: HashSet<B256>,
    }

    impl DedupTracker {
        fn new() -> Self {
            Self {
                order: VecDeque::with_capacity(RECENT_TX_CAPACITY),
                set: HashSet::with_capacity(RECENT_TX_CAPACITY),
            }
        }

        fn is_recently_imported(&self, hash: &B256) -> bool {
            self.set.contains(hash)
        }

        fn mark_imported(&mut self, hash: B256) {
            if self.order.len() >= RECENT_TX_CAPACITY {
                if let Some(evicted) = self.order.pop_front() {
                    self.set.remove(&evicted);
                }
            }
            self.order.push_back(hash);
            self.set.insert(hash);
        }
    }

    #[test]
    fn test_mark_imported_basic() {
        let mut tracker = DedupTracker::new();
        let h1 = B256::repeat_byte(0x01);
        let h2 = B256::repeat_byte(0x02);

        assert!(!tracker.is_recently_imported(&h1));
        tracker.mark_imported(h1);
        assert!(tracker.is_recently_imported(&h1));
        assert!(!tracker.is_recently_imported(&h2));
    }

    #[test]
    fn test_mark_imported_eviction() {
        let mut tracker = DedupTracker::new();

        // Fill to capacity
        for i in 0..RECENT_TX_CAPACITY {
            let mut bytes = [0u8; 32];
            bytes[0..8].copy_from_slice(&(i as u64).to_le_bytes());
            tracker.mark_imported(B256::from(bytes));
        }
        assert_eq!(tracker.order.len(), RECENT_TX_CAPACITY);
        assert_eq!(tracker.set.len(), RECENT_TX_CAPACITY);

        // The first hash should still be present
        let first_hash = B256::from({
            let mut b = [0u8; 32];
            b[0..8].copy_from_slice(&0u64.to_le_bytes());
            b
        });
        assert!(tracker.is_recently_imported(&first_hash));

        // Add one more → should evict the first
        let overflow = B256::repeat_byte(0xFF);
        tracker.mark_imported(overflow);
        assert!(!tracker.is_recently_imported(&first_hash), "first hash should be evicted");
        assert!(tracker.is_recently_imported(&overflow));
        assert_eq!(tracker.order.len(), RECENT_TX_CAPACITY);
        assert_eq!(tracker.set.len(), RECENT_TX_CAPACITY);
    }

    #[test]
    fn test_dedup_set_consistency() {
        let mut tracker = DedupTracker::new();
        let h1 = B256::repeat_byte(0x01);

        // Double insert: VecDeque gets 2 entries, HashSet stays at 1
        tracker.mark_imported(h1);
        tracker.mark_imported(h1);
        assert!(tracker.is_recently_imported(&h1));
    }

    #[test]
    fn test_eviction_fifo_order() {
        let mut tracker = DedupTracker::new();

        // Insert 3 hashes
        let h1 = B256::repeat_byte(0x01);
        let h2 = B256::repeat_byte(0x02);
        let h3 = B256::repeat_byte(0x03);

        // Fill to capacity first
        for i in 0..RECENT_TX_CAPACITY - 3 {
            let mut bytes = [0u8; 32];
            bytes[0..8].copy_from_slice(&(i as u64 + 100).to_le_bytes());
            tracker.mark_imported(B256::from(bytes));
        }
        tracker.mark_imported(h1);
        tracker.mark_imported(h2);
        tracker.mark_imported(h3);
        assert_eq!(tracker.order.len(), RECENT_TX_CAPACITY);

        // Overflow: the oldest filler entries get evicted first, not h1/h2/h3
        let overflow = B256::repeat_byte(0xFF);
        tracker.mark_imported(overflow);
        assert!(tracker.is_recently_imported(&h1));
        assert!(tracker.is_recently_imported(&h2));
        assert!(tracker.is_recently_imported(&h3));
        assert!(tracker.is_recently_imported(&overflow));
    }
}
