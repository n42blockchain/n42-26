//! Binary TCP injection server for high-speed transaction ingestion.
//!
//! Accepts raw EIP-2718 encoded transactions over TCP, bypassing JSON-RPC overhead.
//! Transactions are decoded, sender recovered, and batch-inserted into the pool.
//!
//! Protocol (per batch):
//!   Client → Server: [u32 LE num_txs] [u16 LE tx_len, tx_bytes] × num_txs
//!   Server → Client: [u32 LE accepted_count]
//!   num_txs = 0 → close connection gracefully.

use reth_ethereum_primitives::TransactionSigned;
use reth_primitives_traits::SignedTransaction;
use reth_transaction_pool::{EthPooledTransaction, PoolTransaction, TransactionPool};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, error, info, warn};

/// Global counters for monitoring.
pub struct InjectStats {
    pub received: AtomicU64,
    pub accepted: AtomicU64,
    pub decode_errors: AtomicU64,
    pub pool_errors: AtomicU64,
}

impl InjectStats {
    pub fn new() -> Self {
        Self {
            received: AtomicU64::new(0),
            accepted: AtomicU64::new(0),
            decode_errors: AtomicU64::new(0),
            pool_errors: AtomicU64::new(0),
        }
    }
}

/// Start the binary injection TCP server.
///
/// Reads port from `N42_INJECT_PORT` env var (default: 19900).
/// Each node in testnet gets port = base + node_index.
pub async fn run_inject_server<Pool>(pool: Pool)
where
    Pool: TransactionPool<Transaction = EthPooledTransaction> + Clone + 'static,
{
    let port: u16 = std::env::var("N42_INJECT_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(19900);

    let listener = match TcpListener::bind(("0.0.0.0", port)).await {
        Ok(l) => l,
        Err(e) => {
            warn!(target: "n42::inject", port, error = %e, "failed to bind inject server");
            return;
        }
    };

    let stats = Arc::new(InjectStats::new());

    info!(target: "n42::inject", port, "binary injection server listening");

    // Stats reporter
    let stats_clone = stats.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
        loop {
            interval.tick().await;
            let received = stats_clone.received.load(Ordering::Relaxed);
            if received > 0 {
                info!(
                    target: "n42::inject",
                    received,
                    accepted = stats_clone.accepted.load(Ordering::Relaxed),
                    decode_err = stats_clone.decode_errors.load(Ordering::Relaxed),
                    pool_err = stats_clone.pool_errors.load(Ordering::Relaxed),
                    "inject stats"
                );
            }
        }
    });

    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                info!(target: "n42::inject", %addr, "client connected");
                let pool = pool.clone();
                let stats = stats.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_client(stream, pool, &stats).await {
                        debug!(target: "n42::inject", %addr, error = %e, "client disconnected");
                    }
                });
            }
            Err(e) => {
                error!(target: "n42::inject", error = %e, "accept failed");
            }
        }
    }
}

async fn handle_client<Pool>(
    mut stream: TcpStream,
    pool: Pool,
    stats: &Arc<InjectStats>,
) -> eyre::Result<()>
where
    Pool: TransactionPool<Transaction = EthPooledTransaction>,
{
    // Set TCP_NODELAY for low latency
    stream.set_nodelay(true)?;

    // Reusable buffer for reading tx data
    let mut tx_buf = vec![0u8; 65536];

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

            // Decode EIP-2718 encoded transaction
            let tx: TransactionSigned =
                match alloy_eips::eip2718::Decodable2718::decode_2718(&mut &tx_buf[..tx_len]) {
                    Ok(tx) => tx,
                    Err(_) => {
                        batch_decode_errors += 1;
                        continue;
                    }
                };

            // Recover sender
            let recovered = match tx.try_into_recovered() {
                Ok(r) => r,
                Err(_) => {
                    batch_decode_errors += 1;
                    continue;
                }
            };

            // Convert to pooled transaction
            match EthPooledTransaction::try_from_consensus(recovered) {
                Ok(pooled) => pooled_txs.push(pooled),
                Err(_) => {
                    batch_decode_errors += 1;
                }
            }
        }

        if batch_decode_errors > 0 {
            stats.decode_errors.fetch_add(batch_decode_errors, Ordering::Relaxed);
        }

        // Batch insert into pool
        let batch_size = pooled_txs.len();
        let results = pool.add_external_transactions(pooled_txs).await;
        let accepted = results.iter().filter(|r| r.is_ok()).count();
        let pool_errors = batch_size - accepted;

        stats.accepted.fetch_add(accepted as u64, Ordering::Relaxed);
        if pool_errors > 0 {
            stats.pool_errors.fetch_add(pool_errors as u64, Ordering::Relaxed);
        }

        // Send ACK: u32 LE accepted_count
        let ack = (accepted as u32).to_le_bytes();
        stream.write_all(&ack).await?;
    }

    Ok(())
}
