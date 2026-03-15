//! Binary TCP ingest server for high-speed transaction submission.
//!
//! Accepts EIP-2718 encoded transactions with pre-recovered sender over TCP,
//! avoiding both JSON-RPC overhead and repeated ECDSA sender recovery.
//! Transactions are placed into the pool as pre-validated, using a trusted
//! local benchmark/test harness path.
//!
//! **SECURITY WARNING**: The sender address is trusted without ECDSA verification.
//! This server MUST NOT be exposed on production networks. It is intended only
//! for local benchmarking and controlled testnet environments.
//!
//! Protocol v2 (per batch):
//!   Client → Server: [u32 LE num_txs] [u16 LE tx_len, tx_bytes, 20-byte sender] × num_txs
//!   Server → Client: [u32 LE accepted_count]
//!   num_txs = 0 → close connection gracefully.

use alloy_consensus::{Transaction, transaction::Recovered};
use alloy_primitives::{Address, U256};
use reth_ethereum_primitives::TransactionSigned;
use reth_transaction_pool::{
    AddedTransactionOutcome, CoinbaseTipOrdering, EthPooledTransaction, Pool, PoolResult,
    PoolTransaction, TransactionOrigin, TransactionPool, TransactionValidationOutcome,
    TransactionValidator, blobstore::BlobStore, validate::ValidTransaction,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, error, info, warn};

/// Trait for trusted direct pool submission used by the ingest path.
pub trait DirectPoolIngest: Send + Sync + 'static {
    /// Insert pre-validated transactions directly into the pool.
    /// Skips TransactionValidationTaskExecutor entirely.
    fn add_prevalidated(
        &self,
        txs: Vec<EthPooledTransaction>,
    ) -> Vec<PoolResult<AddedTransactionOutcome>>;

    /// Returns the current number of transactions in the pending sub-pool.
    fn pending_count(&self) -> usize;
}

/// Implement DirectPoolIngest for any reth Pool with EthPooledTransaction.
impl<V, S> DirectPoolIngest for Pool<V, CoinbaseTipOrdering<EthPooledTransaction>, S>
where
    V: TransactionValidator<Transaction = EthPooledTransaction> + 'static,
    S: BlobStore + 'static,
{
    fn add_prevalidated(
        &self,
        txs: Vec<EthPooledTransaction>,
    ) -> Vec<PoolResult<AddedTransactionOutcome>> {
        let outcomes = txs.into_iter().map(|tx| {
            // Use tx nonce as state_nonce so the pool sees NO nonce gap.
            // ancestor(tx_nonce, state_nonce) returns None when equal → NO_NONCE_GAPS → Pending.
            let nonce = tx.transaction().nonce();
            TransactionValidationOutcome::Valid {
                balance: U256::MAX,
                state_nonce: nonce,
                bytecode_hash: None,
                transaction: ValidTransaction::Valid(tx),
                propagate: false,
                authorities: None,
            }
        });
        self.inner()
            .add_transactions(TransactionOrigin::External, outcomes)
    }

    fn pending_count(&self) -> usize {
        self.pool_size().pending
    }
}

/// Global counters for monitoring.
pub struct IngestStats {
    pub received: AtomicU64,
    pub accepted: AtomicU64,
    pub decode_errors: AtomicU64,
    pub pool_errors: AtomicU64,
}

impl Default for IngestStats {
    fn default() -> Self {
        Self {
            received: AtomicU64::new(0),
            accepted: AtomicU64::new(0),
            decode_errors: AtomicU64::new(0),
            pool_errors: AtomicU64::new(0),
        }
    }
}

impl IngestStats {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Start the binary TCP ingest server.
///
/// Reads port from `N42_INGEST_PORT` env var (default: 19900).
/// Legacy fallback: `N42_INJECT_PORT`.
/// Each node in testnet gets port = base + node_index.
pub async fn run_ingest_server<P>(pool: P)
where
    P: DirectPoolIngest + Clone,
{
    let port: u16 = std::env::var("N42_INGEST_PORT")
        .or_else(|_| std::env::var("N42_INJECT_PORT"))
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(19900);

    let listener = match TcpListener::bind(("0.0.0.0", port)).await {
        Ok(l) => l,
        Err(e) => {
            warn!(target: "n42::ingest", port, error = %e, "failed to bind ingest server");
            return;
        }
    };

    let stats = Arc::new(IngestStats::new());

    info!(target: "n42::ingest", port, "binary TCP ingest server listening");

    // Stats reporter
    let stats_clone = stats.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
        loop {
            interval.tick().await;
            let received = stats_clone.received.load(Ordering::Relaxed);
            if received > 0 {
                info!(
                    target: "n42::ingest",
                    received,
                    accepted = stats_clone.accepted.load(Ordering::Relaxed),
                    decode_err = stats_clone.decode_errors.load(Ordering::Relaxed),
                    pool_err = stats_clone.pool_errors.load(Ordering::Relaxed),
                    "ingest stats"
                );
            }
        }
    });

    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                info!(target: "n42::ingest", %addr, "client connected");
                let pool = pool.clone();
                let stats = stats.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_client(stream, pool, &stats).await {
                        debug!(target: "n42::ingest", %addr, error = %e, "client disconnected");
                    }
                });
            }
            Err(e) => {
                error!(target: "n42::ingest", error = %e, "accept failed");
            }
        }
    }
}

/// Pool high-water mark: reject ingest batches when pending count exceeds this.
/// Prevents pool eviction which causes nonce gaps and permanently stuck txs.
fn ingest_high_water() -> usize {
    static HIGH_WATER: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *HIGH_WATER.get_or_init(|| {
        std::env::var("N42_INGEST_HIGH_WATER")
            .or_else(|_| std::env::var("N42_INJECT_HIGH_WATER"))
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(90_000)
    })
}

async fn handle_client<P>(
    mut stream: TcpStream,
    pool: P,
    stats: &Arc<IngestStats>,
) -> eyre::Result<()>
where
    P: DirectPoolIngest,
{
    // Set TCP_NODELAY for low latency
    stream.set_nodelay(true)?;

    // Reusable buffer for reading tx data
    let mut tx_buf = vec![0u8; 65536];
    let high_water = ingest_high_water();
    let mut gate_logged = false;

    loop {
        // Read batch header: u32 LE num_txs
        let mut header = [0u8; 4];
        if stream.read_exact(&mut header).await.is_err() {
            break; // Connection closed
        }
        let num_txs = u32::from_le_bytes(header) as usize;
        if num_txs == 0 {
            break; // Graceful close
        }

        stats.received.fetch_add(num_txs as u64, Ordering::Relaxed);

        // Read and decode all transactions in this batch
        let mut pooled_txs = Vec::with_capacity(num_txs);
        let mut batch_decode_errors = 0u64;

        for _ in 0..num_txs {
            // Read tx length: u16 LE
            let mut len_buf = [0u8; 2];
            stream.read_exact(&mut len_buf).await?;
            let tx_len = u16::from_le_bytes(len_buf) as usize;

            // Read tx data
            if tx_len > tx_buf.len() {
                tx_buf.resize(tx_len, 0);
            }
            stream.read_exact(&mut tx_buf[..tx_len]).await?;

            // Read pre-recovered sender address (20 bytes)
            let mut sender_buf = [0u8; 20];
            stream.read_exact(&mut sender_buf).await?;
            let sender = Address::from(sender_buf);

            // Decode EIP-2718 encoded transaction
            let tx: TransactionSigned =
                match alloy_eips::eip2718::Decodable2718::decode_2718(&mut &tx_buf[..tx_len]) {
                    Ok(tx) => tx,
                    Err(_) => {
                        batch_decode_errors += 1;
                        continue;
                    }
                };

            // Skip ECDSA recovery — use pre-recovered sender from client
            let recovered = Recovered::new_unchecked(tx, sender);

            // Convert to pooled transaction
            match EthPooledTransaction::try_from_consensus(recovered) {
                Ok(pooled) => pooled_txs.push(pooled),
                Err(_) => {
                    batch_decode_errors += 1;
                }
            }
        }

        if batch_decode_errors > 0 {
            stats
                .decode_errors
                .fetch_add(batch_decode_errors, Ordering::Relaxed);
        }

        // Pool gate: reject entire batch when pool is near capacity.
        // This prevents eviction which causes nonce gaps → permanently stuck txs.
        // ACK v3: [u32 accepted][u32 pool_pending] — second word is pool size hint.
        let pending = pool.pending_count();
        if pending >= high_water {
            if !gate_logged {
                info!(target: "n42::ingest", pending, high_water, "pool gate: rejecting ingest batch (pool near limit)");
                gate_logged = true;
            }
            // ACK v3: accepted=0, pool_pending hint
            let mut ack = [0u8; 8];
            ack[4..8].copy_from_slice(&(pending as u32).to_le_bytes());
            stream.write_all(&ack).await?;
            continue;
        }
        gate_logged = false;

        // Trusted direct pool submission path.
        let batch_size = pooled_txs.len();
        let results = pool.add_prevalidated(pooled_txs);
        let accepted = results.iter().filter(|r| r.is_ok()).count();
        let pool_errors = batch_size - accepted;

        stats.accepted.fetch_add(accepted as u64, Ordering::Relaxed);
        if pool_errors > 0 {
            stats
                .pool_errors
                .fetch_add(pool_errors as u64, Ordering::Relaxed);
        }

        // ACK v3: [u32 accepted][u32 pool_pending]
        let new_pending = pool.pending_count();
        let mut ack = [0u8; 8];
        ack[0..4].copy_from_slice(&(accepted as u32).to_le_bytes());
        ack[4..8].copy_from_slice(&(new_pending as u32).to_le_bytes());
        stream.write_all(&ack).await?;
    }

    Ok(())
}
