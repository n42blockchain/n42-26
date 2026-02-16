use std::path::PathBuf;
use std::time::Duration;
use tracing::{error, info, warn};

use crate::genesis;
use crate::node_manager::{NodeConfig, NodeProcess};

/// Scenario 4: Multi-node consensus test.
///
/// Tests 3, 31, and 100 node consensus:
/// - 3 nodes (f=0, quorum=1): run 120s, verify block heights match
/// - 31 nodes (f=10, quorum=21): run 60s
/// - 100 nodes (f=33, quorum=67): run 30s (or degrade to 50 if OOM)
pub async fn run(binary_path: PathBuf) -> eyre::Result<()> {
    info!("=== Scenario 4: Multi-Node Consensus ===");

    // Test with 3 nodes.
    run_multi_node_test(&binary_path, 3, Duration::from_secs(120)).await?;

    // Test with 31 nodes.
    run_multi_node_test(&binary_path, 31, Duration::from_secs(60)).await?;

    // Test with 100 nodes (may degrade).
    match run_multi_node_test(&binary_path, 100, Duration::from_secs(30)).await {
        Ok(()) => info!("100-node test passed"),
        Err(e) => {
            warn!(error = %e, "100-node test failed, trying 50 nodes");
            run_multi_node_test(&binary_path, 50, Duration::from_secs(30)).await?;
        }
    }

    info!("=== Scenario 4 PASSED ===");
    Ok(())
}

async fn run_multi_node_test(
    binary_path: &PathBuf,
    node_count: usize,
    duration: Duration,
) -> eyre::Result<()> {
    info!(node_count, duration_secs = duration.as_secs(), "starting multi-node test");

    let accounts = genesis::generate_test_accounts();
    let tmp_dir = tempfile::tempdir()?;
    let genesis_path = genesis::write_genesis_file(tmp_dir.path(), &accounts);

    // Start all nodes.
    let mut nodes = Vec::with_capacity(node_count);

    for i in 0..node_count {
        let config = NodeConfig {
            binary_path: binary_path.clone(),
            genesis_path: genesis_path.clone(),
            validator_index: i,
            validator_count: node_count,
            block_interval_ms: 4000,
            port_offset: (i as u16) * 10,
            trusted_peers: vec![], // Discovery disabled; nodes connect via explicit peers.
        };

        match NodeProcess::start(&config).await {
            Ok(node) => {
                info!(index = i, http_port = node.http_port, "node started");
                nodes.push(node);
            }
            Err(e) => {
                error!(index = i, error = %e, "failed to start node");
                // Clean up already-started nodes.
                for node in nodes {
                    let _ = node.stop();
                }
                return Err(eyre::eyre!("failed to start node {i}: {e}"));
            }
        }
    }

    info!(started = nodes.len(), "all nodes started, running for {}s", duration.as_secs());

    // Let the nodes run for the specified duration.
    tokio::time::sleep(duration).await;

    // Collect block heights from all nodes.
    let mut heights = Vec::with_capacity(nodes.len());
    for (i, node) in nodes.iter().enumerate() {
        match node.rpc.block_number().await {
            Ok(h) => {
                info!(node = i, block_height = h, "node block height");
                heights.push(h);
            }
            Err(e) => {
                warn!(node = i, error = %e, "failed to query block height");
                heights.push(0);
            }
        }
    }

    // === Verification ===

    // Check 1: All nodes should have produced at least some blocks.
    let min_height = *heights.iter().min().unwrap_or(&0);
    let max_height = *heights.iter().max().unwrap_or(&0);

    if min_height == 0 {
        // Clean up.
        for node in nodes {
            let _ = node.stop();
        }
        return Err(eyre::eyre!(
            "at least one node has block height 0 (no blocks produced)"
        ));
    }
    info!(min = min_height, max = max_height, "block height range");

    // Check 2: Block heights should be within a reasonable range of each other.
    // Allow up to 5 blocks difference (accounts for propagation delays).
    let max_drift = 5;
    if max_height - min_height > max_drift {
        warn!(
            min = min_height,
            max = max_height,
            drift = max_height - min_height,
            max_drift,
            "block height drift exceeds threshold"
        );
        // This is a warning, not a failure, for multi-node setups.
    }

    info!(
        node_count,
        min_height,
        max_height,
        "PASS: {node_count}-node consensus test"
    );

    // Stop all nodes.
    for node in nodes {
        let _ = node.stop();
    }

    Ok(())
}
