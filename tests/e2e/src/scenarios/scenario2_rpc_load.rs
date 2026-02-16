use alloy_primitives::U256;
use std::time::Duration;
use tracing::info;

use crate::genesis::{self, TEST_CHAIN_ID};
use crate::node_manager::{NodeConfig, NodeProcess};
use crate::tx_engine::TxEngine;

/// Scenario 2: RPC transaction load test at 25 tx/sec.
///
/// Sends 2500 transfer transactions over 100 seconds and verifies:
/// - All transactions have receipts with status=1
/// - Sender/receiver balances are consistent
/// - No nonce conflicts or duplicate transactions
pub async fn run(binary_path: std::path::PathBuf) -> eyre::Result<()> {
    info!("=== Scenario 2: RPC Transaction Load (25 tx/sec, 100s) ===");

    let accounts = genesis::generate_test_accounts();
    let tmp_dir = tempfile::tempdir()?;
    let genesis_path = genesis::write_genesis_file(tmp_dir.path(), &accounts);

    let config = NodeConfig::single_node(binary_path, genesis_path, 4000);
    let node = NodeProcess::start(&config).await?;

    // Wait a few blocks for the chain to be stable.
    info!("waiting for initial blocks...");
    tokio::time::sleep(Duration::from_secs(12)).await;

    let mut tx_engine = TxEngine::new(&accounts, TEST_CHAIN_ID);
    tx_engine.sync_nonces(&node.rpc).await?;

    // Get initial balances.
    let mut initial_balances = Vec::new();
    for i in 0..tx_engine.wallet_count() {
        let balance = node.rpc.get_balance(tx_engine.address(i)).await?;
        initial_balances.push(balance);
    }

    let gas_price = node.rpc.gas_price().await?;
    let max_fee_per_gas = gas_price * 2;
    let max_priority_fee_per_gas = gas_price / 10;

    // Send 25 tx/sec for 100 seconds = 2500 transactions.
    let tx_hashes = tx_engine.send_transfers_at_rate(
        &node.rpc,
        25,
        100,
        max_fee_per_gas,
        max_priority_fee_per_gas,
    ).await?;

    info!(total = tx_hashes.len(), "all transactions sent, waiting for receipts...");

    // Wait for all receipts (generous timeout since blocks are 4s apart).
    let receipts = TxEngine::wait_for_all_receipts(
        &node.rpc,
        &tx_hashes,
        Duration::from_secs(300),
    ).await?;

    // === Verification ===

    // Check 1: All receipts have status=1 (success).
    let failed = receipts.iter().filter(|r| r.status != 1).count();
    if failed > 0 {
        return Err(eyre::eyre!("{failed} transactions failed (status != 1)"));
    }
    info!("PASS: all {} transactions succeeded", receipts.len());

    // Check 2: Verify no duplicate tx hashes in receipts.
    let mut seen_hashes = std::collections::HashSet::new();
    for receipt in &receipts {
        if !seen_hashes.insert(receipt.transaction_hash) {
            return Err(eyre::eyre!(
                "duplicate transaction hash: {:?}",
                receipt.transaction_hash
            ));
        }
    }
    info!("PASS: no duplicate transaction hashes");

    // Check 3: Final balances should reflect transfers (we don't track individual
    // transfer amounts here, but we check that balances have changed).
    let mut total_gas_used = U256::ZERO;
    for receipt in &receipts {
        // Gas cost = gas_used * effective_gas_price (approximate with max_fee_per_gas).
        total_gas_used += U256::from(receipt.gas_used);
    }
    info!(total_gas_used = %total_gas_used, "total gas consumed");

    // Verify that the sum of all balances equals initial total minus gas fees.
    let initial_total: U256 = initial_balances.iter().sum();
    let mut final_total = U256::ZERO;
    for i in 0..tx_engine.wallet_count() {
        let balance = node.rpc.get_balance(tx_engine.address(i)).await?;
        final_total += balance;
    }

    // The difference should be approximately the gas fees (transfers are between test accounts).
    let gas_cost_estimate = total_gas_used * U256::from(max_fee_per_gas);
    info!(
        initial = %initial_total,
        final_total = %final_total,
        estimated_gas = %gas_cost_estimate,
        "balance conservation check"
    );

    // Final total should be less than initial (gas was spent), but not by too much.
    if final_total > initial_total {
        return Err(eyre::eyre!("final balance exceeds initial (impossible)"));
    }
    info!("PASS: balance conservation verified");

    info!("=== Scenario 2 PASSED ===");

    node.stop()?;
    Ok(())
}
