//! N42 High-Performance TPS Stress Test v6
//!
//! Innovations:
//! - Multi-account mixed batches: each batch contains txs from multiple accounts
//! - Account-RPC pinning: each account sends to a fixed RPC node
//! - Per-block timestamp analysis: precise TPS from chain data
//! - txpool monitoring: track pool backlog in real-time
//! - Parallel nonce sync: batch RPC calls for fast startup
//! - Full pipeline latency breakdown

use alloy_consensus::{SignableTransaction, TxEip1559};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, Bytes, TxKind, U256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use clap::Parser;
use eyre::Result;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tiny_keccak::{Hasher, Keccak};
use tokio::sync::Semaphore;

#[derive(Parser)]
#[command(name = "n42-stress")]
struct Cli {
    /// Target TPS (0 = unlimited)
    #[arg(long, default_value = "500")]
    target_tps: u64,

    /// Test duration in seconds
    #[arg(long, default_value = "60")]
    duration: u64,

    /// Number of sender accounts
    #[arg(long, default_value = "600")]
    accounts: usize,

    /// Batch size for JSON-RPC batch requests
    #[arg(long, default_value = "100")]
    batch_size: usize,

    /// Number of accounts per batch (multi-account mixing)
    #[arg(long, default_value = "10")]
    accounts_per_batch: usize,

    /// Max concurrent HTTP requests across all RPC endpoints
    #[arg(long, default_value = "512")]
    concurrency: usize,

    /// Step mode: auto-increase TPS to find max
    #[arg(long)]
    step: bool,

    /// RPC endpoints (comma-separated)
    #[arg(long, default_value = "http://127.0.0.1:18545,http://127.0.0.1:18546,http://127.0.0.1:18547,http://127.0.0.1:18548,http://127.0.0.1:18549,http://127.0.0.1:18550,http://127.0.0.1:18551")]
    rpc: String,
}

const CHAIN_ID: u64 = 4242;
const TRANSFER_GAS: u64 = 21_000;
const MAX_FEE_PER_GAS: u128 = 2_000_000_000;
const MAX_PRIORITY_FEE: u128 = 1_000_000_000;
const TRANSFER_VALUE: u128 = 1_000_000_000_000;

struct Stats {
    sent: AtomicU64,
    rpc_errors: AtomicU64,
    http_errors: AtomicU64,
    start_block: AtomicU64,
    /// Total RPC round-trip nanoseconds (for averaging)
    rpc_latency_ns: AtomicU64,
    rpc_latency_count: AtomicU64,
    /// Total signing time nanoseconds
    sign_time_ns: AtomicU64,
    sign_count: AtomicU64,
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
    /// Pinned RPC index for this account
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

/// Sign a mixed batch: txs from multiple accounts interleaved
fn sign_mixed_batch(
    accounts: &[Arc<TestAccount>],
    targets: &[Address],
    batch_size: usize,
    accounts_per_batch: usize,
) -> (Vec<String>, Duration) {
    let start = Instant::now();
    let txs_per_account = batch_size / accounts_per_batch;
    let remainder = batch_size % accounts_per_batch;
    let mut result = Vec::with_capacity(batch_size);

    for (acct_idx, account) in accounts.iter().take(accounts_per_batch).enumerate() {
        let count = txs_per_account + if acct_idx < remainder { 1 } else { 0 };
        for _ in 0..count {
            let nonce = account.nonce.fetch_add(1, Ordering::Relaxed);
            let to = targets[(nonce as usize) % targets.len()];
            let tx = TxEip1559 {
                chain_id: CHAIN_ID,
                nonce,
                gas_limit: TRANSFER_GAS,
                max_fee_per_gas: MAX_FEE_PER_GAS,
                max_priority_fee_per_gas: MAX_PRIORITY_FEE,
                to: TxKind::Call(to),
                value: U256::from(TRANSFER_VALUE),
                input: Bytes::new(),
                access_list: Default::default(),
            };
            let sig_hash = tx.signature_hash();
            let sig = account.signer.sign_hash_sync(&sig_hash).expect("sign");
            let signed = tx.into_signed(sig);
            let mut buf = Vec::new();
            signed.encode_2718(&mut buf);
            result.push(format!("0x{}", hex::encode(&buf)));
        }
    }

    (result, start.elapsed())
}

struct BatchResult {
    ok: usize,
    rpc_error: usize,
    http_error: usize,
    latency: Duration,
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
                for r in &results {
                    if r.get("error").is_some() {
                        rpc_error += 1;
                    } else {
                        ok += 1;
                    }
                }
                BatchResult { ok, rpc_error, http_error: 0, latency }
            }
            Err(_) => BatchResult {
                ok: 0, rpc_error: 0, http_error: raw_txs.len(),
                latency: start.elapsed(),
            },
        },
        Err(_) => BatchResult {
            ok: 0, rpc_error: 0, http_error: raw_txs.len(),
            latency: start.elapsed(),
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
    duration: Duration,
) {
    let start = Instant::now();
    let num_accounts = accounts.len();
    let apb = accounts_per_batch.min(num_accounts).max(1);
    let batches_per_sec = if target_tps > 0 {
        (target_tps as f64 / batch_size as f64).ceil() as u64
    } else {
        10000
    };
    let batch_interval = Duration::from_secs_f64(1.0 / batches_per_sec as f64);

    let mut group_idx: usize = 0;
    // Divide accounts into groups of `apb` for multi-account batching
    let num_groups = num_accounts.div_ceil(apb);

    while start.elapsed() < duration {
        let group_start = (group_idx % num_groups) * apb;
        let group_end = (group_start + apb).min(num_accounts);
        let group = &accounts[group_start..group_end];

        // Use the first account's pinned RPC for this group
        let rpc_url = rpc_urls[group[0].rpc_idx].clone();

        let (raw_txs, sign_time) = sign_mixed_batch(group, targets, batch_size, group.len());

        stats.sign_time_ns.fetch_add(sign_time.as_nanos() as u64, Ordering::Relaxed);
        stats.sign_count.fetch_add(raw_txs.len() as u64, Ordering::Relaxed);

        let permit = semaphore.clone().acquire_owned().await.unwrap();
        let client = client.clone();
        let stats = stats.clone();

        tokio::spawn(async move {
            let result = send_batch(&client, &rpc_url, &raw_txs).await;
            stats.sent.fetch_add(result.ok as u64, Ordering::Relaxed);
            stats.rpc_errors.fetch_add(result.rpc_error as u64, Ordering::Relaxed);
            stats.http_errors.fetch_add(result.http_error as u64, Ordering::Relaxed);
            stats.rpc_latency_ns.fetch_add(result.latency.as_nanos() as u64, Ordering::Relaxed);
            stats.rpc_latency_count.fetch_add(1, Ordering::Relaxed);
            drop(permit);
        });

        group_idx += 1;

        if target_tps > 0 {
            tokio::time::sleep(batch_interval).await;
        }
    }

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

            tracing::info!(
                sent, rpc_err, http_err,
                elapsed = format!("{:.0}s", elapsed),
                effective_tps = format!("{:.1}", sent as f64 / elapsed.max(1.0)),
                block, blocks,
                avg_tx_per_block = sent / blocks.max(1),
                rpc_lat_ms = format!("{:.1}", stats.avg_rpc_latency_ms()),
                sign_us = format!("{:.1}", stats.avg_sign_us()),
                pool_pending = pool.0,
                pool_queued = pool.1,
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
    // Limit analysis to last 50 blocks max
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

    // Per-block metrics
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

    // P50/P95 block TPS
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

    // Time-weighted TPS: total_tx / total_time_span
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

    tracing::info!("=== N42 TPS Stress Test v6 (Full Pipeline Profiling) ===");
    tracing::info!(
        target_tps = cli.target_tps,
        duration = cli.duration,
        accounts = cli.accounts,
        batch_size = cli.batch_size,
        accounts_per_batch = cli.accounts_per_batch,
        concurrency = cli.concurrency,
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

    // Parallel nonce sync
    sync_nonces_parallel(&accounts, &rpc_urls, &client).await;

    let start_block = get_block_number(&client, &rpc_urls[0]).await?;

    // Check initial gas limit
    if let Ok(info) = get_block_info(&client, &rpc_urls[0], start_block).await {
        tracing::info!(
            block = start_block,
            gas_limit = info.gas_limit,
            gas_limit_M = info.gas_limit / 1_000_000,
            max_eth_transfers = info.gas_limit / TRANSFER_GAS,
            "Chain active"
        );
    }

    // Check txpool
    if let Ok((pending, queued)) = get_txpool_status(&client, &rpc_urls[0]).await {
        tracing::info!(pending, queued, "Initial txpool status");
    }

    let stats = Arc::new(Stats::new());
    stats.start_block.store(start_block, Ordering::Relaxed);
    let semaphore = Arc::new(Semaphore::new(cli.concurrency));

    if cli.step {
        let tps_levels = [500, 1000, 2000, 3000, 5000, 7500, 10000];
        let step_duration = Duration::from_secs(60);

        for &target_tps in &tps_levels {
            // Parallel nonce sync
            sync_nonces_parallel(&accounts, &rpc_urls, &client).await;

            stats.reset();
            let sb = get_block_number(&client, &rpc_urls[0]).await?;
            stats.start_block.store(sb, Ordering::Relaxed);
            let start = Instant::now();

            // Adaptive accounts_per_batch: more accounts at higher TPS
            let apb = if target_tps >= 5000 {
                20
            } else if target_tps >= 2000 {
                10
            } else {
                cli.accounts_per_batch
            };

            tracing::info!("========================================");
            tracing::info!(
                target_tps,
                accounts_per_batch = apb,
                batch_size = cli.batch_size,
                "STEP TEST starting"
            );

            let reporter = spawn_reporter(
                stats.clone(), client.clone(), rpc_urls[0].clone(), start, sb,
            );

            run_test(
                &accounts, &targets, &rpc_urls, &client, &stats, &semaphore,
                target_tps, cli.batch_size, apb, step_duration,
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

            // Pipeline breakdown
            tracing::info!(
                target_tps,
                effective_tps = format!("{:.1}", effective_tps),
                sent, rpc_err, http_err, blocks,
                avg_tx_per_block = sent / blocks.max(1),
                fail_rate = format!("{:.1}%", fail_rate),
                avg_rpc_latency_ms = format!("{:.1}", stats.avg_rpc_latency_ms()),
                avg_sign_us = format!("{:.1}", stats.avg_sign_us()),
                "RESULT"
            );

            // Detailed block analysis
            analyze_blocks(&client, &rpc_urls[0], sb, end_block).await;

            // txpool snapshot after step
            if let Ok((pending, queued)) = get_txpool_status(&client, &rpc_urls[0]).await {
                tracing::info!(pending, queued, "txpool after step");
            }

            // Ceiling detection: check if blocks stopped advancing
            if blocks == 0 && target_tps > 500 {
                tracing::warn!(
                    target = target_tps,
                    "STALL DETECTED - no new blocks produced, stopping"
                );
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

            // Wait for pool to drain between steps (up to 60s)
            tracing::info!("Waiting for pool to drain...");
            let drain_start = Instant::now();
            loop {
                if drain_start.elapsed() > Duration::from_secs(60) {
                    tracing::warn!("Pool drain timeout (60s), continuing anyway");
                    break;
                }
                if let Ok((pending, queued)) = get_txpool_status(&client, &rpc_urls[0]).await {
                    if pending + queued < 1000 {
                        tracing::info!(pending, queued, elapsed_ms = drain_start.elapsed().as_millis(), "Pool drained");
                        break;
                    }
                    tracing::info!(pending, queued, "Draining pool...");
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    } else {
        let start = Instant::now();
        let reporter = spawn_reporter(
            stats.clone(), client.clone(), rpc_urls[0].clone(), start, start_block,
        );

        run_test(
            &accounts, &targets, &rpc_urls, &client, &stats, &semaphore,
            cli.target_tps, cli.batch_size, cli.accounts_per_batch,
            Duration::from_secs(cli.duration),
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
            "Complete"
        );

        analyze_blocks(&client, &rpc_urls[0], start_block, end_block).await;
    }

    Ok(())
}
