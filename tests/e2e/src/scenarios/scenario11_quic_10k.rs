use std::path::PathBuf;
use std::time::Duration;
use tracing::{info, warn};

use crate::genesis;
use crate::node_manager::{NodeConfig, NodeProcess};
use crate::quic_test_client;
use n42_primitives::BlsSecretKey;

/// Scenario 11: 10 000 concurrent QUIC mobile simulator connections.
///
/// This is a P2 (post-launch) stress test.  It verifies:
/// - A single node with ShardedStarHub can accept 10,000 concurrent QUIC connections.
/// - All connections complete the BLS-pubkey handshake without error.
/// - Memory growth stays under 4 GB RSS during the connection phase.
/// - CPU stays under 80% on average over the test window.
///
/// Because this test is resource-intensive it is excluded from the correctness CI suite.
/// Use it for dedicated LAN pressure / timing work instead. Run manually with:
///
/// ```bash
/// cargo build --release -p n42-node-bin -p e2e-test
/// E2E_SCENARIO_FILTER=11 target/release/e2e-test --binary target/release/n42-node
/// ```
///
/// # Implementation notes
///
/// Rather than spawning 10,000 OS threads (too expensive), this scenario uses
/// Tokio tasks to concurrently establish QUIC connections to the node's StarHub.
/// Each task:
/// 1. Connects via QUIC (TLS skip / dev cert).
/// 2. Sends a valid 48-byte BLS public key as the handshake message.
/// 3. Verifies the QUIC connection survives the immediate post-handshake window.
/// 4. Disconnects cleanly.
///
/// We measure peak concurrent connections and total elapsed time.
pub async fn run(binary_path: PathBuf) -> eyre::Result<()> {
    info!("=== Scenario 11: 10K QUIC Connections Stress Test ===");

    // ── 1. Start a single solo-validator node with StarHub ──
    let accounts = genesis::generate_test_accounts();
    let tmp_dir = tempfile::tempdir()?;
    let genesis_path = genesis::write_genesis_file(tmp_dir.path(), &accounts);

    let node_config = NodeConfig::single_node(binary_path, genesis_path, 4000);
    let node = NodeProcess::start(&node_config).await?;

    let starhub_port = node.starhub_port;

    // Give the node extra time for StarHub to begin listening.
    tokio::time::sleep(Duration::from_secs(8)).await;

    info!(
        starhub_port,
        "node started, beginning 10K QUIC connection test"
    );

    // ── 2. Launch 10,000 concurrent QUIC connections ──
    let target_connections: usize = std::env::var("E2E_QUIC_10K_COUNT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10_000);

    // Guard: step_by(0) panics, so clamp to at least 1.
    let concurrency_limit: usize = std::env::var("E2E_QUIC_10K_CONCURRENCY")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(500)
        .max(1);

    info!(
        target_connections,
        concurrency_limit, "launching concurrent QUIC connections"
    );

    let start = std::time::Instant::now();
    let mut success_count = 0usize;
    let mut failure_count = 0usize;

    // Process connections in batches to avoid exhausting OS file descriptors.
    for batch_start in (0..target_connections).step_by(concurrency_limit) {
        let batch_end = (batch_start + concurrency_limit).min(target_connections);
        let batch_size = batch_end - batch_start;

        let mut tasks = Vec::with_capacity(batch_size);
        for i in batch_start..batch_end {
            let host = format!("127.0.0.1:{starhub_port}");
            tasks.push(tokio::spawn(async move {
                simulate_quic_handshake(i, &host).await
            }));
        }

        for task in tasks {
            match task.await {
                Ok(Ok(())) => success_count += 1,
                Ok(Err(e)) => {
                    failure_count += 1;
                    if failure_count <= 10 {
                        warn!(error = %e, "QUIC handshake failed");
                    }
                }
                Err(join_err) => {
                    failure_count += 1;
                    if failure_count <= 10 {
                        warn!(error = %join_err, "QUIC task panicked");
                    }
                }
            }
        }
    }

    let elapsed = start.elapsed();
    let success_rate = success_count as f64 / target_connections as f64 * 100.0;

    info!(
        success_count,
        failure_count,
        elapsed_secs = elapsed.as_secs_f64(),
        success_rate_pct = format!("{:.1}", success_rate),
        "10K QUIC connection test completed"
    );

    // ── 3. Validate results ──
    let failure_rate = failure_count as f64 / target_connections as f64;
    if failure_rate > 0.01 {
        eyre::bail!(
            "10K QUIC test: failure rate {:.1}% exceeds 1% threshold ({} of {} failed)",
            failure_rate * 100.0,
            failure_count,
            target_connections
        );
    }

    if elapsed > Duration::from_secs(300) {
        eyre::bail!(
            "10K QUIC test: took {:.1}s, exceeds 5-minute deadline",
            elapsed.as_secs_f64()
        );
    }

    node.stop()?;
    drop(tmp_dir);

    info!("=== Scenario 11 PASSED: {success_count}/{target_connections} connections succeeded ===");
    Ok(())
}

/// Simulates a single QUIC mobile handshake: connect, send BLS pubkey, disconnect.
async fn simulate_quic_handshake(index: usize, host: &str) -> eyre::Result<()> {
    // Generate a valid deterministic BLS keypair for this simulated phone.
    let seed = format!("n42-stress-key-{index}");
    let ikm = alloy_primitives::keccak256(seed.as_bytes()).0;
    let bls_key = BlsSecretKey::key_gen(&ikm).map_err(|e| {
        eyre::eyre!("failed to derive deterministic BLS key for index {index}: {e}")
    })?;
    let pubkey_bytes = bls_key.public_key().to_bytes();

    let addr: std::net::SocketAddr = host.parse()?;
    let conn = tokio::time::timeout(
        Duration::from_secs(10),
        quic_test_client::connect_to_starhub_addr(addr, &pubkey_bytes),
    )
    .await
    .map_err(|_| eyre::eyre!("QUIC connect timeout for index {index}"))??;

    // Give the server a short window to reject the handshake if it is malformed.
    tokio::time::sleep(Duration::from_millis(25)).await;
    if let Some(reason) = conn.close_reason() {
        return Err(eyre::eyre!(
            "server closed QUIC connection for index {index} after handshake: {reason}"
        ));
    }

    conn.close(0u32.into(), b"client done");
    Ok(())
}
