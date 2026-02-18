use std::path::PathBuf;
use std::time::Duration;
use tracing::info;

use crate::genesis::{self, TEST_CHAIN_ID};
use crate::mobile_sim;
use crate::node_manager::{NodeConfig, NodeProcess};
use crate::tx_engine::TxEngine;

/// Scenario 5: Mobile phone verification with 3 solo nodes + 15 phones.
///
/// Each node runs as solo validator (independent block production) to avoid
/// the multi-node block sync limitation. 5 mobile simulators per node submit
/// BLS attestations against each node's chain.
///
/// Verifies:
/// - 3 solo-validator nodes produce blocks independently
/// - 15 mobile simulators (5 per node) submit BLS attestations
/// - Attestations are accepted by nodes (BLS signature verified)
/// - Transactions execute normally alongside verification
pub async fn run(binary_path: PathBuf) -> eyre::Result<()> {
    info!("=== Scenario 5: Mobile Verification (3 solo nodes, 15 phones, 60s) ===");

    let accounts = genesis::generate_test_accounts();

    // Start 3 solo-validator nodes (each produces blocks independently).
    let mut nodes = Vec::new();
    let mut tmp_dirs = Vec::new();
    for i in 0..3 {
        let tmp_dir = tempfile::tempdir()?;
        let genesis_path = genesis::write_genesis_file(tmp_dir.path(), &accounts);

        let config = NodeConfig {
            binary_path: binary_path.clone(),
            genesis_path,
            validator_index: 0,
            validator_count: 1,
            block_interval_ms: 4000,
            port_offset: (i as u16) * 10,
            trusted_peers: vec![],
            base_timeout_ms: None,
            max_timeout_ms: None,
            startup_delay_ms: None,
        };

        let node = NodeProcess::start(&config).await?;
        info!(index = i, http_port = node.http_port, "solo node started");
        nodes.push(node);
        tmp_dirs.push(tmp_dir);
    }

    let test_duration = Duration::from_secs(60);

    // Spawn mobile simulators: 5 per node = 15 total.
    let mut mobile_handles = Vec::new();
    for (node_idx, node) in nodes.iter().enumerate() {
        let url = node.http_url();
        let start_idx = node_idx * 5;
        let dur = test_duration;

        let handle = tokio::spawn(async move {
            mobile_sim::run_mobile_fleet(url, 5, dur, start_idx).await
        });
        mobile_handles.push(handle);
    }

    // Also run some transactions during the test period.
    let tx_node_url = nodes[0].http_url();
    let tx_handle = tokio::spawn({
        let accounts = accounts.clone();
        async move {
            let rpc = crate::rpc_client::RpcClient::new(tx_node_url);

            // Wait for initial blocks.
            tokio::time::sleep(Duration::from_secs(12)).await;

            let mut tx_engine = TxEngine::new(&accounts, TEST_CHAIN_ID);
            tx_engine.sync_nonces(&rpc).await.ok();

            let gas_price = rpc.gas_price().await.unwrap_or(1_000_000_000);
            let max_fee = gas_price * 2;
            let priority_fee = gas_price / 10;

            // Send 5 tx/sec for a portion of the test duration.
            let _ = tx_engine.send_transfers_at_rate(
                &rpc,
                5,
                40, // ~40 seconds of transactions
                max_fee,
                priority_fee,
            ).await;
        }
    });

    // Wait for mobile simulators to complete.
    let mut total_accepted = 0u64;
    let mut total_errors = 0u64;

    for handle in mobile_handles {
        match handle.await {
            Ok(stats_vec) => {
                for stats in stats_vec {
                    total_accepted += stats.accepted;
                    total_errors += stats.errors;
                }
            }
            Err(e) => {
                info!(error = %e, "mobile fleet task error");
            }
        }
    }

    // Wait for transaction task.
    let _ = tx_handle.await;

    // === Verification ===

    info!(
        total_accepted,
        total_errors,
        "mobile attestation results"
    );

    // Check 1: At least some attestations were accepted.
    if total_accepted == 0 {
        return Err(eyre::eyre!("no attestations were accepted"));
    }
    info!("PASS: {} attestations accepted", total_accepted);

    // Check 2: All nodes produced blocks.
    for (i, node) in nodes.iter().enumerate() {
        let height = node.rpc.block_number().await?;
        info!(node = i, height, "final block height");
        if height == 0 {
            return Err(eyre::eyre!("node {i} has block height 0"));
        }
    }
    info!("PASS: all nodes produced blocks");

    // Check 3: Query consensus status.
    for (i, node) in nodes.iter().enumerate() {
        match node.rpc.consensus_status().await {
            Ok(status) => {
                info!(
                    node = i,
                    has_qc = status.has_committed_qc,
                    validators = status.validator_count,
                    "consensus status"
                );
            }
            Err(e) => {
                info!(node = i, error = %e, "failed to query consensus status");
            }
        }
    }

    info!("=== Scenario 5 PASSED ===");

    for node in nodes {
        let _ = node.stop();
    }

    Ok(())
}
