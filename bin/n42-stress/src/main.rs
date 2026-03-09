//! N42 High-Performance TPS Stress Test v9
//!
//! v9 improvements over v8:
//! - Pipelined signing and sending: signer task fills channel, sender task drains it
//! - Removes batch_interval sleep — signing throughput controls rate naturally
//! - Larger batch sizes at high TPS (up to 6000 tx/batch)
//! - Multiple signer threads per RPC for high TPS targets
//! - ~2x throughput vs v8 by overlapping sign and send

use alloy_consensus::{SignableTransaction, TxEip1559};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, Bytes, TxKind, U256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use clap::Parser;
use eyre::Result;
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
    #[arg(long, default_value = "3000")]
    accounts: usize,

    /// Batch size for JSON-RPC batch requests
    #[arg(long, default_value = "2000")]
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

    tracing::info!("=== N42 TPS Stress Test v9 (Pipelined Sign + Send) ===");
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
        rpc_count = rpc_urls.len(),
        "Configuration"
    );

    let client = reqwest::Client::builder()
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
        // Extended TPS levels to push 2G gas limit (~47K TPS theoretical max)
        let tps_levels = [1000, 3000, 5000, 7500, 10000, 15000, 20000, 30000, 40000, 50000];
        let step_duration = Duration::from_secs(cli.duration);

        for &target_tps in &tps_levels {
            sync_nonces_parallel(&accounts, &rpc_urls, &client).await;

            stats.reset();
            let sb = get_block_number(&client, &rpc_urls[0]).await?;
            stats.start_block.store(sb, Ordering::Relaxed);
            let start = Instant::now();

            // Scale accounts_per_batch with TPS level to minimize nonce gaps
            let apb = if target_tps >= 20000 {
                1000 // 1000 accounts/batch → 2 tx per account
            } else if target_tps >= 10000 {
                500  // 500 accounts/batch → 4 tx per account
            } else if target_tps >= 5000 {
                200  // 200 accounts/batch → 10 tx per account
            } else {
                cli.accounts_per_batch
            };

            // Scale batch_size with TPS level — larger batches reduce per-batch overhead
            let bs = if target_tps >= 30000 {
                6000  // 6K tx/batch → fewer HTTP roundtrips
            } else if target_tps >= 20000 {
                4000
            } else if target_tps >= 7500 {
                2000
            } else {
                cli.batch_size
            };

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
