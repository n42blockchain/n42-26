use std::time::Duration;
use tracing::{error, info};

use crate::genesis::{self, initial_balance};
use crate::node_manager::{NodeConfig, NodeProcess};

/// Scenario 1: Single node continuous block production for 400 seconds.
///
/// Verifies:
/// - Node produces ~88 blocks at ~4s intervals (3.0-6.0s tolerance for variance)
/// - All 10 test addresses have their initial balance of 100M N42
pub async fn run(binary_path: std::path::PathBuf) -> eyre::Result<()> {
    info!("=== Scenario 1: Single Node Continuous Block Production (400s) ===");

    let accounts = genesis::generate_test_accounts();
    let tmp_dir = tempfile::tempdir()?;
    let genesis_path = genesis::write_genesis_file(tmp_dir.path(), &accounts);

    let config = NodeConfig::single_node(binary_path, genesis_path, 4000);
    let node = NodeProcess::start(&config).await?;

    info!("node started, monitoring block production for 400 seconds");

    let (total_blocks, block_times) = poll_block_production(
        &node.rpc,
        Duration::from_secs(400),
        Duration::from_secs(2),
    )
    .await?;

    verify_block_production(&node.rpc, &accounts, total_blocks, &block_times).await?;

    info!("=== Scenario 1 PASSED ===");
    node.stop()?;
    Ok(())
}

async fn poll_block_production(
    rpc: &crate::rpc_client::RpcClient,
    test_duration: Duration,
    poll_interval: Duration,
) -> eyre::Result<(u64, Vec<(u64, tokio::time::Instant)>)> {
    let start = tokio::time::Instant::now();
    let mut block_times: Vec<(u64, tokio::time::Instant)> = Vec::new();
    let mut last_block = 0u64;

    while start.elapsed() < test_duration {
        match rpc.block_number().await {
            Ok(current) => {
                if current > last_block {
                    let now = tokio::time::Instant::now();
                    for b in (last_block + 1)..=current {
                        block_times.push((b, now));
                    }
                    last_block = current;
                    info!(block = current, elapsed_secs = start.elapsed().as_secs(), "new block");
                }
            }
            Err(e) => error!(error = %e, "failed to query block number"),
        }
        tokio::time::sleep(poll_interval).await;
    }

    Ok((last_block, block_times))
}

async fn verify_block_production(
    rpc: &crate::rpc_client::RpcClient,
    accounts: &[genesis::TestAccount],
    total_blocks: u64,
    block_times: &[(u64, tokio::time::Instant)],
) -> eyre::Result<()> {
    info!(total_blocks, "block production complete");

    if !(70..=120).contains(&total_blocks) {
        return Err(eyre::eyre!(
            "expected ~88 blocks (70-120 range), got {total_blocks}"
        ));
    }
    info!(total_blocks, "PASS: block count within expected range (70-120)");

    if block_times.len() >= 2 {
        let first_time = block_times.first().unwrap().1;
        let last_time = block_times.last().unwrap().1;
        let total_duration = last_time - first_time;
        let avg_interval = total_duration.as_secs_f64() / (block_times.len() - 1) as f64;

        if !(3.0..=6.0).contains(&avg_interval) {
            return Err(eyre::eyre!(
                "expected ~4.5s average block interval (3-6s range), got {avg_interval:.2}s"
            ));
        }
        info!(avg_interval_secs = format!("{avg_interval:.2}"), "PASS: block interval within range");
    }

    for account in accounts {
        let balance = rpc.get_balance(account.address).await?;
        let expected = initial_balance();
        if balance != expected {
            return Err(eyre::eyre!(
                "account {:?} has balance {balance}, expected {expected}",
                account.address
            ));
        }
    }
    info!("PASS: all test account balances correct");

    Ok(())
}
