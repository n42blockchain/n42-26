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
//! Protocol (legacy default + optional extensions):
//!   Client → Server: [u32 LE num_txs] [u16 LE tx_len, tx_bytes, 20-byte sender] × num_txs
//!   Client → Server: [u32::MAX] to wait for fresh credit without sending a full batch
//!   Server → Client (legacy default): [u32 LE accepted_count] [u32 LE pool_pending]
//!   Server → Client (when `N42_INGEST_EXTENDED_ACK=1`): [u32 LE accepted_count]
//!                               [u32 LE pool_pending] [u32 LE consumed_count]
//!   Server → Client (when `N42_INGEST_CREDIT_PROBE=1`): [u32 LE accepted_count]
//!                               [u32 LE pool_pending] [u32 LE consumed_count]
//!                               [u32 LE credit_available]
//!   num_txs = 0 → close connection gracefully.

use crate::now_unix_ms;
use alloy_consensus::{Transaction, transaction::Recovered};
use alloy_primitives::{Address, U256};
use reth_ethereum_primitives::TransactionSigned;
use reth_transaction_pool::{
    AddedTransactionOutcome, CoinbaseTipOrdering, EthPooledTransaction, Pool, PoolResult,
    PoolTransaction, TransactionOrigin, TransactionPool, TransactionValidationOutcome,
    TransactionValidator, blobstore::BlobStore, validate::ValidTransaction,
};
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

const CREDIT_WAIT_SENTINEL: u32 = u32::MAX;
const DEFAULT_INGEST_HIGH_WATER: usize = 90_000;
const DEFAULT_INGEST_TARGET_PENDING: usize = 82_000;
const DEFAULT_INGEST_VIRTUAL_BLOCK_CREDIT_MS: u64 = 0;
const DEFAULT_POOL_MAX_TXS: usize = 100_000;

#[derive(Clone, Copy, Default)]
struct VirtualBlockCreditState {
    available_txs: usize,
    until_ms: u64,
}

static VIRTUAL_BLOCK_CREDIT: LazyLock<RwLock<VirtualBlockCreditState>> =
    LazyLock::new(|| RwLock::new(VirtualBlockCreditState::default()));

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
    pub soft_gated: AtomicU64,
    pub credit_waits: AtomicU64,
    pub credit_wait_ms: AtomicU64,
    pub partial_batches: AtomicU64,
    pub deferred: AtomicU64,
}

impl Default for IngestStats {
    fn default() -> Self {
        Self {
            received: AtomicU64::new(0),
            accepted: AtomicU64::new(0),
            decode_errors: AtomicU64::new(0),
            pool_errors: AtomicU64::new(0),
            soft_gated: AtomicU64::new(0),
            credit_waits: AtomicU64::new(0),
            credit_wait_ms: AtomicU64::new(0),
            partial_batches: AtomicU64::new(0),
            deferred: AtomicU64::new(0),
        }
    }
}

impl IngestStats {
    pub fn new() -> Self {
        Self::default()
    }
}

fn configured_block_cap() -> Option<usize> {
    std::env::var("N42_MAX_TXS_PER_BLOCK")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|cap| *cap > 0)
}

fn adaptive_pool_limit_txs() -> usize {
    std::env::var("N42_POOL_MAX_TXS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|cap| *cap > 0)
        .unwrap_or_else(|| {
            configured_block_cap()
                .map(|cap| DEFAULT_POOL_MAX_TXS.max(cap.saturating_mul(3)))
                .unwrap_or(DEFAULT_POOL_MAX_TXS)
        })
}

fn ingest_virtual_block_credit_ttl_ms() -> u64 {
    static TTL_MS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *TTL_MS.get_or_init(|| {
        std::env::var("N42_INGEST_VIRTUAL_BLOCK_CREDIT_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_INGEST_VIRTUAL_BLOCK_CREDIT_MS)
    })
}

fn ingest_virtual_block_credit_cap() -> usize {
    static CAP_TXS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CAP_TXS.get_or_init(|| {
        std::env::var("N42_INGEST_VIRTUAL_BLOCK_CREDIT_TXS")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|cap| *cap > 0)
            .unwrap_or_else(|| configured_block_cap().unwrap_or_default())
    })
}

fn reset_expired_virtual_block_credit(state: &mut VirtualBlockCreditState, now_ms: u64) {
    if state.until_ms == 0 || now_ms > state.until_ms {
        state.available_txs = 0;
        state.until_ms = 0;
    }
}

pub(crate) fn note_virtual_block_credit(tx_count: usize, source: &'static str) {
    let ttl_ms = ingest_virtual_block_credit_ttl_ms();
    if ttl_ms == 0 || tx_count == 0 {
        return;
    }

    let credit_txs = tx_count.min(ingest_virtual_block_credit_cap());
    if credit_txs == 0 {
        return;
    }

    let now_ms = now_unix_ms();
    let until_ms = now_ms.saturating_add(ttl_ms);
    if let Ok(mut state) = VIRTUAL_BLOCK_CREDIT.write() {
        reset_expired_virtual_block_credit(&mut state, now_ms);
        state.available_txs = state.available_txs.max(credit_txs);
        state.until_ms = state.until_ms.max(until_ms);
    }
    info!(
        target: "n42::ingest",
        source,
        credit_txs,
        ttl_ms,
        until_ms,
        "virtual block credit armed"
    );
}

fn active_virtual_block_credit(base_high_water: usize) -> usize {
    if ingest_virtual_block_credit_ttl_ms() == 0 {
        return 0;
    }
    let Ok(state) = VIRTUAL_BLOCK_CREDIT.read() else {
        return 0;
    };
    if now_unix_ms() > state.until_ms {
        return 0;
    }

    let pool_limit = adaptive_pool_limit_txs();
    let extra_headroom = pool_limit.saturating_sub(base_high_water);
    let credit_txs = state.available_txs;
    credit_txs.min(extra_headroom)
}

fn effective_high_water(base_high_water: usize, credit_probe_enabled: bool) -> usize {
    if credit_probe_enabled {
        base_high_water.saturating_add(active_virtual_block_credit(base_high_water))
    } else {
        base_high_water
    }
}

fn effective_credit_limit(
    base_high_water: usize,
    target_pending: usize,
    credit_probe_enabled: bool,
) -> usize {
    if credit_probe_enabled {
        effective_high_water(base_high_water, true)
    } else {
        target_pending
    }
}

fn ingest_extended_ack_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        matches!(
            std::env::var("N42_INGEST_EXTENDED_ACK").ok().as_deref(),
            Some("1") | Some("true") | Some("TRUE") | Some("yes") | Some("YES")
        )
    })
}

fn reserve_credit_probe_allowance(base_high_water: usize, pending: usize) -> (usize, usize, usize) {
    let base_credit = base_high_water.saturating_sub(pending);
    if ingest_virtual_block_credit_ttl_ms() == 0 {
        return (base_high_water, base_high_water, base_credit);
    }

    let pool_limit = adaptive_pool_limit_txs();
    let extra_headroom = pool_limit.saturating_sub(base_high_water);
    let now_ms = now_unix_ms();
    let Ok(mut state) = VIRTUAL_BLOCK_CREDIT.write() else {
        return (base_high_water, base_high_water, base_credit);
    };
    reset_expired_virtual_block_credit(&mut state, now_ms);

    let virtual_available = state.available_txs.min(extra_headroom);
    let dynamic_high_water = base_high_water.saturating_add(virtual_available);
    if pending >= dynamic_high_water {
        return (dynamic_high_water, dynamic_high_water, 0);
    }

    let virtual_grant = dynamic_high_water
        .saturating_sub(pending)
        .saturating_sub(base_credit);
    let reserved_virtual = virtual_grant.min(state.available_txs);
    state.available_txs = state.available_txs.saturating_sub(reserved_virtual);
    if state.available_txs == 0 {
        state.until_ms = 0;
    }

    (
        dynamic_high_water,
        dynamic_high_water,
        base_credit.saturating_add(reserved_virtual),
    )
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
    let high_water = ingest_high_water();
    let target_pending = ingest_target_pending(high_water);

    info!(
        target: "n42::ingest",
        port,
        high_water,
        target_pending,
        pool_limit_txs = adaptive_pool_limit_txs(),
        block_cap = configured_block_cap(),
        "binary TCP ingest server listening"
    );

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
                    soft_gated = stats_clone.soft_gated.load(Ordering::Relaxed),
                    credit_waits = stats_clone.credit_waits.load(Ordering::Relaxed),
                    credit_wait_ms = stats_clone.credit_wait_ms.load(Ordering::Relaxed),
                    partial_batches = stats_clone.partial_batches.load(Ordering::Relaxed),
                    deferred = stats_clone.deferred.load(Ordering::Relaxed),
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
            .unwrap_or_else(|| {
                configured_block_cap()
                    .map(|cap| {
                        let pool_limit = adaptive_pool_limit_txs();
                        let headroom = (cap / 3).max(8_000);
                        DEFAULT_INGEST_HIGH_WATER
                            .max(cap.saturating_mul(8) / 3)
                            .min(pool_limit.saturating_sub(headroom))
                    })
                    .unwrap_or(DEFAULT_INGEST_HIGH_WATER)
            })
    })
}

/// Target pending window for partial-fill credit. Keeps some headroom below the hard gate.
fn ingest_target_pending(high_water: usize) -> usize {
    static TARGET_PENDING: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *TARGET_PENDING.get_or_init(|| {
        let default_target = configured_block_cap()
            .map(|cap| high_water.saturating_sub((cap / 6).max(8_000)))
            .unwrap_or_else(|| high_water.saturating_sub(8_000))
            .max(DEFAULT_INGEST_TARGET_PENDING.min(high_water));
        std::env::var("N42_INGEST_TARGET_PENDING")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default_target)
            .min(high_water)
    })
}

/// Poll interval used by the ingest server while holding a buffered batch
/// until the txpool has reopened enough credit to consume it.
fn ingest_credit_poll_interval() -> Duration {
    static CREDIT_POLL_MS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    Duration::from_millis(
        (*CREDIT_POLL_MS.get_or_init(|| {
            std::env::var("N42_INGEST_CREDIT_POLL_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(10)
        }))
        .max(1),
    )
}

fn ingest_credit_probe_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        matches!(
            std::env::var("N42_INGEST_CREDIT_PROBE").ok().as_deref(),
            Some("1") | Some("true") | Some("TRUE") | Some("yes") | Some("YES")
        )
    })
}

fn credit_available(target_pending: usize, pending: usize) -> usize {
    target_pending.saturating_sub(pending)
}

async fn write_ack(
    stream: &mut TcpStream,
    accepted: usize,
    pool_pending: usize,
    consumed: usize,
    credit_available: usize,
    extended_ack_enabled: bool,
    credit_probe_enabled: bool,
) -> eyre::Result<()> {
    if credit_probe_enabled {
        let mut ack = [0u8; 16];
        ack[0..4].copy_from_slice(&(accepted.min(u32::MAX as usize) as u32).to_le_bytes());
        ack[4..8].copy_from_slice(&(pool_pending.min(u32::MAX as usize) as u32).to_le_bytes());
        ack[8..12].copy_from_slice(&(consumed.min(u32::MAX as usize) as u32).to_le_bytes());
        ack[12..16]
            .copy_from_slice(&(credit_available.min(u32::MAX as usize) as u32).to_le_bytes());
        stream.write_all(&ack).await?;
    } else if extended_ack_enabled {
        let mut ack = [0u8; 12];
        ack[0..4].copy_from_slice(&(accepted.min(u32::MAX as usize) as u32).to_le_bytes());
        ack[4..8].copy_from_slice(&(pool_pending.min(u32::MAX as usize) as u32).to_le_bytes());
        ack[8..12].copy_from_slice(&(consumed.min(u32::MAX as usize) as u32).to_le_bytes());
        stream.write_all(&ack).await?;
    } else {
        let mut ack = [0u8; 8];
        ack[0..4].copy_from_slice(&(accepted.min(u32::MAX as usize) as u32).to_le_bytes());
        ack[4..8].copy_from_slice(&(pool_pending.min(u32::MAX as usize) as u32).to_le_bytes());
        stream.write_all(&ack).await?;
    }
    Ok(())
}

async fn wait_for_credit_probe<P>(
    pool: &P,
    stats: &Arc<IngestStats>,
    base_high_water: usize,
    target_pending: usize,
    hard_gate_logged: &mut bool,
    soft_gate_logged: &mut bool,
) -> (usize, usize)
where
    P: DirectPoolIngest,
{
    let poll_interval = ingest_credit_poll_interval();
    let wait_start = Instant::now();
    let mut pending = pool.pending_count();
    let mut waited_for_credit = false;

    loop {
        let dynamic_high_water = effective_high_water(base_high_water, true);

        if pending >= dynamic_high_water {
            if !*hard_gate_logged {
                info!(
                    target: "n42::ingest",
                    pending,
                    high_water = dynamic_high_water,
                    base_high_water,
                    virtual_headroom = dynamic_high_water.saturating_sub(base_high_water),
                    "pool gate: waiting on credit probe until pending falls below high water"
                );
                *hard_gate_logged = true;
            }
        } else {
            *hard_gate_logged = false;
            let (dynamic_high_water, dynamic_credit_limit, credit) =
                reserve_credit_probe_allowance(base_high_water, pending);
            if credit > 0 {
                if waited_for_credit {
                    let waited_ms = wait_start.elapsed().as_millis() as u64;
                    stats.credit_waits.fetch_add(1, Ordering::Relaxed);
                    stats.credit_wait_ms.fetch_add(waited_ms, Ordering::Relaxed);
                    info!(
                        target: "n42::ingest",
                        pending_before = pending,
                        credit_available = credit,
                        credit_limit = dynamic_credit_limit,
                        high_water = dynamic_high_water,
                        base_high_water,
                        virtual_headroom = dynamic_high_water.saturating_sub(base_high_water),
                        waited_ms,
                        "pool credit reopened: replying to credit probe"
                    );
                }
                *soft_gate_logged = false;
                return (pending, credit);
            }

            if !*soft_gate_logged {
                info!(
                    target: "n42::ingest",
                    pending,
                    target_pending,
                    base_high_water,
                    virtual_headroom = dynamic_high_water.saturating_sub(base_high_water),
                    "pool target reached: waiting on credit probe"
                );
                *soft_gate_logged = true;
            }
        }

        if !waited_for_credit {
            stats.soft_gated.fetch_add(1, Ordering::Relaxed);
            waited_for_credit = true;
        }

        tokio::time::sleep(poll_interval).await;
        pending = pool.pending_count();
    }
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
    let base_high_water = ingest_high_water();
    let target_pending = ingest_target_pending(base_high_water);
    let credit_probe_enabled = ingest_credit_probe_enabled();
    let extended_ack_enabled = credit_probe_enabled || ingest_extended_ack_enabled();
    let mut hard_gate_logged = false;
    let mut soft_gate_logged = false;
    let mut leased_credit_remaining = 0usize;

    loop {
        // Read batch header: u32 LE num_txs
        let mut header = [0u8; 4];
        if stream.read_exact(&mut header).await.is_err() {
            break; // Connection closed
        }
        let header_value = u32::from_le_bytes(header);
        if header_value == 0 {
            break; // Graceful close
        }
        if header_value == CREDIT_WAIT_SENTINEL {
            if !credit_probe_enabled {
                let pending = pool.pending_count();
                warn!(
                    target: "n42::ingest",
                    pending,
                    "received credit probe sentinel while credit probe mode is disabled"
                );
                write_ack(&mut stream, 0, pending, 0, 0, extended_ack_enabled, false).await?;
                continue;
            }
            let (pending, credit) = wait_for_credit_probe(
                &pool,
                stats,
                base_high_water,
                target_pending,
                &mut hard_gate_logged,
                &mut soft_gate_logged,
            )
            .await;
            leased_credit_remaining = credit;
            write_ack(
                &mut stream,
                0,
                pending,
                0,
                credit,
                extended_ack_enabled,
                credit_probe_enabled,
            )
            .await?;
            continue;
        }
        let num_txs = header_value as usize;

        stats.received.fetch_add(num_txs as u64, Ordering::Relaxed);

        // Read the batch into memory first. This lets the sender use credit probes
        // without requiring a second decode pass when only a prefix is admissible.
        let mut raw_txs = Vec::with_capacity(num_txs);

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
            raw_txs.push((tx_buf[..tx_len].to_vec(), sender));
        }

        let pending_before = pool.pending_count();
        let dynamic_high_water = effective_high_water(base_high_water, credit_probe_enabled);
        let dynamic_credit_limit =
            effective_credit_limit(base_high_water, target_pending, credit_probe_enabled);
        let take_limit = if credit_probe_enabled {
            leased_credit_remaining.min(num_txs)
        } else if !extended_ack_enabled {
            num_txs
        } else if pending_before >= dynamic_high_water {
            0
        } else {
            credit_available(dynamic_credit_limit, pending_before).min(num_txs)
        };

        if pending_before >= dynamic_high_water
            && (!credit_probe_enabled || leased_credit_remaining == 0)
        {
            if !hard_gate_logged {
                info!(
                    target: "n42::ingest",
                    pending = pending_before,
                    high_water = dynamic_high_water,
                    base_high_water,
                    virtual_headroom = dynamic_high_water.saturating_sub(base_high_water),
                    num_txs,
                    "pool gate: rejecting ingest batch (pool near limit)"
                );
                hard_gate_logged = true;
            }
            write_ack(
                &mut stream,
                0,
                pending_before,
                0,
                0,
                extended_ack_enabled,
                credit_probe_enabled,
            )
            .await?;
            continue;
        }
        hard_gate_logged = false;

        if extended_ack_enabled && take_limit == 0 {
            stats.soft_gated.fetch_add(1, Ordering::Relaxed);
            if !soft_gate_logged {
                info!(
                    target: "n42::ingest",
                    pending = pending_before,
                    target_pending = dynamic_credit_limit,
                    base_high_water,
                    virtual_headroom = dynamic_high_water.saturating_sub(base_high_water),
                    num_txs,
                    "pool target reached: sender should wait on credit probe"
                );
                soft_gate_logged = true;
            }
            write_ack(
                &mut stream,
                0,
                pending_before,
                0,
                0,
                extended_ack_enabled,
                credit_probe_enabled,
            )
            .await?;
            continue;
        }
        soft_gate_logged = false;

        // Decode only the prefix that the current credit window can consume.
        let mut pooled_txs = Vec::with_capacity(take_limit);
        let mut batch_decode_errors = 0u64;
        for (tx_bytes, sender) in raw_txs.iter().take(take_limit) {
            let tx: TransactionSigned =
                match alloy_eips::eip2718::Decodable2718::decode_2718(&mut &tx_bytes[..]) {
                    Ok(tx) => tx,
                    Err(_) => {
                        batch_decode_errors += 1;
                        continue;
                    }
                };

            let recovered = Recovered::new_unchecked(tx, *sender);
            match EthPooledTransaction::try_from_consensus(recovered) {
                Ok(pooled) => pooled_txs.push(pooled),
                Err(_) => batch_decode_errors += 1,
            }
        }

        if batch_decode_errors > 0 {
            stats
                .decode_errors
                .fetch_add(batch_decode_errors, Ordering::Relaxed);
        }

        // Trusted direct pool submission path.
        let batch_size = pooled_txs.len();
        let results = pool.add_prevalidated(pooled_txs);
        let accepted = results.iter().filter(|r| r.is_ok()).count();
        let pool_errors = batch_size - accepted;
        let consumed = take_limit;

        stats.accepted.fetch_add(accepted as u64, Ordering::Relaxed);
        if pool_errors > 0 {
            stats
                .pool_errors
                .fetch_add(pool_errors as u64, Ordering::Relaxed);
        }
        if consumed < num_txs {
            stats.partial_batches.fetch_add(1, Ordering::Relaxed);
            stats
                .deferred
                .fetch_add((num_txs - consumed) as u64, Ordering::Relaxed);
        }
        if credit_probe_enabled {
            leased_credit_remaining = leased_credit_remaining.saturating_sub(consumed);
        }

        let new_pending = pool.pending_count();
        write_ack(
            &mut stream,
            accepted,
            new_pending,
            consumed,
            leased_credit_remaining,
            extended_ack_enabled,
            credit_probe_enabled,
        )
        .await?;
    }

    Ok(())
}
