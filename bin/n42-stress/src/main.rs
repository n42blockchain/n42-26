//! N42 High-Performance TPS Stress Test
//!
//! Uses alloy native signing + JSON-RPC batch requests + async concurrency
//! to saturate the chain's transaction processing capacity.

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

/// N42 TPS Stress Test
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
    #[arg(long, default_value = "10")]
    accounts: usize,

    /// Batch size for JSON-RPC batch requests
    #[arg(long, default_value = "50")]
    batch_size: usize,

    /// Max concurrent HTTP requests across all RPC endpoints
    #[arg(long, default_value = "64")]
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
const TRANSFER_VALUE: u128 = 1_000_000_000_000; // 0.000001 N42

struct Stats {
    sent: AtomicU64,
    failed: AtomicU64,
    start_block: AtomicU64,
}

impl Stats {
    fn new() -> Self {
        Self {
            sent: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            start_block: AtomicU64::new(0),
        }
    }

    fn reset(&self) {
        self.sent.store(0, Ordering::Relaxed);
        self.failed.store(0, Ordering::Relaxed);
    }
}

struct TestAccount {
    signer: PrivateKeySigner,
    address: Address,
    nonce: AtomicU64,
}

/// Derive test account private key using Keccak-256 (matching genesis.rs)
fn derive_private_key(index: usize) -> [u8; 32] {
    let seed = format!("n42-test-key-{}", index);
    let mut hasher = Keccak::v256();
    hasher.update(seed.as_bytes());
    let mut output = [0u8; 32];
    hasher.finalize(&mut output);
    output
}

fn create_accounts(count: usize) -> Vec<Arc<TestAccount>> {
    (0..count)
        .map(|i| {
            let pk = derive_private_key(i);
            let signer = PrivateKeySigner::from_bytes(&pk.into()).expect("valid key");
            let address = signer.address();
            Arc::new(TestAccount {
                signer,
                address,
                nonce: AtomicU64::new(0),
            })
        })
        .collect()
}

/// Sign a batch of transactions for one account, returning raw RLP-encoded hex strings.
fn sign_batch(
    account: &TestAccount,
    targets: &[Address],
    batch_size: usize,
) -> Vec<String> {
    let mut result = Vec::with_capacity(batch_size);
    for i in 0..batch_size {
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
    result
}

/// Send a batch of raw transactions via JSON-RPC batch request.
async fn send_batch(
    client: &reqwest::Client,
    rpc_url: &str,
    raw_txs: &[String],
) -> (usize, usize) {
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

    match client.post(rpc_url).json(&batch).send().await {
        Ok(resp) => {
            match resp.json::<Vec<serde_json::Value>>().await {
                Ok(results) => {
                    let mut ok = 0;
                    let mut fail = 0;
                    for r in &results {
                        if r.get("error").is_some() {
                            fail += 1;
                        } else {
                            ok += 1;
                        }
                    }
                    (ok, fail)
                }
                Err(_) => (0, raw_txs.len()),
            }
        }
        Err(_) => (0, raw_txs.len()),
    }
}

async fn get_nonce(client: &reqwest::Client, rpc_url: &str, address: &Address) -> Result<u64> {
    let payload = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "eth_getTransactionCount",
        "params": [format!("{address:?}"), "pending"],
        "id": 1
    });
    let resp: serde_json::Value = client.post(rpc_url).json(&payload).send().await?.json().await?;
    let hex = resp["result"].as_str().unwrap_or("0x0");
    Ok(u64::from_str_radix(hex.trim_start_matches("0x"), 16)?)
}

async fn get_block_number(client: &reqwest::Client, rpc_url: &str) -> Result<u64> {
    let payload = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "eth_blockNumber",
        "params": [],
        "id": 1
    });
    let resp: serde_json::Value = client.post(rpc_url).json(&payload).send().await?.json().await?;
    let hex = resp["result"].as_str().unwrap_or("0x0");
    Ok(u64::from_str_radix(hex.trim_start_matches("0x"), 16)?)
}

async fn run_test(
    accounts: &[Arc<TestAccount>],
    targets: &[Address],
    rpc_urls: &[String],
    client: &reqwest::Client,
    stats: &Arc<Stats>,
    semaphore: &Arc<Semaphore>,
    target_tps: u64,
    batch_size: usize,
    duration: Duration,
) {
    let start = Instant::now();
    let num_accounts = accounts.len();
    // How many batches per second total to hit target TPS
    let batches_per_sec = if target_tps > 0 {
        (target_tps as f64 / batch_size as f64).ceil() as u64
    } else {
        10000 // unlimited
    };
    let batch_interval = Duration::from_secs_f64(1.0 / batches_per_sec as f64);

    let mut batch_idx: usize = 0;

    while start.elapsed() < duration {
        let account = &accounts[batch_idx % num_accounts];
        let rpc_url = rpc_urls[batch_idx % rpc_urls.len()].clone();

        // Sign batch (CPU-bound, but fast in Rust)
        let raw_txs = sign_batch(account, targets, batch_size);

        // Acquire semaphore permit for concurrency control
        let permit = semaphore.clone().acquire_owned().await.unwrap();
        let client = client.clone();
        let stats = stats.clone();

        tokio::spawn(async move {
            let (ok, fail) = send_batch(&client, &rpc_url, &raw_txs).await;
            stats.sent.fetch_add(ok as u64, Ordering::Relaxed);
            stats.failed.fetch_add(fail as u64, Ordering::Relaxed);
            drop(permit);
        });

        batch_idx += 1;

        // Rate limiting
        if target_tps > 0 {
            tokio::time::sleep(batch_interval).await;
        }
    }

    // Wait for in-flight requests
    let _ = semaphore.clone().acquire_many(semaphore.available_permits() as u32).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
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

    tracing::info!("=== N42 Rust TPS Stress Test ===");
    tracing::info!(
        target_tps = cli.target_tps,
        duration = cli.duration,
        accounts = cli.accounts,
        batch_size = cli.batch_size,
        concurrency = cli.concurrency,
        rpc_count = rpc_urls.len(),
        "Configuration"
    );

    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(16)
        .timeout(Duration::from_secs(10))
        .build()?;

    // Create accounts
    let accounts = create_accounts(cli.accounts);
    let targets: Vec<Address> = accounts.iter().map(|a| a.address).collect();

    // Sync nonces
    for acct in &accounts {
        let nonce = get_nonce(&client, &rpc_urls[0], &acct.address).await?;
        acct.nonce.store(nonce, Ordering::Relaxed);
        tracing::info!(index = ?accounts.iter().position(|a| a.address == acct.address),
                       address = ?acct.address, nonce, "Account ready");
    }

    let start_block = get_block_number(&client, &rpc_urls[0]).await?;
    tracing::info!(block = start_block, "Chain active");

    let stats = Arc::new(Stats::new());
    stats.start_block.store(start_block, Ordering::Relaxed);
    let semaphore = Arc::new(Semaphore::new(cli.concurrency));

    if cli.step {
        // Step mode
        let tps_levels = [100, 200, 300, 500, 750, 1000, 1500, 2000, 3000];
        let step_duration = Duration::from_secs(60);

        for &target_tps in &tps_levels {
            // Re-sync nonces
            for acct in &accounts {
                let nonce = get_nonce(&client, &rpc_urls[0], &acct.address).await?;
                acct.nonce.store(nonce, Ordering::Relaxed);
            }

            stats.reset();
            let sb = get_block_number(&client, &rpc_urls[0]).await?;
            stats.start_block.store(sb, Ordering::Relaxed);
            let start = Instant::now();

            tracing::info!("========================================");
            tracing::info!(target_tps, "STEP TEST starting");

            // Stats reporter
            let stats_clone = stats.clone();
            let client_clone = client.clone();
            let rpc_clone = rpc_urls[0].clone();
            let sb_copy = sb;
            let reporter = tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_secs(10));
                loop {
                    interval.tick().await;
                    let sent = stats_clone.sent.load(Ordering::Relaxed);
                    let failed = stats_clone.failed.load(Ordering::Relaxed);
                    let elapsed = start.elapsed().as_secs_f64();
                    let block = get_block_number(&client_clone, &rpc_clone).await.unwrap_or(0);
                    let blocks = block.saturating_sub(sb_copy);
                    tracing::info!(
                        sent, failed, elapsed = format!("{:.0}s", elapsed),
                        effective_tps = format!("{:.1}", sent as f64 / elapsed.max(1.0)),
                        block, blocks, avg_tx_per_block = sent / blocks.max(1),
                        "STATS"
                    );
                }
            });

            run_test(
                &accounts, &targets, &rpc_urls, &client, &stats, &semaphore,
                target_tps, cli.batch_size, step_duration,
            ).await;

            reporter.abort();

            let sent = stats.sent.load(Ordering::Relaxed);
            let failed = stats.failed.load(Ordering::Relaxed);
            let elapsed = start.elapsed().as_secs_f64();
            let end_block = get_block_number(&client, &rpc_urls[0]).await?;
            let blocks = end_block - sb;
            let effective_tps = sent as f64 / elapsed.max(1.0);
            let avg_tx_block = sent / blocks.max(1);
            let fail_rate = if sent + failed > 0 {
                (failed as f64 / (sent + failed) as f64) * 100.0
            } else {
                0.0
            };

            tracing::info!(
                target_tps, effective_tps = format!("{:.1}", effective_tps),
                sent, failed, blocks, avg_tx_per_block = avg_tx_block,
                fail_rate = format!("{:.1}%", fail_rate),
                "RESULT"
            );

            // Ceiling detection
            if effective_tps < target_tps as f64 * 0.7 && target_tps > 200 {
                tracing::info!(
                    effective = format!("{:.0}", effective_tps),
                    target = target_tps,
                    "CEILING DETECTED — effective TPS much lower than target"
                );
                break;
            }

            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    } else {
        // Single target mode
        let start = Instant::now();

        // Stats reporter
        let stats_clone = stats.clone();
        let client_clone = client.clone();
        let rpc_clone = rpc_urls[0].clone();
        let reporter = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(10));
            loop {
                interval.tick().await;
                let sent = stats_clone.sent.load(Ordering::Relaxed);
                let failed = stats_clone.failed.load(Ordering::Relaxed);
                let elapsed = start.elapsed().as_secs_f64();
                let block = get_block_number(&client_clone, &rpc_clone).await.unwrap_or(0);
                let blocks = block.saturating_sub(start_block);
                tracing::info!(
                    sent, failed, elapsed = format!("{:.0}s", elapsed),
                    effective_tps = format!("{:.1}", sent as f64 / elapsed.max(1.0)),
                    block, blocks, avg_tx_per_block = sent / blocks.max(1),
                    "STATS"
                );
            }
        });

        run_test(
            &accounts, &targets, &rpc_urls, &client, &stats, &semaphore,
            cli.target_tps, cli.batch_size, Duration::from_secs(cli.duration),
        ).await;

        reporter.abort();

        let sent = stats.sent.load(Ordering::Relaxed);
        let failed = stats.failed.load(Ordering::Relaxed);
        let elapsed = start.elapsed().as_secs_f64();
        let end_block = get_block_number(&client, &rpc_urls[0]).await?;
        let blocks = end_block - start_block;

        tracing::info!("=== FINAL RESULT ===");
        tracing::info!(
            target_tps = cli.target_tps,
            effective_tps = format!("{:.1}", sent as f64 / elapsed.max(1.0)),
            sent, failed, blocks,
            avg_tx_per_block = sent / blocks.max(1),
            duration = format!("{:.0}s", elapsed),
            "Complete"
        );
    }

    Ok(())
}
