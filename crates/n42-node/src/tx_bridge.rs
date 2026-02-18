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
