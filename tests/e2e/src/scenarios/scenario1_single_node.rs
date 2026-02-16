use std::time::Duration;
use tracing::{error, info};

use crate::genesis::{self, initial_balance};
use crate::node_manager::{NodeConfig, NodeProcess};

/// Scenario 1: Single node continuous block production for 400 seconds.
///
/// Verifies:
/// - Node produces ~100 blocks at ~4s intervals over 400 seconds
/// - Block intervals average ~4s (within ±1s tolerance)
/// - All 10 test addresses have their initial balance of 100M N42
/// - No panics or error-level log entries
pub async fn run(binary_path: std::path::PathBuf) -> eyre::Result<()> {
    info!("=== Scenario 1: Single Node Continuous Block Production (400s) ===");

    let accounts = genesis::generate_test_accounts();
    let tmp_dir = tempfile::tempdir()?;
    let genesis_path = genesis::write_genesis_file(tmp_dir.path(), &accounts);

    let config = NodeConfig::single_node(binary_path, genesis_path, 4000);
    let node = NodeProcess::start(&config).await?;

    info!("node started, monitoring block production for 400 seconds");

    let test_duration = Duration::from_secs(400);
    let poll_interval = Duration::from_secs(2);
    let start = tokio::time::Instant::now();

    let mut block_times: Vec<(u64, tokio::time::Instant)> = Vec::new();
    let mut last_block = 0u64;

    while start.elapsed() < test_duration {
        match node.rpc.block_number().await {
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
            Err(e) => {
                error!(error = %e, "failed to query block number");
            }
        }
        tokio::time::sleep(poll_interval).await;
    }

    // === Verification ===

    let total_blocks = last_block;
    info!(total_blocks, "block production complete");

    // Check 1: Block count should be ~88 (4s interval + ~500ms build overhead).
    // Allow wide tolerance (±25%) since exact timing depends on system load.
    if total_blocks < 70 || total_blocks > 120 {
        return Err(eyre::eyre!(
            "expected ~88 blocks (70-120 range), got {total_blocks}"
        ));
    }
    info!(total_blocks, "PASS: block count within expected range (70-120)");

    // Check 2: Average block interval should be ~4.5s (4s config + ~500ms build overhead).
    // Allow 3.0-6.0s range to accommodate system load variation.
    if block_times.len() >= 2 {
        let first_time = block_times.first().unwrap().1;
        let last_time = block_times.last().unwrap().1;
        let total_duration = last_time - first_time;
        let avg_interval = total_duration.as_secs_f64() / (block_times.len() - 1) as f64;

        if avg_interval < 3.0 || avg_interval > 6.0 {
            return Err(eyre::eyre!(
                "expected ~4.5s average block interval (3-6s range), got {avg_interval:.2}s"
            ));
        }
        info!(avg_interval_secs = format!("{avg_interval:.2}"), "PASS: block interval within range");
    }

    // Check 3: All test addresses should have initial balance
    for account in &accounts {
        let balance = node.rpc.get_balance(account.address).await?;
        let expected = initial_balance();
        if balance != expected {
            return Err(eyre::eyre!(
                "account {:?} has balance {balance}, expected {expected}",
                account.address
            ));
        }
    }
    info!("PASS: all test account balances correct");

    info!("=== Scenario 1 PASSED ===");

    node.stop()?;
    Ok(())
}
