//! N42 High-Performance TPS Stress Test v10
//!
//! v10 improvements over v9:
//! - `--blast N` mode: pre-sign N txs, blast at max speed with optimal settings
//! - Smaller batch size default (500) for faster HTTP round trips
//! - HTTP/1.1 forced (avoids HTTP/2 single-connection multiplexing bottleneck)
//! - Higher per-RPC concurrency (32 inflight batches vs 4 pipeline depth)
//! - Real-time block TPS monitoring during blast
//! - Default accounts increased to 5000
//!
//! v9 features retained:
//! - Pipelined signing and sending for --step mode
//! - Pre-sign save/load for binary tx files
//! - Backpressure monitoring

use alloy_consensus::{SignableTransaction, TxEip1559};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, Bytes, TxKind, U256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use clap::Parser;
use eyre::Result;
use std::io::{BufReader, BufWriter, Read as IoRead, Write as IoWrite};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tiny_keccak::{Hasher, Keccak};
use tokio::sync::Semaphore;

#[derive(Parser)]
#[command(name = "n42-stress")]
struct Cli {
    /// Target TPS (0 = unlimited)
    #[arg(long, default_value = "2000")]
    target_tps: u64,

    /// Test duration in seconds
    #[arg(long, default_value = "60")]
    duration: u64,

    /// Number of sender accounts
    #[arg(long, default_value = "5000")]
    accounts: usize,

    /// Batch size for JSON-RPC batch requests
    #[arg(long, default_value = "500")]
    batch_size: usize,

    /// Number of accounts per batch (multi-account mixing)
    #[arg(long, default_value = "100")]
    accounts_per_batch: usize,

    /// Max concurrent HTTP requests across all RPC endpoints
    #[arg(long, default_value = "1024")]
    concurrency: usize,

    /// Percentage of contract-call txs (0-100, simulates ERC-20 transfers)
    #[arg(long, default_value = "0")]
    erc20_ratio: u8,

    /// Step mode: auto-increase TPS to find max
    #[arg(long)]
    step: bool,

    /// Blast mode: pre-sign N txs, then blast at max speed.
    /// Optimal for measuring node throughput ceiling.
    /// Example: --blast 5000000 signs 5M txs then sends as fast as possible.
    #[arg(long, default_value = "0")]
    blast: u64,

    /// Pre-fill mode: flood the tx pool before measurement starts.
    /// Value = number of txs to pre-send (e.g., 10000).
    #[arg(long, default_value = "0")]
    prefill: u64,

    /// Pool backpressure: pause sending when pool_pending exceeds this
    #[arg(long, default_value = "200000")]
    max_pool: u64,

    /// Pool resume threshold: resume sending when pool_pending drops below this
    #[arg(long, default_value = "100000")]
    resume_pool: u64,

    /// Pre-sign mode: sign N transactions into memory first, then blast-send.
    /// Removes signing from the hot path for maximum injection rate.
    /// Example: --presign 3000000 signs 3M txs, then sends in 30s = 100K TPS injection.
    #[arg(long, default_value = "0")]
    presign: u64,

    /// Save pre-signed transactions to binary file for reuse.
    /// Usage: --presign-save txdata.bin --presign 5000000 --accounts 5000
    /// Requires chain connection to sync nonces. File can be reused with --presign-load.
    #[arg(long)]
    presign_save: Option<String>,

    /// Load pre-signed transactions from binary file and blast-send.
    /// Usage: --presign-load txdata.bin
    /// Skips all signing — pure I/O send for maximum injection rate.
    #[arg(long)]
    presign_load: Option<String>,

    /// Wave mode: inject exactly <cap> txs, wait for block, repeat.
    /// Keeps pool_pending ≈ cap, preventing pool overload.
    /// Usage: --wave 48000 --presign-load txdata.bin
    #[arg(long, default_value = "0")]
    wave: u64,

    /// Binary TCP injection endpoints (comma-separated host:port).
    /// Bypasses JSON-RPC, sends raw EIP-2718 txs over TCP for maximum injection speed.
    /// Usage: --inject 127.0.0.1:19900,127.0.0.1:19901,... --presign-load txdata.bin
    #[arg(long)]
    inject: Option<String>,

    /// RPC endpoints (comma-separated)
    #[arg(long, default_value = "http://127.0.0.1:18545,http://127.0.0.1:18546,http://127.0.0.1:18547,http://127.0.0.1:18548,http://127.0.0.1:18549,http://127.0.0.1:18550,http://127.0.0.1:18551")]
    rpc: String,
}

const CHAIN_ID: u64 = 4242;
const TRANSFER_GAS: u64 = 21_000;
const CONTRACT_CALL_GAS: u64 = 65_000;
const MAX_FEE_PER_GAS: u128 = 2_000_000_000;
const MAX_PRIORITY_FEE: u128 = 1_000_000_000;
const TRANSFER_VALUE: u128 = 1_000_000_000_000;
/// Pre-deployed stress contract address (must match n42-chainspec STRESS_CONTRACT_ADDRESS)
const STRESS_CONTRACT: Address = {
    let mut addr = [0u8; 20];
    addr[18] = 0xC0;
    addr[19] = 0x42;
    Address::new(addr)
};

struct Stats {
    sent: AtomicU64,
    rpc_errors: AtomicU64,
    http_errors: AtomicU64,
    start_block: AtomicU64,
    rpc_latency_ns: AtomicU64,
    rpc_latency_count: AtomicU64,
    sign_time_ns: AtomicU64,
    sign_count: AtomicU64,
    backpressure_pauses: AtomicU64,
    backpressure_ms: AtomicU64,
    nonce_resyncs: AtomicU64,
}

impl Stats {
    fn new() -> Self {
        Self {
            sent: AtomicU64::new(0),
            rpc_errors: AtomicU64::new(0),
            http_errors: AtomicU64::new(0),
            start_block: AtomicU64::new(0),
            rpc_latency_ns: AtomicU64::new(0),
            rpc_latency_count: AtomicU64::new(0),
            sign_time_ns: AtomicU64::new(0),
            sign_count: AtomicU64::new(0),
            backpressure_pauses: AtomicU64::new(0),
            backpressure_ms: AtomicU64::new(0),
            nonce_resyncs: AtomicU64::new(0),
        }
    }

    fn reset(&self) {
        self.sent.store(0, Ordering::Relaxed);
        self.rpc_errors.store(0, Ordering::Relaxed);
        self.http_errors.store(0, Ordering::Relaxed);
        self.rpc_latency_ns.store(0, Ordering::Relaxed);
        self.rpc_latency_count.store(0, Ordering::Relaxed);
        self.sign_time_ns.store(0, Ordering::Relaxed);
        self.sign_count.store(0, Ordering::Relaxed);
        self.backpressure_pauses.store(0, Ordering::Relaxed);
        self.backpressure_ms.store(0, Ordering::Relaxed);
        self.nonce_resyncs.store(0, Ordering::Relaxed);
    }

    fn avg_rpc_latency_ms(&self) -> f64 {
        let count = self.rpc_latency_count.load(Ordering::Relaxed);
        if count == 0 {
            return 0.0;
        }
        (self.rpc_latency_ns.load(Ordering::Relaxed) as f64 / count as f64) / 1_000_000.0
    }

    fn avg_sign_us(&self) -> f64 {
        let count = self.sign_count.load(Ordering::Relaxed);
        if count == 0 {
            return 0.0;
        }
        (self.sign_time_ns.load(Ordering::Relaxed) as f64 / count as f64) / 1000.0
    }
}

struct TestAccount {
    signer: PrivateKeySigner,
    address: Address,
    nonce: AtomicU64,
    rpc_idx: usize,
}

fn derive_private_key(index: usize) -> [u8; 32] {
    let seed = format!("n42-test-key-{}", index);
    let mut hasher = Keccak::v256();
    hasher.update(seed.as_bytes());
    let mut output = [0u8; 32];
    hasher.finalize(&mut output);
    output
}

fn create_accounts(count: usize, num_rpcs: usize) -> Vec<Arc<TestAccount>> {
    (0..count)
        .map(|i| {
            let pk = derive_private_key(i);
            let signer = PrivateKeySigner::from_bytes(&pk.into()).expect("valid key");
            let address = signer.address();
            Arc::new(TestAccount {
                signer,
                address,
                nonce: AtomicU64::new(0),
                rpc_idx: i % num_rpcs,
            })
        })
        .collect()
}

/// Partition accounts by RPC index. Returns Vec[rpc_idx] = Vec<account>.
fn partition_by_rpc(accounts: &[Arc<TestAccount>], num_rpcs: usize) -> Vec<Vec<Arc<TestAccount>>> {
    let mut partitions: Vec<Vec<Arc<TestAccount>>> = vec![Vec::new(); num_rpcs];
    for account in accounts {
        partitions[account.rpc_idx].push(account.clone());
    }
    partitions
}

/// Sign a batch. Nonces allocated atomically before signing.
/// Returns (raw_txs, sign_duration, nonce_info).
fn sign_mixed_batch(
    accounts: &[Arc<TestAccount>],
    targets: &[Address],
    batch_size: usize,
    accounts_per_batch: usize,
    erc20_ratio: u8,
) -> (Vec<String>, Duration, Vec<(usize, u64, u64)>) {
    let start = Instant::now();
    let apb = accounts_per_batch.min(accounts.len()).max(1);
    let txs_per_account = batch_size / apb;
    let remainder = batch_size % apb;
    let mut result = Vec::with_capacity(batch_size);
    let mut nonce_info = Vec::with_capacity(apb);
    let mut tx_index: usize = 0;

    for (acct_idx, account) in accounts.iter().take(apb).enumerate() {
        let count = txs_per_account + if acct_idx < remainder { 1 } else { 0 };
        if count == 0 {
            continue;
        }
        let nonce_start = account.nonce.fetch_add(count as u64, Ordering::Relaxed);
        nonce_info.push((acct_idx, nonce_start, count as u64));

        for j in 0..count {
            let nonce = nonce_start + j as u64;
            let is_contract_call = erc20_ratio > 0 && (tx_index % 100) < erc20_ratio as usize;
            tx_index += 1;

            let tx = if is_contract_call {
                TxEip1559 {
                    chain_id: CHAIN_ID,
                    nonce,
                    gas_limit: CONTRACT_CALL_GAS,
                    max_fee_per_gas: MAX_FEE_PER_GAS,
                    max_priority_fee_per_gas: MAX_PRIORITY_FEE,
                    to: TxKind::Call(STRESS_CONTRACT),
                    value: U256::ZERO,
                    input: Bytes::new(),
                    access_list: Default::default(),
                }
            } else {
                let to = targets[(nonce as usize) % targets.len()];
                TxEip1559 {
                    chain_id: CHAIN_ID,
                    nonce,
                    gas_limit: TRANSFER_GAS,
                    max_fee_per_gas: MAX_FEE_PER_GAS,
                    max_priority_fee_per_gas: MAX_PRIORITY_FEE,
                    to: TxKind::Call(to),
                    value: U256::from(TRANSFER_VALUE),
                    input: Bytes::new(),
                    access_list: Default::default(),
                }
            };
            let sig_hash = tx.signature_hash();
            let sig = account.signer.sign_hash_sync(&sig_hash).expect("sign");
            let signed = tx.into_signed(sig);
            let mut buf = Vec::with_capacity(128);
            signed.encode_2718(&mut buf);
            result.push(format!("0x{}", hex::encode(&buf)));
        }
    }

    (result, start.elapsed(), nonce_info)
}

struct BatchResult {
    ok: usize,
    rpc_error: usize,
    http_error: usize,
    latency: Duration,
    needs_resync: bool,
}

async fn send_batch(
    client: &reqwest::Client,
    rpc_url: &str,
    raw_txs: &[String],
) -> BatchResult {
    let batch: Vec<serde_json::Value> = raw_txs
        .iter()
        .enumerate()
        .map(|(i, tx)| {
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": "eth_sendRawTransaction",
                "params": [tx],
                "id": i + 1
            })
        })
        .collect();

    let start = Instant::now();
    match client.post(rpc_url).json(&batch).send().await {
        Ok(resp) => match resp.json::<Vec<serde_json::Value>>().await {
            Ok(results) => {
                let latency = start.elapsed();
                let mut ok = 0;
                let mut rpc_error = 0;
                let mut nonce_errors = 0;
                for r in &results {
                    if let Some(err) = r.get("error") {
                        rpc_error += 1;
                        let msg = err.get("message").and_then(|m| m.as_str()).unwrap_or("");
                        if msg.contains("nonce") || msg.contains("already known")
                            || msg.contains("replacement") || msg.contains("underpriced")
                        {
                            nonce_errors += 1;
                        }
                    } else {
                        ok += 1;
                    }
                }
                let total = ok + rpc_error;
                let needs_resync = total > 0 && nonce_errors * 2 > total;
                BatchResult { ok, rpc_error, http_error: 0, latency, needs_resync }
            }
            Err(_) => BatchResult {
                ok: 0, rpc_error: 0, http_error: raw_txs.len(),
                latency: start.elapsed(), needs_resync: false,
            },
        },
        Err(_) => BatchResult {
            ok: 0, rpc_error: 0, http_error: raw_txs.len(),
            latency: start.elapsed(), needs_resync: false,
        },
    }
}

async fn rpc_call(
    client: &reqwest::Client,
    rpc_url: &str,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value> {
    let payload = serde_json::json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
        "id": 1
    });
    let resp: serde_json::Value = client.post(rpc_url).json(&payload).send().await?.json().await?;
    Ok(resp)
}

fn parse_hex_u64(hex: &str) -> u64 {
    u64::from_str_radix(hex.trim_start_matches("0x"), 16).unwrap_or(0)
}

async fn get_nonce(client: &reqwest::Client, rpc_url: &str, address: &Address) -> Result<u64> {
    let resp = rpc_call(client, rpc_url, "eth_getTransactionCount",
        serde_json::json!([format!("{address:?}"), "pending"])).await?;
    Ok(parse_hex_u64(resp["result"].as_str().unwrap_or("0x0")))
}

async fn get_block_number(client: &reqwest::Client, rpc_url: &str) -> Result<u64> {
    let resp = rpc_call(client, rpc_url, "eth_blockNumber", serde_json::json!([])).await?;
    Ok(parse_hex_u64(resp["result"].as_str().unwrap_or("0x0")))
}

async fn get_txpool_status(client: &reqwest::Client, rpc_url: &str) -> Result<(u64, u64)> {
    let resp = rpc_call(client, rpc_url, "txpool_status", serde_json::json!([])).await?;
    let pending = parse_hex_u64(resp["result"]["pending"].as_str().unwrap_or("0x0"));
    let queued = parse_hex_u64(resp["result"]["queued"].as_str().unwrap_or("0x0"));
    Ok((pending, queued))
}

#[derive(Debug)]
struct BlockInfo {
    number: u64,
    timestamp: u64,
    tx_count: u64,
    gas_used: u64,
    gas_limit: u64,
}

async fn get_block_info(client: &reqwest::Client, rpc_url: &str, block: u64) -> Result<BlockInfo> {
    let resp = rpc_call(client, rpc_url, "eth_getBlockByNumber",
        serde_json::json!([format!("0x{:x}", block), false])).await?;
    let b = &resp["result"];
    let tx_count = b["transactions"].as_array().map(|a| a.len() as u64).unwrap_or(0);
    Ok(BlockInfo {
        number: block,
        timestamp: parse_hex_u64(b["timestamp"].as_str().unwrap_or("0x0")),
        tx_count,
        gas_used: parse_hex_u64(b["gasUsed"].as_str().unwrap_or("0x0")),
        gas_limit: parse_hex_u64(b["gasLimit"].as_str().unwrap_or("0x0")),
    })
}

/// Parallel nonce sync using concurrent requests
async fn sync_nonces_parallel(
    accounts: &[Arc<TestAccount>],
    rpc_urls: &[String],
    client: &reqwest::Client,
) {
    let start = Instant::now();
    let mut handles = Vec::new();

    for account in accounts.iter() {
        let client = client.clone();
        let rpc_url = rpc_urls[account.rpc_idx].clone();
        let account = account.clone();
        handles.push(tokio::spawn(async move {
            if let Ok(nonce) = get_nonce(&client, &rpc_url, &account.address).await {
                account.nonce.store(nonce, Ordering::Relaxed);
            }
        }));
    }

    for handle in handles {
        let _ = handle.await;
    }

    tracing::info!(
        accounts = accounts.len(),
        elapsed_ms = start.elapsed().as_millis(),
        "Nonces synced (parallel)"
    );
}

/// Resync nonces for a subset of accounts
async fn resync_account_nonces(
    accounts: &[Arc<TestAccount>],
    rpc_url: &str,
    client: &reqwest::Client,
) {
    let mut handles = Vec::new();
    for account in accounts.iter() {
        let client = client.clone();
        let rpc_url = rpc_url.to_string();
        let account = account.clone();
        handles.push(tokio::spawn(async move {
            if let Ok(nonce) = get_nonce(&client, &rpc_url, &account.address).await {
                account.nonce.store(nonce, Ordering::Relaxed);
            }
        }));
    }
    for handle in handles {
        let _ = handle.await;
    }
}

/// Backpressure monitor: polls txpool and sets shared flag.
/// Runs as a background task, does NOT block senders.
fn spawn_backpressure_monitor(
    client: reqwest::Client,
    rpc_url: String,
    bp_active: Arc<AtomicBool>,
    max_pool: u64,
    resume_pool: u64,
    stats: Arc<Stats>,
    stop: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut was_active = false;
        loop {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;

            if let Ok((pending, _)) = get_txpool_status(&client, &rpc_url).await {
                if !was_active && pending > max_pool {
                    bp_active.store(true, Ordering::Relaxed);
                    was_active = true;
                    stats.backpressure_pauses.fetch_add(1, Ordering::Relaxed);
                    tracing::debug!(pending, max_pool, "backpressure: ACTIVE");
                } else if was_active && pending < resume_pool {
                    bp_active.store(false, Ordering::Relaxed);
                    was_active = false;
                    tracing::debug!(pending, resume_pool, "backpressure: released");
                }
            }
        }
    })
}

/// Signed batch ready to send.
struct SignedBatch {
    raw_txs: Vec<String>,
    sign_time: Duration,
    group_start: usize,
    group_end: usize,
}

/// Per-RPC pipelined sender: signer and sender run concurrently via channel.
///
/// v9 architecture:
///   Signer task → [bounded channel (depth=4)] → Sender task → [fire-and-forget HTTP]
///
/// This overlaps signing batch N+1 with sending batch N, roughly doubling throughput.
#[allow(clippy::too_many_arguments)]
async fn sender_loop(
    rpc_idx: usize,
    accounts: Vec<Arc<TestAccount>>,
    targets: Vec<Address>,
    rpc_url: String,
    client: reqwest::Client,
    stats: Arc<Stats>,
    semaphore: Arc<Semaphore>,
    target_tps: u64,
    batch_size: usize,
    accounts_per_batch: usize,
    erc20_ratio: u8,
    duration: Duration,
    bp_active: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) {
    if accounts.is_empty() {
        return;
    }

    let num_accounts = accounts.len();
    let apb = accounts_per_batch.min(num_accounts).max(1);

    // Cap batch size: at most 20 txs per account, and respect per-RPC TPS target
    let actual_batch_size = if target_tps > 0 {
        batch_size
            .min(apb * 20)                              // max 20 txs/account/batch
            .min((target_tps as usize * 2).max(50))     // don't overshoot 2x per-RPC TPS
    } else {
        batch_size.min(apb * 20)
    };

    let num_groups = num_accounts.div_ceil(apb);

    // Pipeline depth: how many pre-signed batches to buffer
    let pipeline_depth = 4usize;

    // Rate limiter: if target_tps > 0, compute min interval between batch sends
    let batch_interval = if target_tps > 0 {
        let batches_per_sec = (target_tps as f64 / actual_batch_size as f64).ceil() as u64;
        Some(Duration::from_secs_f64(1.0 / batches_per_sec.max(1) as f64))
    } else {
        None
    };

    tracing::debug!(
        rpc_idx, num_accounts, apb, actual_batch_size, pipeline_depth,
        batch_interval_ms = batch_interval.map(|d| d.as_millis() as u64).unwrap_or(0),
        "sender_loop v9 (pipelined) started"
    );

    // Channel between signer and sender
    let (batch_tx, mut batch_rx) = tokio::sync::mpsc::channel::<SignedBatch>(pipeline_depth);

    // === Signer task: continuously signs batches and pushes to channel ===
    let signer_accounts = accounts.clone();
    let signer_targets = targets;
    let signer_stop = stop.clone();
    let signer_bp = bp_active.clone();
    let signer_stats = stats.clone();
    let signer_handle = tokio::spawn(async move {
        let start = Instant::now();
        let mut group_idx: usize = 0;

        while start.elapsed() < duration && !signer_stop.load(Ordering::Relaxed) {
            // Respect backpressure
            if signer_bp.load(Ordering::Relaxed) {
                let pause_start = Instant::now();
                tokio::time::sleep(Duration::from_millis(50)).await;
                signer_stats.backpressure_ms.fetch_add(
                    pause_start.elapsed().as_millis() as u64,
                    Ordering::Relaxed,
                );
                continue;
            }

            let group_start = (group_idx % num_groups) * apb;
            let group_end = (group_start + apb).min(num_accounts);
            let group: Vec<Arc<TestAccount>> = signer_accounts[group_start..group_end].to_vec();

            let targets_clone = signer_targets.clone();
            let bs = actual_batch_size;
            let gl = group.len();
            let er = erc20_ratio;
            let sign_result = tokio::task::spawn_blocking(move || {
                sign_mixed_batch(&group, &targets_clone, bs, gl, er)
            }).await;

            let (raw_txs, sign_time, _nonce_info) = match sign_result {
                Ok(r) => r,
                Err(_) => continue,
            };

            signer_stats.sign_time_ns.fetch_add(sign_time.as_nanos() as u64, Ordering::Relaxed);
            signer_stats.sign_count.fetch_add(raw_txs.len() as u64, Ordering::Relaxed);

            let batch = SignedBatch { raw_txs, sign_time, group_start, group_end };
            if batch_tx.send(batch).await.is_err() {
                break; // receiver dropped
            }

            group_idx += 1;
        }
    });

    // === Sender task: drains channel and fires off HTTP requests ===
    let mut last_send = Instant::now();
    while let Some(batch) = batch_rx.recv().await {
        if stop.load(Ordering::Relaxed) {
            break;
        }

        // Rate limiting: ensure minimum interval between sends
        if let Some(interval) = batch_interval {
            let since_last = last_send.elapsed();
            if since_last < interval {
                tokio::time::sleep(interval - since_last).await;
            }
        }
        last_send = Instant::now();

        let permit = semaphore.clone().acquire_owned().await.unwrap();
        let client_clone = client.clone();
        let stats_clone = stats.clone();
        let rpc_url_clone = rpc_url.clone();
        let resync_accounts: Vec<Arc<TestAccount>> =
            accounts[batch.group_start..batch.group_end].to_vec();

        tokio::spawn(async move {
            let result = send_batch(&client_clone, &rpc_url_clone, &batch.raw_txs).await;
            stats_clone.sent.fetch_add(result.ok as u64, Ordering::Relaxed);
            stats_clone.rpc_errors.fetch_add(result.rpc_error as u64, Ordering::Relaxed);
            stats_clone.http_errors.fetch_add(result.http_error as u64, Ordering::Relaxed);
            stats_clone.rpc_latency_ns.fetch_add(result.latency.as_nanos() as u64, Ordering::Relaxed);
            stats_clone.rpc_latency_count.fetch_add(1, Ordering::Relaxed);

            if result.needs_resync {
                stats_clone.nonce_resyncs.fetch_add(1, Ordering::Relaxed);
                resync_account_nonces(&resync_accounts, &rpc_url_clone, &client_clone).await;
            }

            drop(permit);
        });
    }

    signer_handle.abort();
}

/// Pre-sign all transactions into memory, grouped by RPC endpoint.
/// Returns Vec[rpc_idx] = Vec<raw_tx_hex>.
fn presign_all(
    accounts: &[Arc<TestAccount>],
    targets: &[Address],
    total: u64,
    num_rpcs: usize,
    erc20_ratio: u8,
) -> Vec<Vec<String>> {
    let num_accounts = accounts.len();
    let txs_per_account = (total as usize) / num_accounts;
    let remainder = (total as usize) % num_accounts;

    tracing::info!(
        total, num_accounts, txs_per_account,
        remainder, num_rpcs, erc20_ratio,
        "Pre-signing transactions..."
    );

    let start = Instant::now();

    // Parallel signing using std::thread::scope (CPU-bound work)
    let per_account_results: Vec<(usize, Vec<String>)> = std::thread::scope(|s| {
        let handles: Vec<_> = accounts.iter().enumerate().map(|(acct_idx, account)| {
            let targets = targets;
            s.spawn(move || {
                let count = txs_per_account + if acct_idx < remainder { 1 } else { 0 };
                if count == 0 {
                    return (account.rpc_idx, Vec::new());
                }
                let nonce_start = account.nonce.fetch_add(count as u64, Ordering::Relaxed);
                let mut txs = Vec::with_capacity(count);
                for j in 0..count {
                    let nonce = nonce_start + j as u64;
                    let tx_index = acct_idx * txs_per_account + j;
                    let is_contract_call = erc20_ratio > 0 && (tx_index % 100) < erc20_ratio as usize;

                    let tx = if is_contract_call {
                        TxEip1559 {
                            chain_id: CHAIN_ID,
                            nonce,
                            gas_limit: CONTRACT_CALL_GAS,
                            max_fee_per_gas: MAX_FEE_PER_GAS,
                            max_priority_fee_per_gas: MAX_PRIORITY_FEE,
                            to: TxKind::Call(STRESS_CONTRACT),
                            value: U256::ZERO,
                            input: Bytes::new(),
                            access_list: Default::default(),
                        }
                    } else {
                        let to = targets[(nonce as usize) % targets.len()];
                        TxEip1559 {
                            chain_id: CHAIN_ID,
                            nonce,
                            gas_limit: TRANSFER_GAS,
                            max_fee_per_gas: MAX_FEE_PER_GAS,
                            max_priority_fee_per_gas: MAX_PRIORITY_FEE,
                            to: TxKind::Call(to),
                            value: U256::from(TRANSFER_VALUE),
                            input: Bytes::new(),
                            access_list: Default::default(),
                        }
                    };
                    let sig_hash = tx.signature_hash();
                    let sig = account.signer.sign_hash_sync(&sig_hash).expect("sign");
                    let signed = tx.into_signed(sig);
                    let mut buf = Vec::with_capacity(128);
                    signed.encode_2718(&mut buf);
                    txs.push(format!("0x{}", hex::encode(&buf)));
                }
                (account.rpc_idx, txs)
            })
        }).collect();

        handles.into_iter().map(|h| h.join().expect("thread join")).collect()
    });

    // Group by RPC endpoint
    let mut grouped: Vec<Vec<String>> = vec![Vec::new(); num_rpcs];
    let mut total_signed = 0usize;
    for (rpc_idx, txs) in per_account_results {
        total_signed += txs.len();
        grouped[rpc_idx].extend(txs);
    }

    let elapsed = start.elapsed();
    let sign_rate = total_signed as f64 / elapsed.as_secs_f64();
    tracing::info!(
        total_signed,
        elapsed_ms = elapsed.as_millis(),
        sign_rate = format!("{:.0}/s", sign_rate),
        per_rpc = grouped.iter().map(|g| g.len()).collect::<Vec<_>>().as_slice().iter()
            .map(|n| n.to_string()).collect::<Vec<_>>().join(","),
        "Pre-sign complete"
    );

    grouped
}

/// Binary file format for pre-signed transactions:
/// Header: b"N42T" (4) + version (1) + chain_id (8) + num_rpcs (4) + total_txs (8) = 25 bytes
/// Per-RPC: tx_count (8 bytes), then for each tx: tx_len (2 bytes) + tx_bytes
const FILE_MAGIC: &[u8; 4] = b"N42T";
const FILE_VERSION: u8 = 2;

/// Raw EIP-2718 encoded transaction bytes paired with 20-byte sender address.
type RawTxWithSender = (Vec<u8>, [u8; 20]);

/// Pre-sign transactions and save to binary file (raw RLP, not hex).
fn presign_and_save(
    accounts: &[Arc<TestAccount>],
    targets: &[Address],
    total: u64,
    num_rpcs: usize,
    erc20_ratio: u8,
    path: &str,
) -> Result<()> {
    let num_accounts = accounts.len();
    let txs_per_account = (total as usize) / num_accounts;
    let remainder = (total as usize) % num_accounts;

    tracing::info!(
        total, num_accounts, txs_per_account, remainder, num_rpcs, path,
        "Pre-signing and saving to file..."
    );

    let start = Instant::now();

    // Parallel signing — returns raw RLP bytes (not hex) grouped by rpc_idx
    let per_account_results: Vec<(usize, Vec<(Vec<u8>, Address)>)> = std::thread::scope(|s| {
        let handles: Vec<_> = accounts.iter().enumerate().map(|(acct_idx, account)| {
            let targets = targets;
            s.spawn(move || {
                let count = txs_per_account + if acct_idx < remainder { 1 } else { 0 };
                if count == 0 {
                    return (account.rpc_idx, Vec::new());
                }
                let nonce_start = account.nonce.fetch_add(count as u64, Ordering::Relaxed);
                let mut txs = Vec::with_capacity(count);
                for j in 0..count {
                    let nonce = nonce_start + j as u64;
                    let tx_index = acct_idx * txs_per_account + j;
                    let is_contract_call = erc20_ratio > 0 && (tx_index % 100) < erc20_ratio as usize;

                    let tx = if is_contract_call {
                        TxEip1559 {
                            chain_id: CHAIN_ID,
                            nonce,
                            gas_limit: CONTRACT_CALL_GAS,
                            max_fee_per_gas: MAX_FEE_PER_GAS,
                            max_priority_fee_per_gas: MAX_PRIORITY_FEE,
                            to: TxKind::Call(STRESS_CONTRACT),
                            value: U256::ZERO,
                            input: Bytes::new(),
                            access_list: Default::default(),
                        }
                    } else {
                        let to = targets[(nonce as usize) % targets.len()];
                        TxEip1559 {
                            chain_id: CHAIN_ID,
                            nonce,
                            gas_limit: TRANSFER_GAS,
                            max_fee_per_gas: MAX_FEE_PER_GAS,
                            max_priority_fee_per_gas: MAX_PRIORITY_FEE,
                            to: TxKind::Call(to),
                            value: U256::from(TRANSFER_VALUE),
                            input: Bytes::new(),
                            access_list: Default::default(),
                        }
                    };
                    let sig_hash = tx.signature_hash();
                    let sig = account.signer.sign_hash_sync(&sig_hash).expect("sign");
                    let signed = tx.into_signed(sig);
                    let mut buf = Vec::with_capacity(128);
                    signed.encode_2718(&mut buf);
                    txs.push((buf, account.address));
                }
                (account.rpc_idx, txs)
            })
        }).collect();

        handles.into_iter().map(|h| h.join().expect("thread join")).collect()
    });

    // Group by RPC: each entry is (tx_bytes, sender_address)
    let mut grouped: Vec<Vec<(Vec<u8>, Address)>> = vec![Vec::new(); num_rpcs];
    let mut total_signed = 0usize;
    for (rpc_idx, txs) in per_account_results {
        total_signed += txs.len();
        grouped[rpc_idx].extend(txs);
    }

    let sign_elapsed = start.elapsed();
    let sign_rate = total_signed as f64 / sign_elapsed.as_secs_f64();
    tracing::info!(
        total_signed,
        sign_ms = sign_elapsed.as_millis(),
        sign_rate = format!("{:.0}/s", sign_rate),
        "Signing complete, writing file..."
    );

    // Write binary file
    let file = std::fs::File::create(path)?;
    let mut w = BufWriter::with_capacity(8 * 1024 * 1024, file);

    // Header
    w.write_all(FILE_MAGIC)?;
    w.write_all(&[FILE_VERSION])?;
    w.write_all(&CHAIN_ID.to_le_bytes())?;
    w.write_all(&(num_rpcs as u32).to_le_bytes())?;
    w.write_all(&(total_signed as u64).to_le_bytes())?;

    // Per-RPC tx data: v2 format includes 20-byte sender after each tx
    let mut total_bytes = 25u64; // header size
    for rpc_txs in &grouped {
        w.write_all(&(rpc_txs.len() as u64).to_le_bytes())?;
        total_bytes += 8;
        for (tx_bytes, sender) in rpc_txs {
            let len = tx_bytes.len() as u16;
            w.write_all(&len.to_le_bytes())?;
            w.write_all(tx_bytes)?;
            w.write_all(sender.as_slice())?;
            total_bytes += 2 + tx_bytes.len() as u64 + 20;
        }
    }
    w.flush()?;

    let total_elapsed = start.elapsed();
    tracing::info!(
        path,
        total_txs = total_signed,
        file_size_mb = total_bytes / (1024 * 1024),
        sign_ms = sign_elapsed.as_millis(),
        write_ms = (total_elapsed - sign_elapsed).as_millis(),
        total_ms = total_elapsed.as_millis(),
        per_rpc = grouped.iter().map(|g| g.len().to_string()).collect::<Vec<_>>().join(","),
        "Pre-signed transactions saved"
    );

    Ok(())
}

/// Load pre-signed transactions from binary file → Vec[rpc_idx] = Vec<hex_string>.
fn load_presigned(path: &str) -> Result<Vec<Vec<String>>> {
    let start = Instant::now();
    let file = std::fs::File::open(path)?;
    let file_size = file.metadata()?.len();
    let mut r = BufReader::with_capacity(8 * 1024 * 1024, file);

    // Read header
    let mut magic = [0u8; 4];
    r.read_exact(&mut magic)?;
    if &magic != FILE_MAGIC {
        eyre::bail!("Invalid file magic: expected N42T, got {:?}", magic);
    }

    let mut version = [0u8; 1];
    r.read_exact(&mut version)?;
    let file_version = version[0];
    if file_version != 1 && file_version != 2 {
        eyre::bail!("Unsupported file version: {}", file_version);
    }

    let mut chain_id_buf = [0u8; 8];
    r.read_exact(&mut chain_id_buf)?;
    let chain_id = u64::from_le_bytes(chain_id_buf);
    if chain_id != CHAIN_ID {
        eyre::bail!("Chain ID mismatch: file={}, expected={}", chain_id, CHAIN_ID);
    }

    let mut num_rpcs_buf = [0u8; 4];
    r.read_exact(&mut num_rpcs_buf)?;
    let num_rpcs = u32::from_le_bytes(num_rpcs_buf) as usize;

    let mut total_txs_buf = [0u8; 8];
    r.read_exact(&mut total_txs_buf)?;
    let total_txs = u64::from_le_bytes(total_txs_buf);

    tracing::info!(
        path, file_size_mb = file_size / (1024 * 1024),
        num_rpcs, total_txs, file_version,
        "Loading pre-signed transactions..."
    );

    // Read per-RPC tx data (v2 has 20-byte sender after each tx, skip it for hex mode)
    let mut grouped: Vec<Vec<String>> = Vec::with_capacity(num_rpcs);
    let mut loaded = 0u64;
    let mut tx_buf = vec![0u8; 65536]; // reusable buffer

    for rpc_idx in 0..num_rpcs {
        let mut count_buf = [0u8; 8];
        r.read_exact(&mut count_buf)?;
        let tx_count = u64::from_le_bytes(count_buf) as usize;

        let mut txs = Vec::with_capacity(tx_count);
        for _ in 0..tx_count {
            let mut len_buf = [0u8; 2];
            r.read_exact(&mut len_buf)?;
            let tx_len = u16::from_le_bytes(len_buf) as usize;
            r.read_exact(&mut tx_buf[..tx_len])?;
            if file_version >= 2 {
                // Skip 20-byte sender in v2 format (not needed for RPC hex mode)
                let mut sender_skip = [0u8; 20];
                r.read_exact(&mut sender_skip)?;
            }
            txs.push(format!("0x{}", hex::encode(&tx_buf[..tx_len])));
            loaded += 1;
        }

        if rpc_idx == 0 || rpc_idx == num_rpcs - 1 {
            tracing::info!(rpc_idx, txs = txs.len(), "RPC group loaded");
        }
        grouped.push(txs);
    }

    let elapsed = start.elapsed();
    let load_rate = loaded as f64 / elapsed.as_secs_f64();
    tracing::info!(
        total_loaded = loaded,
        elapsed_ms = elapsed.as_millis(),
        load_rate = format!("{:.0}/s", load_rate),
        mem_estimate_mb = loaded * 240 / (1024 * 1024),
        "Pre-signed transactions loaded"
    );

    Ok(grouped)
}

/// Load pre-signed transactions as raw binary bytes with sender addresses (v2 format).
/// Returns Vec[rpc_idx] = Vec<(tx_bytes, sender_20bytes)>.
fn load_presigned_binary(path: &str, num_endpoints: usize) -> Result<Vec<Vec<RawTxWithSender>>> {
    let start = Instant::now();
    let file = std::fs::File::open(path)?;
    let file_size = file.metadata()?.len();
    let mut r = BufReader::with_capacity(8 * 1024 * 1024, file);

    // Read header
    let mut magic = [0u8; 4];
    r.read_exact(&mut magic)?;
    if &magic != FILE_MAGIC {
        eyre::bail!("Invalid file magic");
    }

    let mut version = [0u8; 1];
    r.read_exact(&mut version)?;
    if version[0] != FILE_VERSION {
        eyre::bail!("Unsupported file version: {} (expected {})", version[0], FILE_VERSION);
    }

    let mut chain_id_buf = [0u8; 8];
    r.read_exact(&mut chain_id_buf)?;
    let chain_id = u64::from_le_bytes(chain_id_buf);
    if chain_id != CHAIN_ID {
        eyre::bail!("Chain ID mismatch: file={}, expected={}", chain_id, CHAIN_ID);
    }

    let mut num_rpcs_buf = [0u8; 4];
    r.read_exact(&mut num_rpcs_buf)?;
    let file_num_rpcs = u32::from_le_bytes(num_rpcs_buf) as usize;

    let mut total_txs_buf = [0u8; 8];
    r.read_exact(&mut total_txs_buf)?;
    let total_txs = u64::from_le_bytes(total_txs_buf);

    tracing::info!(
        path, file_size_mb = file_size / (1024 * 1024),
        file_num_rpcs, total_txs, num_endpoints,
        "Loading pre-signed transactions (binary v2 with sender)..."
    );

    // Read per-RPC tx data with sender, preserving nonce ordering per account.
    let mut file_groups: Vec<Vec<RawTxWithSender>> = Vec::with_capacity(file_num_rpcs);
    let mut tx_buf = vec![0u8; 65536];

    for _ in 0..file_num_rpcs {
        let mut count_buf = [0u8; 8];
        r.read_exact(&mut count_buf)?;
        let tx_count = u64::from_le_bytes(count_buf) as usize;

        let mut txs = Vec::with_capacity(tx_count);
        for _ in 0..tx_count {
            let mut len_buf = [0u8; 2];
            r.read_exact(&mut len_buf)?;
            let tx_len = u16::from_le_bytes(len_buf) as usize;
            r.read_exact(&mut tx_buf[..tx_len])?;
            let mut sender = [0u8; 20];
            r.read_exact(&mut sender)?;
            txs.push((tx_buf[..tx_len].to_vec(), sender));
        }
        file_groups.push(txs);
    }

    // Map file RPC groups to inject endpoints.
    // When file has fewer groups than endpoints, redistribute by sender address
    // to ensure each account's txs stay on the same endpoint (preserving nonce order).
    let mut grouped: Vec<Vec<RawTxWithSender>> = vec![Vec::new(); num_endpoints];
    if file_groups.len() >= num_endpoints {
        // File has enough groups: map groups directly to endpoints
        for (i, txs) in file_groups.into_iter().enumerate() {
            grouped[i % num_endpoints].extend(txs);
        }
    } else {
        // File has fewer groups (e.g., 1 RPC): redistribute by sender
        // Group txs by sender, then assign each sender's batch to an endpoint
        let all_txs: Vec<RawTxWithSender> = file_groups.into_iter().flatten().collect();
        let mut sender_groups: std::collections::BTreeMap<[u8; 20], Vec<RawTxWithSender>> =
            std::collections::BTreeMap::new();
        for tx in all_txs {
            sender_groups.entry(tx.1).or_default().push(tx);
        }
        // Distribute senders round-robin across endpoints
        for (i, (_sender, txs)) in sender_groups.into_iter().enumerate() {
            grouped[i % num_endpoints].extend(txs);
        }
        tracing::info!(
            "Redistributed single-group presign across {} endpoints by sender",
            num_endpoints
        );
    }

    let elapsed = start.elapsed();
    let loaded = grouped.iter().map(|g| g.len()).sum::<usize>();
    tracing::info!(
        total_loaded = loaded,
        elapsed_ms = elapsed.as_millis(),
        per_endpoint = grouped.iter().map(|g| g.len().to_string()).collect::<Vec<_>>().join(","),
        "Binary v2 load complete (with sender)"
    );

    Ok(grouped)
}

/// Binary TCP injection mode: stream raw txs with sender over TCP to node inject servers.
#[allow(clippy::too_many_arguments)]
async fn run_inject_mode(
    endpoints: &[String],
    presigned: Vec<Vec<RawTxWithSender>>,
    batch_size: usize,
    target_tps: u64,
    stats: &Arc<Stats>,
    rpc_urls: &[String],
    client: &reqwest::Client,
    start_block: u64,
    duration_secs: u64,
) {
    let start = Instant::now();
    let stop = Arc::new(AtomicBool::new(false));

    // Monitor using RPC (injection via TCP, monitoring via JSON-RPC)
    let monitor_client = client.clone();
    let monitor_rpc = rpc_urls[0].clone();
    let monitor_stats = stats.clone();
    let monitor_stop = stop.clone();
    let monitor_handle = tokio::spawn(async move {
        let mut last_block = start_block;
        let mut last_time = Instant::now();
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        interval.tick().await;
        loop {
            interval.tick().await;
            if monitor_stop.load(Ordering::Relaxed) {
                break;
            }
            let now_block = get_block_number(&monitor_client, &monitor_rpc).await.unwrap_or(last_block);
            if now_block > last_block {
                let mut recent_txs = 0u64;
                let check_from = if now_block > last_block + 5 { now_block - 5 } else { last_block };
                for b in (check_from + 1)..=now_block {
                    if let Ok(info) = get_block_info(&monitor_client, &monitor_rpc, b).await {
                        recent_txs += info.tx_count;
                    }
                }
                let dt = last_time.elapsed().as_secs_f64();
                let sent = monitor_stats.sent.load(Ordering::Relaxed);
                let pool = get_txpool_status(&monitor_client, &monitor_rpc).await.unwrap_or((0, 0));
                tracing::info!(
                    blocks = now_block - start_block,
                    recent_block_txs = recent_txs,
                    block_tps = format!("{:.0}", recent_txs as f64 / dt.max(0.1)),
                    inject_sent = sent,
                    inject_err = monitor_stats.rpc_errors.load(Ordering::Relaxed),
                    pool_pending = pool.0,
                    "INJECT_MONITOR"
                );
                last_block = now_block;
                last_time = Instant::now();
            }
        }
    });

    // Duration guard
    let dur_stop = stop.clone();
    let dur_guard = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(duration_secs)).await;
        dur_stop.store(true, Ordering::Relaxed);
        tracing::info!(duration_secs, "Inject duration reached");
    });

    // Spawn one TCP sender per endpoint with dynamic rate control
    let mut handles = Vec::new();
    for (idx, txs) in presigned.into_iter().enumerate() {
        if txs.is_empty() {
            continue;
        }
        let endpoint = endpoints[idx % endpoints.len()].clone();
        let stats = stats.clone();
        let stop = stop.clone();
        let bs = batch_size;
        let num_endpoints = endpoints.len().max(1) as u64;
        let per_ep_tps = if target_tps > 0 { (target_tps / num_endpoints).max(1) } else { 0 };

        handles.push(tokio::spawn(async move {
            // Connect to inject server
            let mut stream = match TcpStream::connect(&endpoint).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(endpoint, error = %e, "failed to connect to inject server");
                    return;
                }
            };
            let _ = stream.set_nodelay(true);
            tracing::info!(endpoint, txs = txs.len(), batch_size = bs, per_ep_tps, "TCP inject connected");

            // Rate limiter: compute interval between batches
            let batch_interval = if per_ep_tps > 0 {
                let batches_per_sec = (per_ep_tps as f64 / bs as f64).ceil() as u64;
                Duration::from_micros(1_000_000 / batches_per_sec.max(1))
            } else {
                Duration::ZERO
            };

            // Dynamic rate control thresholds
            const POOL_LOW: u64 = 50_000;   // below: full speed
            const POOL_HIGH: u64 = 80_000;  // above: pause until LOW
            let mut pool_gated = false;
            let mut gate_wait_ms = 0u64;

            // Index-based iteration for retry support
            let chunks: Vec<&[RawTxWithSender]> = txs.chunks(bs).collect();
            let mut chunk_idx = 0;

            while chunk_idx < chunks.len() {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let chunk = chunks[chunk_idx];
                let batch_start = Instant::now();

                // Build binary batch: [u32 num_txs] [u16 tx_len, tx_bytes, 20-byte sender] × n
                let num_txs = chunk.len() as u32;
                let total_size: usize = 4 + chunk.iter().map(|(tx, _)| 2 + tx.len() + 20).sum::<usize>();
                let mut buf = Vec::with_capacity(total_size);
                buf.extend_from_slice(&num_txs.to_le_bytes());
                for (tx, sender) in chunk {
                    buf.extend_from_slice(&(tx.len() as u16).to_le_bytes());
                    buf.extend_from_slice(tx);
                    buf.extend_from_slice(sender);
                }

                // Send batch
                if let Err(e) = stream.write_all(&buf).await {
                    stats.rpc_errors.fetch_add(chunk.len() as u64, Ordering::Relaxed);
                    tracing::warn!(endpoint, error = %e, "TCP write failed");
                    break;
                }

                // Read ACK v3: [u32 accepted][u32 pool_pending]
                // Falls back to v2 (4-byte ACK) if server doesn't send pool hint.
                let mut ack = [0u8; 8];
                match stream.read_exact(&mut ack).await {
                    Ok(_) => {
                        let accepted = u32::from_le_bytes(ack[0..4].try_into().unwrap()) as u64;
                        let pool_pending = u32::from_le_bytes(ack[4..8].try_into().unwrap()) as u64;

                        if accepted == 0 && pool_pending > 0 {
                            // Server rejected: pool gate active. Retry same batch.
                            if !pool_gated {
                                tracing::info!(endpoint, pool_pending, "pool gate: server rejected batch, waiting...");
                                pool_gated = true;
                            }
                            gate_wait_ms += 200;
                            tokio::time::sleep(Duration::from_millis(200)).await;
                            continue; // retry same chunk_idx
                        }

                        if pool_gated {
                            tracing::info!(endpoint, pool_pending, gate_wait_ms, "pool gate: resumed injection");
                            pool_gated = false;
                            gate_wait_ms = 0;
                        }

                        stats.sent.fetch_add(accepted, Ordering::Relaxed);
                        let rejected = chunk.len() as u64 - accepted;
                        if rejected > 0 {
                            stats.rpc_errors.fetch_add(rejected, Ordering::Relaxed);
                        }

                        // Dynamic rate: if pool is getting full, apply rate limiting
                        if pool_pending > POOL_HIGH {
                            // Wait until pool drops (server will gate, but add client delay too)
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        } else if pool_pending > POOL_LOW && !batch_interval.is_zero() {
                            // Normal rate limiting
                            let elapsed = batch_start.elapsed();
                            if elapsed < batch_interval {
                                tokio::time::sleep(batch_interval - elapsed).await;
                            }
                        }
                        // pool < POOL_LOW: full speed, no sleep
                    }
                    Err(e) => {
                        tracing::warn!(endpoint, error = %e, "TCP ACK read failed");
                        break;
                    }
                }

                chunk_idx += 1;
            }

            // Send close signal
            let _ = stream.write_all(&0u32.to_le_bytes()).await;
            tracing::info!(endpoint, "TCP inject sender done");
        }));
    }

    for h in handles {
        let _ = h.await;
    }

    stop.store(true, Ordering::Relaxed);
    dur_guard.abort();
    monitor_handle.abort();

    let sent = stats.sent.load(Ordering::Relaxed);
    let elapsed = start.elapsed().as_secs_f64();
    tracing::info!(
        injection_tps = format!("{:.0}", sent as f64 / elapsed.max(1.0)),
        sent,
        errors = stats.rpc_errors.load(Ordering::Relaxed),
        duration = format!("{:.1}s", elapsed),
        "TCP injection complete"
    );
}

/// Send a wave of txs over a single TCP stream. Returns (accepted, errors).
async fn tcp_send_wave(
    stream: &mut TcpStream,
    txs: &[RawTxWithSender],
    batch_size: usize,
) -> (u64, u64) {
    let mut total_ok = 0u64;
    let mut total_err = 0u64;

    for chunk in txs.chunks(batch_size) {
        // Build binary batch: [u32 num_txs] [u16 tx_len, tx_bytes, 20-byte sender] × n
        let num_txs = chunk.len() as u32;
        let total_size: usize = 4 + chunk.iter().map(|(tx, _)| 2 + tx.len() + 20).sum::<usize>();
        let mut buf = Vec::with_capacity(total_size);
        buf.extend_from_slice(&num_txs.to_le_bytes());
        for (tx, sender) in chunk {
            buf.extend_from_slice(&(tx.len() as u16).to_le_bytes());
            buf.extend_from_slice(tx);
            buf.extend_from_slice(sender);
        }

        if let Err(e) = stream.write_all(&buf).await {
            total_err += chunk.len() as u64;
            tracing::warn!(error = %e, "TCP write failed in wave");
            break;
        }

        // Read ACK
        let mut ack = [0u8; 4];
        match stream.read_exact(&mut ack).await {
            Ok(_) => {
                let accepted = u32::from_le_bytes(ack) as u64;
                total_ok += accepted;
                let rejected = chunk.len() as u64 - accepted;
                if rejected > 0 {
                    total_err += rejected;
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "TCP ACK read failed in wave");
                total_err += chunk.len() as u64;
                break;
            }
        }
    }

    (total_ok, total_err)
}

/// Synchronized TCP injection: inject exactly `wave_cap` txs per wave,
/// wait for pool to drain, then inject next wave.
///
/// When N42_DISABLE_TX_FORWARD=1: runs per-node independent async loops.
/// Each node independently: inject cap → poll own pool → drain → inject next.
/// This ensures each leader always has cap txs ready, with zero idle time.
///
/// When TX Forward enabled: global wave mode (inject cap/N to each, wait for all drain).
#[allow(clippy::too_many_arguments)]
async fn run_sync_inject_mode(
    endpoints: &[String],
    presigned: Vec<Vec<RawTxWithSender>>,
    batch_size: usize,
    wave_cap: usize,
    rpc_urls: &[String],
    client: &reqwest::Client,
    start_block: u64,
    duration_secs: u64,
) {
    let num_eps = endpoints.len();
    let disable_forward = std::env::var("N42_DISABLE_TX_FORWARD").map(|v| v == "1").unwrap_or(false);
    let start = Instant::now();
    let deadline = Duration::from_secs(duration_secs);
    let stop = Arc::new(AtomicBool::new(false));

    if disable_forward {
        // === Per-node independent injection mode ===
        // Each node gets its own async loop: inject cap → poll own pool → inject next.
        tracing::info!(
            wave_cap, num_eps, batch_size, per_node_cap = wave_cap,
            "SYNC_INJECT per-node mode (TX Forward OFF)"
        );

        // Shared stats for aggregation
        let global_sent = Arc::new(AtomicU64::new(0));
        let global_err = Arc::new(AtomicU64::new(0));
        let global_waves = Arc::new(AtomicU64::new(0));
        let global_drain_ms = Arc::new(AtomicU64::new(0));

        // Duration guard
        let dur_stop = stop.clone();
        let dur_guard = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(duration_secs)).await;
            dur_stop.store(true, Ordering::Relaxed);
        });

        // Convert presigned into per-endpoint owned data
        let mut ep_data: Vec<(String, String, Vec<RawTxWithSender>)> = Vec::new();
        for (idx, txs) in presigned.into_iter().enumerate() {
            if idx < num_eps {
                ep_data.push((
                    endpoints[idx].clone(),
                    rpc_urls[idx].clone(),
                    txs,
                ));
            }
        }

        let mut handles = Vec::new();
        for (endpoint, rpc_url, txs) in ep_data {
            let stop = stop.clone();
            let client = client.clone();
            let g_sent = global_sent.clone();
            let g_err = global_err.clone();
            let g_waves = global_waves.clone();
            let g_drain = global_drain_ms.clone();
            let bs = batch_size;
            let cap = wave_cap;

            handles.push(tokio::spawn(async move {
                // Connect TCP
                let mut stream = match TcpStream::connect(&endpoint).await {
                    Ok(s) => { let _ = s.set_nodelay(true); s }
                    Err(e) => {
                        tracing::error!(endpoint, error = %e, "per-node TCP connect failed");
                        return;
                    }
                };
                tracing::info!(endpoint, txs = txs.len(), cap, "per-node inject started");

                let mut cursor = 0usize;

                // Wait for this node's pool to be ready
                for _ in 0..50 {
                    if let Ok((pending, _)) = get_txpool_status(&client, &rpc_url).await && pending < 500 {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }

                loop {
                    if stop.load(Ordering::Relaxed) { break; }

                    // Check remaining txs
                    let remaining = txs.len() - cursor;
                    let take = remaining.min(cap);
                    if take == 0 {
                        tracing::info!(endpoint, "node txs exhausted");
                        break;
                    }

                    // Inject cap txs
                    let inject_start = Instant::now();
                    let slice = &txs[cursor..cursor + take];
                    let (ok, err) = tcp_send_wave(&mut stream, slice, bs).await;
                    let inject_ms = inject_start.elapsed().as_millis() as u64;
                    cursor += take;
                    g_sent.fetch_add(ok, Ordering::Relaxed);
                    g_err.fetch_add(err, Ordering::Relaxed);

                    // Wait for this node's pool to drain
                    let drain_start = Instant::now();
                    loop {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                        if stop.load(Ordering::Relaxed) { break; }
                        if let Ok((pending, _)) = get_txpool_status(&client, &rpc_url).await && pending < 500 {
                            break;
                        }
                        if drain_start.elapsed() > Duration::from_secs(30) {
                            tracing::warn!(endpoint, "per-node drain timeout");
                            break;
                        }
                    }
                    let drain_ms = drain_start.elapsed().as_millis() as u64;
                    let wave = g_waves.fetch_add(1, Ordering::Relaxed) + 1;
                    g_drain.fetch_add(drain_ms, Ordering::Relaxed);

                    let tps = ok as f64 / (drain_ms as f64 / 1000.0).max(0.001);
                    tracing::info!(
                        wave, node = endpoint,
                        txs = ok, err,
                        inject_ms, drain_ms,
                        view_tps = format!("{:.0}", tps),
                        "NODE_WAVE"
                    );
                }

                // Send close signal
                let _ = stream.write_all(&0u32.to_le_bytes()).await;
            }));
        }

        // Wait for all per-node tasks
        for h in handles {
            let _ = h.await;
        }
        dur_guard.abort();

        // Summary
        let elapsed = start.elapsed().as_secs_f64();
        let sent = global_sent.load(Ordering::Relaxed);
        let errors = global_err.load(Ordering::Relaxed);
        let waves = global_waves.load(Ordering::Relaxed);
        let drain_total = global_drain_ms.load(Ordering::Relaxed);

        tracing::info!("=== SYNC INJECT RESULT (per-node) ===");
        tracing::info!(
            waves,
            total_txs = sent,
            total_errors = errors,
            elapsed = format!("{:.1}s", elapsed),
            total_drain_ms = drain_total,
            avg_drain_ms = drain_total / waves.max(1),
            sustained_tps = format!("{:.0}", sent as f64 / (drain_total as f64 / 1000.0).max(0.001)),
            wall_tps = format!("{:.0}", sent as f64 / elapsed.max(1.0)),
            per_node_tps = format!("{:.0}", sent as f64 / elapsed.max(1.0) / num_eps as f64),
            "Complete"
        );
        return;
    }

    // === Global wave mode (TX Forward ON) ===
    let per_ep_cap = wave_cap / num_eps;

    // Establish persistent TCP connections to all endpoints
    let mut streams: Vec<Option<TcpStream>> = Vec::with_capacity(num_eps);
    for ep in endpoints {
        match TcpStream::connect(ep).await {
            Ok(s) => {
                let _ = s.set_nodelay(true);
                tracing::info!(endpoint = ep, "sync-inject TCP connected");
                streams.push(Some(s));
            }
            Err(e) => {
                tracing::error!(endpoint = ep, error = %e, "sync-inject TCP connect failed");
                streams.push(None);
            }
        }
    }

    // Per-endpoint cursors
    let mut cursors: Vec<usize> = vec![0; num_eps];
    let mut wave_times: Vec<(u64, u64, u64)> = Vec::new();
    let mut wave_num = 0u64;
    let mut total_sent = 0u64;
    let mut total_err = 0u64;

    // Wait for pool to be empty before first wave
    for _ in 0..50 {
        if let Ok((pending, _)) = get_txpool_status(client, &rpc_urls[0]).await && pending < 500 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    tracing::info!(
        wave_cap, per_ep_cap, num_eps, batch_size,
        "SYNC_INJECT global wave mode (TX Forward ON)"
    );

    loop {
        if start.elapsed() >= deadline { break; }

        let mut wave_txs = 0usize;
        for (idx, cursor) in cursors.iter().enumerate() {
            let remaining = presigned[idx].len() - cursor;
            wave_txs += remaining.min(per_ep_cap);
        }
        if wave_txs == 0 {
            tracing::info!("all presigned txs exhausted");
            break;
        }

        let inject_start = Instant::now();
        let mut ep_ranges: Vec<(usize, usize)> = Vec::new();

        for idx in 0..num_eps {
            let cursor = cursors[idx];
            let remaining = presigned[idx].len() - cursor;
            let take = remaining.min(per_ep_cap);
            ep_ranges.push((cursor, cursor + take));
        }

        let mut wave_ok = 0u64;
        let mut wave_err = 0u64;

        for idx in 0..num_eps {
            let (start_cursor, end_cursor) = ep_ranges[idx];
            let take = end_cursor - start_cursor;
            if take == 0 { continue; }

            if let Some(ref mut stream) = streams[idx] {
                let slice = &presigned[idx][start_cursor..end_cursor];
                let (ok, err) = tcp_send_wave(stream, slice, batch_size).await;
                wave_ok += ok;
                wave_err += err;
            }
            cursors[idx] = end_cursor;
        }

        let inject_ms = inject_start.elapsed().as_millis() as u64;
        total_sent += wave_ok;
        total_err += wave_err;

        // Wait for pool drain
        let drain_start = Instant::now();
        let mut drain_pending = 0u64;

        loop {
            tokio::time::sleep(Duration::from_millis(10)).await;
            if let Ok((pending, _)) = get_txpool_status(client, &rpc_urls[0]).await {
                drain_pending = pending;
                if pending < 500 { break; }
            }
            if drain_start.elapsed() > Duration::from_secs(30) {
                tracing::warn!(pending = drain_pending, "sync-inject drain timeout");
                break;
            }
        }

        let drain_ms = drain_start.elapsed().as_millis() as u64;
        let total_ms = inject_start.elapsed().as_millis() as u64;
        wave_num += 1;

        let block = get_block_number(client, &rpc_urls[0]).await.unwrap_or(0);
        let tps = wave_ok as f64 / (drain_ms as f64 / 1000.0).max(0.001);

        tracing::info!(
            wave = wave_num,
            txs = wave_ok,
            err = wave_err,
            inject_ms,
            drain_ms,
            total_ms,
            block = block - start_block,
            pool_pending = drain_pending,
            view_tps = format!("{:.0}", tps),
            "SYNC_WAVE"
        );

        wave_times.push((inject_ms, drain_ms, wave_ok));
    }

    // Summary
    let elapsed = start.elapsed().as_secs_f64();
    let total_drain_ms: u64 = wave_times.iter().map(|(_, d, _)| *d).sum();
    let total_inject_ms: u64 = wave_times.iter().map(|(i, _, _)| *i).sum();
    let total_wave_txs: u64 = wave_times.iter().map(|(_, _, t)| *t).sum();

    let mut drain_times: Vec<u64> = wave_times.iter().map(|(_, d, _)| *d).collect();
    drain_times.sort();
    let n = drain_times.len();

    if n > 0 {
        let p50 = drain_times[n / 2];
        let p95 = drain_times[((n as f64 * 0.95) as usize).min(n - 1)];
        let avg = total_drain_ms / n as u64;
        let sustained_tps = total_wave_txs as f64 / (total_drain_ms as f64 / 1000.0).max(0.001);

        tracing::info!("=== SYNC INJECT RESULT ===");
        tracing::info!(
            waves = n,
            total_txs = total_sent,
            total_errors = total_err,
            elapsed = format!("{:.1}s", elapsed),
            total_inject_ms,
            total_drain_ms,
            drain_avg_ms = avg,
            drain_p50_ms = p50,
            drain_p95_ms = p95,
            drain_min_ms = drain_times[0],
            drain_max_ms = drain_times[n - 1],
            sustained_tps = format!("{:.0}", sustained_tps),
            wall_tps = format!("{:.0}", total_sent as f64 / elapsed.max(1.0)),
            "Complete"
        );
    } else {
        tracing::warn!("No waves completed");
    }
}

/// Send pre-signed transactions with rate limiting.
#[allow(clippy::too_many_arguments)]
async fn run_presign_send(
    presigned: Vec<Vec<String>>,
    rpc_urls: &[String],
    client: &reqwest::Client,
    stats: &Arc<Stats>,
    semaphore: &Arc<Semaphore>,
    batch_size: usize,
    target_tps: u64,
    max_pool: u64,
    resume_pool: u64,
    stop: &Arc<AtomicBool>,
) {
    let bp_active = Arc::new(AtomicBool::new(false));
    let bp_monitor = spawn_backpressure_monitor(
        client.clone(), rpc_urls[0].clone(), bp_active.clone(),
        max_pool, resume_pool, stats.clone(), stop.clone(),
    );

    let num_rpcs = presigned.len();
    let per_rpc_tps = if target_tps > 0 {
        (target_tps / num_rpcs as u64).max(1)
    } else {
        0
    };

    let mut handles = Vec::new();
    for (rpc_idx, txs) in presigned.into_iter().enumerate() {
        if txs.is_empty() {
            continue;
        }
        let rpc_url = rpc_urls[rpc_idx].clone();
        let client = client.clone();
        let stats = stats.clone();
        let semaphore = semaphore.clone();
        let bp_active = bp_active.clone();
        let stop = stop.clone();
        let bs = batch_size;

        // Rate limiter per RPC
        let batch_interval = if per_rpc_tps > 0 {
            let batches_per_sec = (per_rpc_tps as f64 / bs as f64).ceil() as u64;
            Some(Duration::from_secs_f64(1.0 / batches_per_sec.max(1) as f64))
        } else {
            None
        };

        tracing::info!(
            rpc_idx, txs = txs.len(), batch_size = bs,
            per_rpc_tps,
            batch_interval_ms = batch_interval.map(|d| d.as_millis() as u64).unwrap_or(0),
            "presign sender starting"
        );

        handles.push(tokio::spawn(async move {
            let mut last_send = Instant::now();
            for chunk in txs.chunks(bs) {
                if stop.load(Ordering::Relaxed) {
                    break;
                }

                // Respect backpressure
                while bp_active.load(Ordering::Relaxed) {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }

                // Rate limiting
                if let Some(interval) = batch_interval {
                    let since_last = last_send.elapsed();
                    if since_last < interval {
                        tokio::time::sleep(interval - since_last).await;
                    }
                }
                last_send = Instant::now();

                let permit = semaphore.clone().acquire_owned().await.unwrap();
                let client_clone = client.clone();
                let stats_clone = stats.clone();
                let rpc_url_clone = rpc_url.clone();
                let batch: Vec<String> = chunk.to_vec();

                tokio::spawn(async move {
                    let result = send_batch(&client_clone, &rpc_url_clone, &batch).await;
                    stats_clone.sent.fetch_add(result.ok as u64, Ordering::Relaxed);
                    stats_clone.rpc_errors.fetch_add(result.rpc_error as u64, Ordering::Relaxed);
                    stats_clone.http_errors.fetch_add(result.http_error as u64, Ordering::Relaxed);
                    stats_clone.rpc_latency_ns.fetch_add(result.latency.as_nanos() as u64, Ordering::Relaxed);
                    stats_clone.rpc_latency_count.fetch_add(1, Ordering::Relaxed);
                    drop(permit);
                });
            }
        }));
    }

    for h in handles {
        let _ = h.await;
    }

    bp_monitor.abort();

    // Wait for in-flight requests
    tokio::time::sleep(Duration::from_millis(2000)).await;
}

/// Wave mode: inject exactly `wave_cap` txs, wait for new block, repeat.
/// Keeps pool_pending ≈ cap, preventing pool overload and ensuring fast builds.
async fn run_wave_mode(
    presigned: Vec<Vec<String>>,
    rpc_urls: &[String],
    client: &reqwest::Client,
    stats: &Arc<Stats>,
    semaphore: &Arc<Semaphore>,
    batch_size: usize,
    wave_cap: usize,
    duration: Duration,
) {
    let num_rpcs = presigned.len();
    let per_rpc_cap = wave_cap / num_rpcs;

    // Flatten into per-RPC cursors
    let mut cursors: Vec<usize> = vec![0; num_rpcs];
    let start = Instant::now();
    let mut wave_num = 0u64;
    let mut last_block = get_block_number(client, &rpc_urls[0]).await.unwrap_or(0);

    tracing::info!(
        wave_cap, per_rpc_cap, num_rpcs, batch_size,
        "wave mode starting"
    );

    loop {
        if start.elapsed() >= duration {
            break;
        }

        // Check if we have enough txs left
        let mut have_txs = false;
        for (rpc_idx, cursor) in cursors.iter().enumerate() {
            if *cursor < presigned[rpc_idx].len() {
                have_txs = true;
                break;
            }
        }
        if !have_txs {
            tracing::info!("all presigned txs exhausted");
            break;
        }

        let wave_start = Instant::now();

        // Send one wave: per_rpc_cap txs to each RPC, in parallel
        let mut inject_handles = Vec::new();
        let mut wave_total = 0usize;

        for rpc_idx in 0..num_rpcs {
            let cursor = cursors[rpc_idx];
            let remaining = presigned[rpc_idx].len() - cursor;
            let take = remaining.min(per_rpc_cap);
            if take == 0 {
                continue;
            }
            wave_total += take;
            let end = cursor + take;
            cursors[rpc_idx] = end;

            // Fire ALL batches in parallel (not sequential) for max injection speed
            for chunk in presigned[rpc_idx][cursor..end].chunks(batch_size) {
                let rpc_url = rpc_urls[rpc_idx].clone();
                let client = client.clone();
                let stats = stats.clone();
                let sem = semaphore.clone();
                let batch: Vec<String> = chunk.to_vec();

                inject_handles.push(tokio::spawn(async move {
                    let permit = sem.acquire_owned().await.unwrap();
                    let result = send_batch(&client, &rpc_url, &batch).await;
                    drop(permit);
                    (result.ok as u64, result.rpc_error as u64)
                }));
            }
        }

        // Wait for all injection to complete
        let mut wave_ok = 0u64;
        let mut wave_err = 0u64;
        for h in inject_handles {
            if let Ok((ok, err)) = h.await {
                wave_ok += ok;
                wave_err += err;
            }
        }
        stats.sent.fetch_add(wave_ok, Ordering::Relaxed);
        stats.rpc_errors.fetch_add(wave_err, Ordering::Relaxed);

        let inject_ms = wave_start.elapsed().as_millis() as u64;

        // Wait for new block
        let wait_start = Instant::now();
        loop {
            tokio::time::sleep(Duration::from_millis(10)).await;
            if let Ok(bn) = get_block_number(client, &rpc_urls[0]).await {
                if bn > last_block {
                    last_block = bn;
                    break;
                }
            }
            if wait_start.elapsed() > Duration::from_secs(30) {
                tracing::warn!("wave timeout waiting for new block");
                break;
            }
        }

        let wait_ms = wait_start.elapsed().as_millis() as u64;
        let total_ms = wave_start.elapsed().as_millis() as u64;
        wave_num += 1;

        // Get pool status
        let (pending, queued) = get_txpool_status(client, &rpc_urls[0]).await.unwrap_or((0, 0));

        tracing::info!(
            wave = wave_num,
            txs = wave_total,
            ok = wave_ok,
            err = wave_err,
            inject_ms,
            wait_ms,
            total_ms,
            block = last_block,
            pool_pending = pending,
            pool_queued = queued,
            tps = format!("{:.0}", wave_total as f64 / (total_ms as f64 / 1000.0).max(0.001)),
            "WAVE"
        );
    }
}

/// Run test with parallel senders (one per RPC endpoint).
#[allow(clippy::too_many_arguments)]
async fn run_test(
    accounts: &[Arc<TestAccount>],
    targets: &[Address],
    rpc_urls: &[String],
    client: &reqwest::Client,
    stats: &Arc<Stats>,
    semaphore: &Arc<Semaphore>,
    target_tps: u64,
    batch_size: usize,
    accounts_per_batch: usize,
    erc20_ratio: u8,
    duration: Duration,
    max_pool: u64,
    resume_pool: u64,
    stop: &Arc<AtomicBool>,
) {
    let num_rpcs = rpc_urls.len();
    let partitions = partition_by_rpc(accounts, num_rpcs);

    // Per-RPC TPS target (divide evenly)
    let per_rpc_tps = if target_tps > 0 {
        (target_tps / num_rpcs as u64).max(1)
    } else {
        0
    };

    // Per-RPC accounts_per_batch: use as many accounts as possible per batch
    // to minimize txs-per-account (reducing nonce gap damage).
    // Each sender has its own partition, so accounts_per_batch is NOT divided by num_rpcs.
    let per_rpc_apb = |partition_size: usize| -> usize {
        accounts_per_batch.min(partition_size).max(1)
    };

    // Shared backpressure flag
    let bp_active = Arc::new(AtomicBool::new(false));
    let bp_monitor = spawn_backpressure_monitor(
        client.clone(), rpc_urls[0].clone(), bp_active.clone(),
        max_pool, resume_pool, stats.clone(), stop.clone(),
    );

    // Spawn sender tasks
    let mut handles = Vec::new();
    for (rpc_idx, partition) in partitions.into_iter().enumerate() {
        if partition.is_empty() {
            continue;
        }
        let apb = per_rpc_apb(partition.len());
        tracing::info!(
            rpc_idx, accounts = partition.len(), apb,
            per_rpc_tps, batch_size,
            "spawning sender"
        );

        handles.push(tokio::spawn(sender_loop(
            rpc_idx,
            partition,
            targets.to_vec(),
            rpc_urls[rpc_idx].clone(),
            client.clone(),
            stats.clone(),
            semaphore.clone(),
            per_rpc_tps,
            batch_size,
            apb,
            erc20_ratio,
            duration,
            bp_active.clone(),
            stop.clone(),
        )));
    }

    // Wait for all senders to finish
    for h in handles {
        let _ = h.await;
    }

    bp_monitor.abort();

    // Wait for in-flight requests
    tokio::time::sleep(Duration::from_millis(500)).await;
}

fn spawn_reporter(
    stats: Arc<Stats>,
    client: reqwest::Client,
    rpc_url: String,
    start: Instant,
    sb: u64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(10));
        loop {
            interval.tick().await;
            let sent = stats.sent.load(Ordering::Relaxed);
            let rpc_err = stats.rpc_errors.load(Ordering::Relaxed);
            let http_err = stats.http_errors.load(Ordering::Relaxed);
            let elapsed = start.elapsed().as_secs_f64();
            let block = get_block_number(&client, &rpc_url).await.unwrap_or(0);
            let blocks = block.saturating_sub(sb);

            let pool = get_txpool_status(&client, &rpc_url).await.unwrap_or((0, 0));
            let bp_pauses = stats.backpressure_pauses.load(Ordering::Relaxed);
            let bp_ms = stats.backpressure_ms.load(Ordering::Relaxed);
            let resyncs = stats.nonce_resyncs.load(Ordering::Relaxed);

            tracing::info!(
                sent, rpc_err, http_err,
                elapsed = format!("{:.0}s", elapsed),
                effective_tps = format!("{:.1}", sent as f64 / elapsed.max(1.0)),
                block, blocks,
                avg_tx_per_block = if blocks > 0 { sent / blocks } else { 0 },
                rpc_lat_ms = format!("{:.1}", stats.avg_rpc_latency_ms()),
                sign_us = format!("{:.1}", stats.avg_sign_us()),
                pool_pending = pool.0,
                pool_queued = pool.1,
                bp_pauses,
                bp_ms,
                resyncs,
                "STATS"
            );
        }
    })
}

/// Detailed per-block analysis with timestamps for accurate TPS
async fn analyze_blocks(
    client: &reqwest::Client,
    rpc_url: &str,
    start_block: u64,
    end_block: u64,
) {
    if end_block <= start_block {
        return;
    }

    let block_count = end_block - start_block;
    let analysis_start = if block_count > 50 { end_block - 50 } else { start_block };
    let actual_count = end_block - analysis_start;

    let mut blocks = Vec::new();
    for b in (analysis_start + 1)..=end_block {
        if let Ok(info) = get_block_info(client, rpc_url, b).await {
            blocks.push(info);
        }
    }

    if blocks.len() < 2 {
        return;
    }

    let mut total_tx = 0u64;
    let mut total_gas_used = 0u64;
    let mut max_tx = 0u64;
    let mut max_block_tps = 0.0f64;
    let mut block_times = Vec::new();
    let mut block_tps_values = Vec::new();

    for i in 0..blocks.len() {
        total_tx += blocks[i].tx_count;
        total_gas_used += blocks[i].gas_used;
        max_tx = max_tx.max(blocks[i].tx_count);

        if i > 0 {
            let dt = blocks[i].timestamp.saturating_sub(blocks[i - 1].timestamp);
            if dt > 0 {
                let tps = blocks[i].tx_count as f64 / dt as f64;
                block_tps_values.push(tps);
                max_block_tps = max_block_tps.max(tps);
                block_times.push(dt);
            }
        }
    }

    let avg_block_time = if !block_times.is_empty() {
        block_times.iter().sum::<u64>() as f64 / block_times.len() as f64
    } else {
        0.0
    };

    let avg_block_tps = if !block_tps_values.is_empty() {
        block_tps_values.iter().sum::<f64>() / block_tps_values.len() as f64
    } else {
        0.0
    };

    block_tps_values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p50_tps = if !block_tps_values.is_empty() {
        block_tps_values[block_tps_values.len() / 2]
    } else {
        0.0
    };
    let p95_idx = ((block_tps_values.len() as f64 * 0.95) as usize).min(block_tps_values.len().saturating_sub(1));
    let p95_tps = if !block_tps_values.is_empty() {
        block_tps_values[p95_idx]
    } else {
        0.0
    };

    let avg_gas_limit = if !blocks.is_empty() {
        blocks.iter().map(|b| b.gas_limit).sum::<u64>() / blocks.len() as u64
    } else {
        0
    };
    let avg_gas_used = total_gas_used / actual_count.max(1);
    let gas_util = if avg_gas_limit > 0 {
        (avg_gas_used as f64 / avg_gas_limit as f64) * 100.0
    } else {
        0.0
    };

    let time_span = blocks.last().unwrap().timestamp - blocks.first().unwrap().timestamp;
    let overall_tps = if time_span > 0 {
        total_tx as f64 / time_span as f64
    } else {
        0.0
    };

    tracing::info!(
        blocks = actual_count,
        total_tx,
        avg_block_time = format!("{:.1}s", avg_block_time),
        overall_tps = format!("{:.1}", overall_tps),
        avg_block_tps = format!("{:.1}", avg_block_tps),
        p50_tps = format!("{:.1}", p50_tps),
        p95_tps = format!("{:.1}", p95_tps),
        max_block_tps = format!("{:.1}", max_block_tps),
        max_tx_in_block = max_tx,
        avg_gas_used,
        gas_limit = avg_gas_limit,
        gas_utilization = format!("{:.1}%", gas_util),
        "BLOCK_ANALYSIS"
    );

    // Print top 5 blocks by TPS
    let mut block_with_tps: Vec<(u64, u64, f64, u64, u64)> = Vec::new();
    for i in 1..blocks.len() {
        let dt = blocks[i].timestamp.saturating_sub(blocks[i - 1].timestamp);
        if dt > 0 {
            let tps = blocks[i].tx_count as f64 / dt as f64;
            block_with_tps.push((blocks[i].number, blocks[i].tx_count, tps, blocks[i].gas_used, blocks[i].gas_limit));
        }
    }
    block_with_tps.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
    for (i, (num, txs, tps, gas_used, gas_limit)) in block_with_tps.iter().take(5).enumerate() {
        let util = if *gas_limit > 0 { (*gas_used as f64 / *gas_limit as f64) * 100.0 } else { 0.0 };
        tracing::info!(
            rank = i + 1,
            block = num,
            txs,
            tps = format!("{:.1}", tps),
            gas_used,
            gas_limit,
            gas_util = format!("{:.1}%", util),
            "TOP_BLOCK"
        );
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    let rpc_urls: Vec<String> = cli.rpc.split(',').map(|s| s.trim().to_string()).collect();

    tracing::info!("=== N42 TPS Stress Test v10 (Blast Mode + Optimized Injection) ===");
    tracing::info!(
        target_tps = cli.target_tps,
        duration = cli.duration,
        accounts = cli.accounts,
        batch_size = cli.batch_size,
        accounts_per_batch = cli.accounts_per_batch,
        erc20_ratio = cli.erc20_ratio,
        concurrency = cli.concurrency,
        max_pool = cli.max_pool,
        resume_pool = cli.resume_pool,
        blast = cli.blast,
        rpc_count = rpc_urls.len(),
        "Configuration"
    );

    // Force HTTP/1.1 to avoid HTTP/2 single-connection multiplexing bottleneck.
    // With HTTP/1.1, reqwest opens multiple TCP connections per host,
    // allowing true parallel batch processing on the server side.
    let client = reqwest::Client::builder()
        .http1_only()
        .pool_max_idle_per_host(cli.concurrency)
        .pool_idle_timeout(Duration::from_secs(30))
        .tcp_keepalive(Duration::from_secs(15))
        .timeout(Duration::from_secs(60))
        .build()?;

    let accounts = create_accounts(cli.accounts, rpc_urls.len());
    let targets: Vec<Address> = accounts.iter().map(|a| a.address).collect();
    let semaphore = Arc::new(Semaphore::new(cli.concurrency));
    let stop = Arc::new(AtomicBool::new(false));

    // Parallel nonce sync
    sync_nonces_parallel(&accounts, &rpc_urls, &client).await;

    // === Pre-sign save mode: sign and save to file, then exit ===
    if let Some(ref save_path) = cli.presign_save {
        let total = if cli.presign > 0 { cli.presign } else { 5_000_000 };
        tracing::info!("=== N42 Pre-sign Save Mode ===");
        presign_and_save(
            &accounts, &targets, total, rpc_urls.len(), cli.erc20_ratio, save_path,
        )?;
        return Ok(());
    }

    // === Binary TCP inject mode ===
    if let Some(ref inject_endpoints) = cli.inject {
        let load_path = cli.presign_load.as_ref()
            .ok_or_else(|| eyre::eyre!("--inject requires --presign-load"))?;
        let endpoints: Vec<String> = inject_endpoints.split(',').map(|s| s.trim().to_string()).collect();

        tracing::info!("=== N42 BINARY TCP INJECT MODE ===");
        tracing::info!(endpoints = endpoints.len(), "TCP inject endpoints");

        let presigned = load_presigned_binary(load_path, endpoints.len())?;

        let start_block = get_block_number(&client, &rpc_urls[0]).await?;
        if let Ok(info) = get_block_info(&client, &rpc_urls[0], start_block).await {
            tracing::info!(
                block = start_block,
                gas_limit = info.gas_limit,
                gas_limit_M = info.gas_limit / 1_000_000,
                "Chain active"
            );
        }

        if cli.wave > 0 {
            // Synchronized TCP inject: wave_cap txs per wave, wait for pool drain
            tracing::info!("=== SYNC TCP INJECT MODE (wave={}) ===", cli.wave);

            run_sync_inject_mode(
                &endpoints, presigned, cli.batch_size, cli.wave as usize,
                &rpc_urls, &client, start_block, cli.duration,
            ).await;

            let final_block = get_block_number(&client, &rpc_urls[0]).await?;
            analyze_blocks(&client, &rpc_urls[0], start_block, final_block).await;
            return Ok(());
        }

        let stats = Arc::new(Stats::new());
        stats.start_block.store(start_block, Ordering::Relaxed);

        run_inject_mode(
            &endpoints, presigned, cli.batch_size, cli.target_tps, &stats,
            &rpc_urls, &client, start_block, cli.duration,
        ).await;

        // Wait for pool drain
        tracing::info!("Waiting for pending pool to drain...");
        let drain_start = Instant::now();
        loop {
            if drain_start.elapsed() > Duration::from_secs(120) {
                tracing::warn!("Pool drain timeout (120s)");
                break;
            }
            if let Ok((pending, queued)) = get_txpool_status(&client, &rpc_urls[0]).await {
                if pending < 500 {
                    tracing::info!(pending, queued, drain_ms = drain_start.elapsed().as_millis(), "Pool drained");
                    break;
                }
                tracing::info!(pending, queued, "Draining...");
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }

        let final_block = get_block_number(&client, &rpc_urls[0]).await?;
        analyze_blocks(&client, &rpc_urls[0], start_block, final_block).await;

        return Ok(());
    }

    // === Blast mode: pre-sign N txs, then blast at max speed ===
    if cli.blast > 0 {
        tracing::info!("=== N42 BLAST MODE v10 ===");
        tracing::info!(
            total_txs = cli.blast,
            accounts = cli.accounts,
            batch_size = cli.batch_size,
            rpcs = rpc_urls.len(),
            "Pre-signing transactions (all CPU cores)..."
        );

        let presigned = presign_all(
            &accounts, &targets, cli.blast, rpc_urls.len(), cli.erc20_ratio,
        );

        let start_block = get_block_number(&client, &rpc_urls[0]).await?;
        if let Ok(info) = get_block_info(&client, &rpc_urls[0], start_block).await {
            tracing::info!(
                block = start_block,
                gas_limit = info.gas_limit,
                gas_limit_M = info.gas_limit / 1_000_000,
                max_eth_transfers = info.gas_limit / TRANSFER_GAS,
                "Chain active"
            );
        }

        let stats = Arc::new(Stats::new());
        stats.start_block.store(start_block, Ordering::Relaxed);
        let start = Instant::now();

        // Real-time block TPS monitor (every 2s)
        let monitor_client = client.clone();
        let monitor_rpc = rpc_urls[0].clone();
        let monitor_stats = stats.clone();
        let monitor_stop = stop.clone();
        let monitor_start_block = start_block;
        let monitor_handle = tokio::spawn(async move {
            let mut last_block = monitor_start_block;
            let mut last_time = Instant::now();
            let mut interval = tokio::time::interval(Duration::from_secs(2));
            interval.tick().await; // skip first immediate tick
            loop {
                interval.tick().await;
                if monitor_stop.load(Ordering::Relaxed) {
                    break;
                }
                let now_block = get_block_number(&monitor_client, &monitor_rpc).await.unwrap_or(last_block);
                if now_block > last_block {
                    // Fetch latest blocks for real-time TPS
                    let mut recent_txs = 0u64;
                    let check_from = if now_block > last_block + 5 { now_block - 5 } else { last_block };
                    for b in (check_from + 1)..=now_block {
                        if let Ok(info) = get_block_info(&monitor_client, &monitor_rpc, b).await {
                            recent_txs += info.tx_count;
                        }
                    }
                    let dt = last_time.elapsed().as_secs_f64();
                    let sent = monitor_stats.sent.load(Ordering::Relaxed);
                    let rpc_err = monitor_stats.rpc_errors.load(Ordering::Relaxed);
                    let pool = get_txpool_status(&monitor_client, &monitor_rpc).await.unwrap_or((0, 0));

                    tracing::info!(
                        blocks = now_block - monitor_start_block,
                        recent_block_txs = recent_txs,
                        block_tps = format!("{:.0}", recent_txs as f64 / dt.max(0.1)),
                        injection_sent = sent,
                        injection_err = rpc_err,
                        injection_tps = format!("{:.0}", sent as f64 / start.elapsed().as_secs_f64().max(0.1)),
                        pool_pending = pool.0,
                        pool_queued = pool.1,
                        "BLAST_MONITOR"
                    );
                    last_block = now_block;
                    last_time = Instant::now();
                }
            }
        });

        // Blast send: use target_tps for rate limiting (0 = unlimited)
        let blast_tps = cli.target_tps;
        if blast_tps > 0 {
            tracing::info!(target_tps = blast_tps, "Blast with rate limiting (sustained mode)");
        } else {
            tracing::info!("Blast with NO rate limiting (max speed)");
        }
        stop.store(false, Ordering::Relaxed);

        // Auto-stop after --duration seconds (if blast has enough txs)
        let duration_stop = stop.clone();
        let duration_secs = cli.duration;
        let duration_guard = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(duration_secs)).await;
            duration_stop.store(true, Ordering::Relaxed);
            tracing::info!(duration_secs, "Blast duration reached, stopping injection");
        });

        run_presign_send(
            presigned, &rpc_urls, &client, &stats, &semaphore,
            cli.batch_size, blast_tps, cli.max_pool, cli.resume_pool, &stop,
        ).await;

        duration_guard.abort();

        stop.store(true, Ordering::Relaxed);
        monitor_handle.abort();

        let sent = stats.sent.load(Ordering::Relaxed);
        let rpc_err = stats.rpc_errors.load(Ordering::Relaxed);
        let http_err = stats.http_errors.load(Ordering::Relaxed);
        let elapsed = start.elapsed().as_secs_f64();
        let end_block = get_block_number(&client, &rpc_urls[0]).await?;
        let blocks = end_block - start_block;

        tracing::info!("=== BLAST INJECTION COMPLETE ===");
        tracing::info!(
            injection_tps = format!("{:.0}", sent as f64 / elapsed.max(1.0)),
            sent, rpc_err, http_err, blocks,
            duration = format!("{:.1}s", elapsed),
            avg_rpc_latency_ms = format!("{:.1}", stats.avg_rpc_latency_ms()),
            bp_pauses = stats.backpressure_pauses.load(Ordering::Relaxed),
            "Injection phase done, waiting for pool drain..."
        );

        // Wait for pool drain
        let drain_start = Instant::now();
        loop {
            if drain_start.elapsed() > Duration::from_secs(120) {
                tracing::warn!("Pool drain timeout (120s)");
                break;
            }
            if let Ok((pending, queued)) = get_txpool_status(&client, &rpc_urls[0]).await {
                if pending < 500 {
                    tracing::info!(pending, queued, drain_ms = drain_start.elapsed().as_millis(), "Pool drained");
                    break;
                }
                tracing::info!(pending, queued, "Draining...");
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }

        let final_block = get_block_number(&client, &rpc_urls[0]).await?;
        let total_blocks = final_block - start_block;
        let total_elapsed = start.elapsed().as_secs_f64();

        tracing::info!("=== BLAST RESULT ===");
        tracing::info!(
            total_txs = sent,
            total_blocks,
            total_time = format!("{:.1}s", total_elapsed),
            sustained_tps = format!("{:.0}", sent as f64 / total_elapsed.max(1.0)),
            avg_tx_per_block = sent / total_blocks.max(1),
            "Summary"
        );

        analyze_blocks(&client, &rpc_urls[0], start_block, final_block).await;

        return Ok(());
    }

    // === Wave mode: synchronized injection, one wave per block ===
    if cli.wave > 0 {
        let load_path = cli.presign_load.as_ref()
            .ok_or_else(|| eyre::eyre!("--wave requires --presign-load"))?;
        tracing::info!("=== N42 Wave Mode: cap={} ===", cli.wave);
        let presigned = load_presigned(load_path)?;

        let start_block = get_block_number(&client, &rpc_urls[0]).await?;
        if let Ok(info) = get_block_info(&client, &rpc_urls[0], start_block).await {
            tracing::info!(
                block = start_block,
                gas_limit = info.gas_limit,
                gas_limit_M = info.gas_limit / 1_000_000,
                "Chain active"
            );
        }

        let stats = Arc::new(Stats::new());
        stats.start_block.store(start_block, Ordering::Relaxed);
        let start = Instant::now();

        let reporter = spawn_reporter(
            stats.clone(), client.clone(), rpc_urls[0].clone(), start, start_block,
        );

        run_wave_mode(
            presigned, &rpc_urls, &client, &stats, &semaphore,
            cli.batch_size, cli.wave as usize,
            Duration::from_secs(cli.duration),
        ).await;

        reporter.abort();

        let sent = stats.sent.load(Ordering::Relaxed);
        let rpc_err = stats.rpc_errors.load(Ordering::Relaxed);
        let elapsed = start.elapsed().as_secs_f64();
        let end_block = get_block_number(&client, &rpc_urls[0]).await?;
        let blocks = end_block - start_block;

        tracing::info!("=== WAVE RESULT ===");
        tracing::info!(
            total_txs = sent,
            total_blocks = blocks,
            total_time = format!("{:.1}s", elapsed),
            overall_tps = format!("{:.0}", sent as f64 / elapsed.max(1.0)),
            avg_tx_per_block = sent / blocks.max(1),
            rpc_err,
            "Complete"
        );

        // Wait for pool drain
        tracing::info!("Waiting for pending pool to drain...");
        let drain_start = Instant::now();
        loop {
            if drain_start.elapsed() > Duration::from_secs(120) {
                tracing::warn!("Pool drain timeout (120s)");
                break;
            }
            if let Ok((pending, _)) = get_txpool_status(&client, &rpc_urls[0]).await {
                if pending < 500 {
                    tracing::info!(pending, drain_ms = drain_start.elapsed().as_millis(), "Pool drained");
                    break;
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }

        let final_block = get_block_number(&client, &rpc_urls[0]).await?;
        analyze_blocks(&client, &rpc_urls[0], start_block, final_block).await;

        return Ok(());
    }

    // === Pre-sign load mode: load from file and blast-send ===
    if let Some(ref load_path) = cli.presign_load {
        tracing::info!("=== N42 Pre-sign Load + Blast Send Mode ===");
        let presigned = load_presigned(load_path)?;

        let start_block = get_block_number(&client, &rpc_urls[0]).await?;
        if let Ok(info) = get_block_info(&client, &rpc_urls[0], start_block).await {
            tracing::info!(
                block = start_block,
                gas_limit = info.gas_limit,
                gas_limit_M = info.gas_limit / 1_000_000,
                "Chain active"
            );
        }

        let stats = Arc::new(Stats::new());
        stats.start_block.store(start_block, Ordering::Relaxed);
        let start = Instant::now();

        let reporter = spawn_reporter(
            stats.clone(), client.clone(), rpc_urls[0].clone(), start, start_block,
        );

        stop.store(false, Ordering::Relaxed);
        run_presign_send(
            presigned, &rpc_urls, &client, &stats, &semaphore,
            cli.batch_size, cli.target_tps, cli.max_pool, cli.resume_pool, &stop,
        ).await;

        reporter.abort();

        let sent = stats.sent.load(Ordering::Relaxed);
        let rpc_err = stats.rpc_errors.load(Ordering::Relaxed);
        let http_err = stats.http_errors.load(Ordering::Relaxed);
        let elapsed = start.elapsed().as_secs_f64();
        let end_block = get_block_number(&client, &rpc_urls[0]).await?;
        let blocks = end_block - start_block;

        tracing::info!("=== PRE-SIGN LOAD RESULT ===");
        tracing::info!(
            effective_tps = format!("{:.1}", sent as f64 / elapsed.max(1.0)),
            injection_tps = format!("{:.1}", (sent + rpc_err) as f64 / elapsed.max(1.0)),
            sent, rpc_err, http_err, blocks,
            avg_tx_per_block = sent / blocks.max(1),
            duration = format!("{:.1}s", elapsed),
            avg_rpc_latency_ms = format!("{:.1}", stats.avg_rpc_latency_ms()),
            bp_pauses = stats.backpressure_pauses.load(Ordering::Relaxed),
            bp_total_ms = stats.backpressure_ms.load(Ordering::Relaxed),
            "Complete"
        );

        // Wait for pool drain
        tracing::info!("Waiting for pending pool to drain...");
        let drain_start = Instant::now();
        loop {
            if drain_start.elapsed() > Duration::from_secs(120) {
                tracing::warn!("Pool drain timeout (120s)");
                break;
            }
            if let Ok((pending, queued)) = get_txpool_status(&client, &rpc_urls[0]).await {
                if pending < 500 {
                    tracing::info!(pending, queued, "Pool drained");
                    break;
                }
                tracing::info!(pending, queued, "Draining...");
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }

        let final_block = get_block_number(&client, &rpc_urls[0]).await?;
        analyze_blocks(&client, &rpc_urls[0], start_block, final_block).await;

        return Ok(());
    }

    // === Pre-sign mode (in-memory): sign all txs upfront, then blast-send ===
    if cli.presign > 0 {
        tracing::info!("=== N42 Pre-sign Mode (legacy, use --blast for v10) ===");

        let presigned = presign_all(
            &accounts, &targets, cli.presign, rpc_urls.len(), cli.erc20_ratio,
        );

        let start_block = get_block_number(&client, &rpc_urls[0]).await?;
        if let Ok(info) = get_block_info(&client, &rpc_urls[0], start_block).await {
            tracing::info!(
                block = start_block,
                gas_limit = info.gas_limit,
                gas_limit_M = info.gas_limit / 1_000_000,
                "Chain active"
            );
        }

        let stats = Arc::new(Stats::new());
        stats.start_block.store(start_block, Ordering::Relaxed);
        let start = Instant::now();

        let reporter = spawn_reporter(
            stats.clone(), client.clone(), rpc_urls[0].clone(), start, start_block,
        );

        stop.store(false, Ordering::Relaxed);
        run_presign_send(
            presigned, &rpc_urls, &client, &stats, &semaphore,
            cli.batch_size, cli.target_tps, cli.max_pool, cli.resume_pool, &stop,
        ).await;

        reporter.abort();

        let sent = stats.sent.load(Ordering::Relaxed);
        let rpc_err = stats.rpc_errors.load(Ordering::Relaxed);
        let http_err = stats.http_errors.load(Ordering::Relaxed);
        let elapsed = start.elapsed().as_secs_f64();
        let end_block = get_block_number(&client, &rpc_urls[0]).await?;
        let blocks = end_block - start_block;

        tracing::info!("=== PRE-SIGN RESULT ===");
        tracing::info!(
            presign_total = cli.presign,
            effective_tps = format!("{:.1}", sent as f64 / elapsed.max(1.0)),
            injection_tps = format!("{:.1}", cli.presign as f64 / elapsed.max(1.0)),
            sent, rpc_err, http_err, blocks,
            avg_tx_per_block = sent / blocks.max(1),
            duration = format!("{:.1}s", elapsed),
            avg_rpc_latency_ms = format!("{:.1}", stats.avg_rpc_latency_ms()),
            bp_pauses = stats.backpressure_pauses.load(Ordering::Relaxed),
            bp_total_ms = stats.backpressure_ms.load(Ordering::Relaxed),
            "Complete"
        );

        // Wait for pool to drain and blocks to be produced
        tracing::info!("Waiting for pending pool to drain...");
        let drain_start = Instant::now();
        loop {
            if drain_start.elapsed() > Duration::from_secs(120) {
                tracing::warn!("Pool drain timeout (120s)");
                break;
            }
            if let Ok((pending, queued)) = get_txpool_status(&client, &rpc_urls[0]).await {
                if pending < 500 {
                    tracing::info!(pending, queued, "Pool drained");
                    break;
                }
                tracing::info!(pending, queued, "Draining...");
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }

        let final_block = get_block_number(&client, &rpc_urls[0]).await?;
        analyze_blocks(&client, &rpc_urls[0], start_block, final_block).await;

        return Ok(());
    }

    // Pre-fill tx pool if requested
    if cli.prefill > 0 {
        let prefill_start = Instant::now();
        let prefill_total = cli.prefill as usize;
        let batch_sz = cli.batch_size;
        let apb = cli.accounts_per_batch.min(accounts.len()).max(1);
        let num_groups = accounts.len().div_ceil(apb);
        let mut group_idx = 0usize;
        let mut sent = 0usize;
        tracing::info!(txs = prefill_total, "Pre-filling tx pool...");

        while sent < prefill_total {
            let group_start = (group_idx % num_groups) * apb;
            let group_end = (group_start + apb).min(accounts.len());
            let group = &accounts[group_start..group_end];
            let rpc_url = rpc_urls[group[0].rpc_idx].clone();

            let this_batch = batch_sz.min(prefill_total - sent);
            let (raw_txs, _, _) = sign_mixed_batch(group, &targets, this_batch, group.len(), cli.erc20_ratio);
            let c = client.clone();
            let sem = semaphore.clone();
            let permit = sem.acquire_owned().await.unwrap();
            tokio::spawn(async move {
                let _ = send_batch(&c, &rpc_url, &raw_txs).await;
                drop(permit);
            });

            sent += this_batch;
            group_idx += 1;
        }

        // Wait for all in-flight prefill requests
        tokio::time::sleep(Duration::from_secs(2)).await;
        sync_nonces_parallel(&accounts, &rpc_urls, &client).await;

        if let Ok((pending, queued)) = get_txpool_status(&client, &rpc_urls[0]).await {
            tracing::info!(
                prefill = prefill_total,
                elapsed_ms = prefill_start.elapsed().as_millis(),
                pool_pending = pending,
                pool_queued = queued,
                "Pre-fill complete (nonces resynced)"
            );
        }
    }

    let start_block = get_block_number(&client, &rpc_urls[0]).await?;

    if let Ok(info) = get_block_info(&client, &rpc_urls[0], start_block).await {
        tracing::info!(
            block = start_block,
            gas_limit = info.gas_limit,
            gas_limit_M = info.gas_limit / 1_000_000,
            max_eth_transfers = info.gas_limit / TRANSFER_GAS,
            "Chain active"
        );
    }

    if let Ok((pending, queued)) = get_txpool_status(&client, &rpc_urls[0]).await {
        tracing::info!(pending, queued, "Initial txpool status");
    }

    let stats = Arc::new(Stats::new());
    stats.start_block.store(start_block, Ordering::Relaxed);

    if cli.step {
        // Extended TPS levels to push 2G gas limit (~95K TPS theoretical max)
        let tps_levels = [3000, 7500, 15000, 25000, 40000, 60000, 80000, 100000];
        let step_duration = Duration::from_secs(cli.duration);

        for &target_tps in &tps_levels {
            sync_nonces_parallel(&accounts, &rpc_urls, &client).await;

            stats.reset();
            let sb = get_block_number(&client, &rpc_urls[0]).await?;
            stats.start_block.store(sb, Ordering::Relaxed);
            let start = Instant::now();

            // Scale accounts_per_batch with TPS level to minimize nonce gaps
            let apb = if target_tps >= 40000 {
                2000 // 2000 accounts/batch
            } else if target_tps >= 15000 {
                1000 // 1000 accounts/batch
            } else if target_tps >= 7500 {
                500  // 500 accounts/batch
            } else {
                cli.accounts_per_batch
            };

            // v10: use smaller batches (500) for faster HTTP round trips
            // Smaller payload = faster serialize/deserialize = lower latency
            let bs = cli.batch_size; // default 500 in v10

            tracing::info!("========================================");
            tracing::info!(
                target_tps,
                accounts_per_batch = apb,
                batch_size = bs,
                num_accounts = accounts.len(),
                "STEP TEST starting"
            );

            let reporter = spawn_reporter(
                stats.clone(), client.clone(), rpc_urls[0].clone(), start, sb,
            );

            stop.store(false, Ordering::Relaxed);
            run_test(
                &accounts, &targets, &rpc_urls, &client, &stats, &semaphore,
                target_tps, bs, apb, cli.erc20_ratio, step_duration,
                cli.max_pool, cli.resume_pool, &stop,
            ).await;

            reporter.abort();

            let sent = stats.sent.load(Ordering::Relaxed);
            let rpc_err = stats.rpc_errors.load(Ordering::Relaxed);
            let http_err = stats.http_errors.load(Ordering::Relaxed);
            let failed = rpc_err + http_err;
            let elapsed = start.elapsed().as_secs_f64();
            let end_block = get_block_number(&client, &rpc_urls[0]).await?;
            let blocks = end_block - sb;
            let effective_tps = sent as f64 / elapsed.max(1.0);
            let fail_rate = if sent + failed > 0 {
                (failed as f64 / (sent + failed) as f64) * 100.0
            } else {
                0.0
            };

            tracing::info!(
                target_tps,
                effective_tps = format!("{:.1}", effective_tps),
                sent, rpc_err, http_err, blocks,
                avg_tx_per_block = sent / blocks.max(1),
                fail_rate = format!("{:.1}%", fail_rate),
                avg_rpc_latency_ms = format!("{:.1}", stats.avg_rpc_latency_ms()),
                avg_sign_us = format!("{:.1}", stats.avg_sign_us()),
                bp_pauses = stats.backpressure_pauses.load(Ordering::Relaxed),
                bp_total_ms = stats.backpressure_ms.load(Ordering::Relaxed),
                nonce_resyncs = stats.nonce_resyncs.load(Ordering::Relaxed),
                "RESULT"
            );

            analyze_blocks(&client, &rpc_urls[0], sb, end_block).await;

            if let Ok((pending, queued)) = get_txpool_status(&client, &rpc_urls[0]).await {
                tracing::info!(pending, queued, "txpool after step");
            }

            if blocks == 0 && target_tps > 500 {
                tracing::warn!(target = target_tps, "STALL DETECTED - no new blocks produced, stopping");
                break;
            }

            if effective_tps < target_tps as f64 * 0.3 && target_tps > 500 {
                tracing::info!(
                    effective = format!("{:.0}", effective_tps),
                    target = target_tps,
                    "CEILING DETECTED - stopping"
                );
                break;
            }

            // Drain pending pool between steps
            tracing::info!("Waiting for pending pool to drain...");
            let drain_start = Instant::now();
            loop {
                if drain_start.elapsed() > Duration::from_secs(30) {
                    tracing::warn!("Pool drain timeout (30s), continuing anyway");
                    break;
                }
                if let Ok((pending, queued)) = get_txpool_status(&client, &rpc_urls[0]).await {
                    if pending < 500 {
                        tracing::info!(pending, queued, elapsed_ms = drain_start.elapsed().as_millis(), "Pending pool drained");
                        break;
                    }
                    tracing::info!(pending, queued, "Draining pending pool...");
                }
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    } else {
        let start = Instant::now();
        let reporter = spawn_reporter(
            stats.clone(), client.clone(), rpc_urls[0].clone(), start, start_block,
        );

        stop.store(false, Ordering::Relaxed);
        run_test(
            &accounts, &targets, &rpc_urls, &client, &stats, &semaphore,
            cli.target_tps, cli.batch_size, cli.accounts_per_batch,
            cli.erc20_ratio, Duration::from_secs(cli.duration),
            cli.max_pool, cli.resume_pool, &stop,
        ).await;

        reporter.abort();

        let sent = stats.sent.load(Ordering::Relaxed);
        let rpc_err = stats.rpc_errors.load(Ordering::Relaxed);
        let http_err = stats.http_errors.load(Ordering::Relaxed);
        let elapsed = start.elapsed().as_secs_f64();
        let end_block = get_block_number(&client, &rpc_urls[0]).await?;
        let blocks = end_block - start_block;

        tracing::info!("=== FINAL RESULT ===");
        tracing::info!(
            target_tps = cli.target_tps,
            effective_tps = format!("{:.1}", sent as f64 / elapsed.max(1.0)),
            sent, rpc_err, http_err, blocks,
            avg_tx_per_block = sent / blocks.max(1),
            duration = format!("{:.0}s", elapsed),
            avg_rpc_latency_ms = format!("{:.1}", stats.avg_rpc_latency_ms()),
            avg_sign_us = format!("{:.1}", stats.avg_sign_us()),
            bp_pauses = stats.backpressure_pauses.load(Ordering::Relaxed),
            bp_total_ms = stats.backpressure_ms.load(Ordering::Relaxed),
            nonce_resyncs = stats.nonce_resyncs.load(Ordering::Relaxed),
            "Complete"
        );

        analyze_blocks(&client, &rpc_urls[0], start_block, end_block).await;
    }

    Ok(())
}
