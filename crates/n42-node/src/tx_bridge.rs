use alloy_primitives::B256;
use reth_ethereum_primitives::TransactionSigned;
use reth_primitives_traits::SignedTransaction;
use reth_transaction_pool::{EthPooledTransaction, PoolTransaction, TransactionPool};
use std::collections::{HashSet, VecDeque};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Maximum number of recently-seen transaction hashes to track for dedup.
const RECENT_TX_CAPACITY: usize = 10_000;

/// Bridges the local transaction pool with the P2P network.
///
/// - **Outbound**: monitors the local pool for new transactions and sends them
///   to the orchestrator for broadcasting via GossipSub `/n42/mempool/1`.
/// - **Inbound**: receives transactions from the network and imports them into
///   the local pool as external transactions.
///
/// Uses a FIFO dedup ring buffer (VecDeque + HashSet) to prevent echo broadcasting.
pub struct TxPoolBridge<Pool> {
    pool: Pool,
    import_rx: mpsc::Receiver<Vec<u8>>,
    broadcast_tx: mpsc::Sender<Vec<u8>>,
    recently_imported_order: VecDeque<B256>,
    recently_imported_set: HashSet<B256>,
}

impl<Pool> TxPoolBridge<Pool>
where
    Pool: TransactionPool<Transaction = EthPooledTransaction> + Clone + 'static,
{
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

    pub async fn run(mut self) {
        info!("TxPoolBridge started");
        let mut pending_listener = self.pool.pending_transactions_listener();

        loop {
            tokio::select! {
                tx_hash = pending_listener.recv() => {
                    match tx_hash {
                        Some(tx_hash) => {
                            if self.is_recently_imported(&tx_hash) {
                                continue;
                            }
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

                data = self.import_rx.recv() => {
                    match data {
                        Some(tx_bytes) => self.handle_network_transaction(tx_bytes).await,
                        None => {
                            info!("import channel closed, shutting down TxPoolBridge");
                            break;
                        }
                    }
                }
            }
        }
    }

    async fn handle_network_transaction(&mut self, tx_bytes: Vec<u8>) {
        let tx: TransactionSigned =
            match alloy_rlp::Decodable::decode(&mut tx_bytes.as_slice()) {
                Ok(tx) => tx,
                Err(e) => {
                    warn!(error = %e, "failed to decode network transaction");
                    return;
                }
            };

        let tx_hash = *tx.tx_hash();

        if self.pool.contains(&tx_hash) {
            return;
        }

        self.mark_imported(tx_hash);

        let recovered = match tx.try_into_recovered() {
            Ok(r) => r,
            Err(_) => {
                debug!(%tx_hash, "failed to recover sender from network transaction");
                return;
            }
        };

        let pooled = match EthPooledTransaction::try_from_consensus(recovered) {
            Ok(p) => p,
            Err(e) => {
                debug!(error = %e, "failed to convert network transaction for pool");
                return;
            }
        };

        match self.pool.add_external_transaction(pooled).await {
            Ok(_) => debug!(%tx_hash, "imported network transaction into pool"),
            Err(e) => debug!(error = %e, "failed to import network transaction"),
        }
    }

    fn is_recently_imported(&self, hash: &B256) -> bool {
        self.recently_imported_set.contains(hash)
    }

    fn mark_imported(&mut self, hash: B256) {
        // Guard against double-insert to prevent VecDeque/HashSet desync on eviction.
        if self.recently_imported_set.contains(&hash) {
            return;
        }
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

    /// Standalone dedup tracker mirroring TxPoolBridge internals for unit testing.
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
            if self.set.contains(&hash) {
                return;
            }
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

        for i in 0..RECENT_TX_CAPACITY {
            let mut bytes = [0u8; 32];
            bytes[0..8].copy_from_slice(&(i as u64).to_le_bytes());
            tracker.mark_imported(B256::from(bytes));
        }
        assert_eq!(tracker.order.len(), RECENT_TX_CAPACITY);

        let first_hash = B256::from({
            let mut b = [0u8; 32];
            b[0..8].copy_from_slice(&0u64.to_le_bytes());
            b
        });
        assert!(tracker.is_recently_imported(&first_hash));

        let overflow = B256::repeat_byte(0xFF);
        tracker.mark_imported(overflow);
        assert!(!tracker.is_recently_imported(&first_hash), "first hash should be evicted");
        assert!(tracker.is_recently_imported(&overflow));
        assert_eq!(tracker.order.len(), RECENT_TX_CAPACITY);
        assert_eq!(tracker.set.len(), RECENT_TX_CAPACITY);
    }

    #[test]
    fn test_duplicate_insert_no_desync() {
        let mut tracker = DedupTracker::new();
        let h1 = B256::repeat_byte(0x01);

        tracker.mark_imported(h1);
        tracker.mark_imported(h1); // no-op
        assert_eq!(tracker.order.len(), 1);
        assert_eq!(tracker.set.len(), 1);
        assert!(tracker.is_recently_imported(&h1));
    }

    #[test]
    fn test_duplicate_survives_eviction_cycle() {
        // Regression: previously double-insert caused VecDeque/HashSet desync where
        // evicting the first copy removed the hash from the set prematurely.
        let mut tracker = DedupTracker::new();
        let target = B256::repeat_byte(0xAA);

        tracker.mark_imported(target);
        tracker.mark_imported(target); // no-op after fix

        for i in 1..RECENT_TX_CAPACITY {
            let mut bytes = [0u8; 32];
            bytes[0..8].copy_from_slice(&(i as u64).to_le_bytes());
            tracker.mark_imported(B256::from(bytes));
        }

        tracker.mark_imported(B256::repeat_byte(0xFF));
        assert!(!tracker.is_recently_imported(&target), "target should be evicted cleanly");
        assert_eq!(tracker.order.len(), RECENT_TX_CAPACITY);
        assert_eq!(tracker.set.len(), RECENT_TX_CAPACITY, "set and order must stay in sync");
    }

    #[test]
    fn test_eviction_fifo_order() {
        let mut tracker = DedupTracker::new();

        let h1 = B256::repeat_byte(0x01);
        let h2 = B256::repeat_byte(0x02);
        let h3 = B256::repeat_byte(0x03);

        for i in 0..RECENT_TX_CAPACITY - 3 {
            let mut bytes = [0u8; 32];
            bytes[0..8].copy_from_slice(&(i as u64 + 100).to_le_bytes());
            tracker.mark_imported(B256::from(bytes));
        }
        tracker.mark_imported(h1);
        tracker.mark_imported(h2);
        tracker.mark_imported(h3);

        // Overflow evicts the oldest filler entries, not h1/h2/h3.
        tracker.mark_imported(B256::repeat_byte(0xFF));
        assert!(tracker.is_recently_imported(&h1));
        assert!(tracker.is_recently_imported(&h2));
        assert!(tracker.is_recently_imported(&h3));
        assert!(tracker.is_recently_imported(&B256::repeat_byte(0xFF)));
    }
}
