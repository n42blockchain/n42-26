use alloy_primitives::U256;
use std::time::Duration;
use tracing::info;

use crate::erc20::Erc20Manager;
use crate::genesis::{self, TEST_CHAIN_ID};
use crate::node_manager::{NodeConfig, NodeProcess};
use crate::tx_engine::TxEngine;

/// Scenario 3: ERC-20 USDT contract deployment and transfer load.
///
/// Verifies:
/// - ERC-20 contract deploys successfully
/// - 5 tx/sec ERC-20 transfers execute correctly over 60 seconds
/// - balanceOf queries return expected values
/// - Total supply is conserved
pub async fn run(binary_path: std::path::PathBuf) -> eyre::Result<()> {
    info!("=== Scenario 3: ERC-20 USDT Contract (5 tx/sec, 60s) ===");

    let accounts = genesis::generate_test_accounts();
    let tmp_dir = tempfile::tempdir()?;
    let genesis_path = genesis::write_genesis_file(tmp_dir.path(), &accounts);

    let config = NodeConfig::single_node(binary_path, genesis_path, 4000);
    let node = NodeProcess::start(&config).await?;

    // Wait for initial blocks.
    info!("waiting for initial blocks...");
    tokio::time::sleep(Duration::from_secs(12)).await;

    let mut tx_engine = TxEngine::new(&accounts, TEST_CHAIN_ID);
    tx_engine.sync_nonces(&node.rpc).await?;

    let gas_price = node.rpc.gas_price().await?;
    let max_fee = gas_price * 2;
    let priority_fee = gas_price / 10;

    // Deploy the ERC-20 contract.
    let erc20 = Erc20Manager::deploy(
        &mut tx_engine,
        &node.rpc,
        0, // deployer is account[0]
        max_fee,
        priority_fee,
    ).await?;

    info!(contract = %erc20.contract_address, "contract deployed");

    // Verify initial state.
    let total_supply = erc20.total_supply(&node.rpc).await?;
    info!(total_supply = %total_supply, "initial total supply");

    let deployer_balance = erc20.balance_of(&node.rpc, tx_engine.address(0)).await?;
    info!(deployer_balance = %deployer_balance, "deployer token balance");

    if deployer_balance != total_supply {
        return Err(eyre::eyre!(
            "deployer should have all tokens: got {deployer_balance}, expected {total_supply}"
        ));
    }
    info!("PASS: deployer has all tokens");

    // Send ERC-20 transfers at 5 tx/sec for 60 seconds = 300 transfers.
    let tx_per_second = 5u32;
    let duration_secs = 60u64;
    let total_tx = (tx_per_second as u64) * duration_secs;
    let interval = Duration::from_millis(1000 / tx_per_second as u64);

    let mut tx_hashes = Vec::with_capacity(total_tx as usize);
    let transfer_amount = U256::from(1_000_000u64); // 1 USDT (6 decimals)

    info!(total_tx, "starting ERC-20 transfer batch");

    for i in 0..total_tx {
        // All transfers from account[0] to account[1..9] round-robin.
        let to_idx = ((i as usize) % 9) + 1;
        let to_addr = tx_engine.address(to_idx);

        let (tx_hash, raw_tx) = erc20.build_transfer_tx(
            &mut tx_engine,
            0,
            to_addr,
            transfer_amount,
            max_fee,
            priority_fee,
        )?;

        node.rpc.send_raw_transaction(&raw_tx).await?;
        tx_hashes.push(tx_hash);

        if (i + 1) % 50 == 0 {
            info!(sent = i + 1, total = total_tx, "ERC-20 transfer progress");
        }

        tokio::time::sleep(interval).await;
    }

    info!("all ERC-20 transfers sent, waiting for receipts...");

    let receipts = TxEngine::wait_for_all_receipts(
        &node.rpc,
        &tx_hashes,
        Duration::from_secs(300),
    ).await?;

    // === Verification ===

    // Check 1: All receipts have status=1.
    let failed = receipts.iter().filter(|r| r.status != 1).count();
    if failed > 0 {
        return Err(eyre::eyre!("{failed} ERC-20 transfers failed"));
    }
    info!("PASS: all {} ERC-20 transfers succeeded", receipts.len());

    // Check 2: Verify token balances.
    let deployer_final = erc20.balance_of(&node.rpc, tx_engine.address(0)).await?;
    let expected_spent = transfer_amount * U256::from(total_tx);
    let expected_deployer = deployer_balance - expected_spent;

    if deployer_final != expected_deployer {
        return Err(eyre::eyre!(
            "deployer balance mismatch: got {deployer_final}, expected {expected_deployer}"
        ));
    }
    info!("PASS: deployer token balance correct");

    // Check 3: Each recipient should have received their share.
    let mut recipient_total = U256::ZERO;
    for i in 1..tx_engine.wallet_count() {
        let balance = erc20.balance_of(&node.rpc, tx_engine.address(i)).await?;
        recipient_total += balance;
    }

    if deployer_final + recipient_total != total_supply {
        return Err(eyre::eyre!(
            "total supply not conserved: deployer={deployer_final} + recipients={recipient_total} != {total_supply}"
        ));
    }
    info!("PASS: total supply conserved");

    info!("=== Scenario 3 PASSED ===");

    node.stop()?;
    Ok(())
}
