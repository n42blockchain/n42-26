use std::path::PathBuf;
use std::time::Duration;
use tracing::{info, warn};

use alloy_primitives::U256;
use n42_mobile::code_cache::CodeCache;
use n42_mobile::packet::{decode_packet, decode_stream_packet};
use n42_mobile::receipt::{VerificationReceipt, sign_receipt};
use n42_mobile::verifier::{
    update_cache_after_stream_verify, update_cache_after_verify, verify_block, verify_block_stream,
};
use n42_primitives::BlsSecretKey;

use crate::erc20::Erc20Manager;
use crate::genesis::{self, TEST_CHAIN_ID};
use crate::node_manager::{NodeConfig, NodeProcess};
use crate::quic_test_client;
use crate::rpc_client::RpcClient;
use crate::tx_engine::TxEngine;

/// Scenario 8: Mobile EVM Verification via QUIC.
///
/// Tests the mobile verification pipeline:
/// 1. Start single-node chain, connect via QUIC
/// 2. Deploy ERC-20 contract, send ETH + ERC-20 transfers
/// 3. Receive VerificationPackets, run EVM re-execution
/// 4. Verify receipts_root match, sign with BLS12-381
/// 5. Send receipts back via QUIC
pub async fn run(binary_path: PathBuf) -> eyre::Result<()> {
    info!("=== Scenario 8: Mobile EVM Verification (FULL E2E via QUIC) ===");

    let accounts = genesis::generate_test_accounts();

    // ─── 1. Start a single node ───
    let tmp_dir = tempfile::tempdir()?;
    let genesis_path = genesis::write_genesis_file(tmp_dir.path(), &accounts);

    let config = NodeConfig {
        binary_path,
        genesis_path,
        validator_index: 0,
        validator_count: 1,
        block_interval_ms: 4000,
        port_offset: 0,
        trusted_peers: vec![],
        base_timeout_ms: None,
        max_timeout_ms: None,
        startup_delay_ms: None,
    };

    let node = NodeProcess::start(&config).await?;
    info!(
        http_port = node.http_port,
        starhub_port = node.starhub_port,
        "single node started"
    );

    let rpc = RpcClient::new(node.http_url());

    // ─── 2. Wait for initial blocks ───
    info!("waiting for chain to start producing blocks...");
    let mut retries = 0;
    loop {
        match rpc.block_number().await {
            Ok(height) if height >= 2 => {
                info!(height, "initial blocks produced");
                break;
            }
            _ => {
                retries += 1;
                if retries > 30 {
                    return Err(eyre::eyre!("timeout waiting for initial blocks"));
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }

    // ─── 3. Connect QUIC client to StarHub BEFORE sending transactions ───
    // This ensures we don't miss any broadcast packets.
    let mobile_key = BlsSecretKey::random()?;
    let mobile_pubkey = mobile_key.public_key();
    let pubkey_bytes = mobile_pubkey.to_bytes();

    info!(
        pubkey = hex::encode(pubkey_bytes),
        "mobile verifier identity created"
    );

    let quic_conn = quic_test_client::connect_to_starhub(node.starhub_port, &pubkey_bytes).await?;
    info!("QUIC handshake complete — connected to StarHub");

    // ─── 4. Deploy ERC-20 contract + send mixed transactions ───
    let mut tx_engine = TxEngine::new(&accounts, TEST_CHAIN_ID);
    tx_engine.sync_nonces(&rpc).await?;

    let gas_price = rpc.gas_price().await.unwrap_or(1_000_000_000);
    let max_fee = (gas_price * 2) as u128;
    let priority_fee = (gas_price / 10) as u128;

    // Deploy ERC-20 contract
    info!("deploying ERC-20 contract...");
    let erc20 = Erc20Manager::deploy(&mut tx_engine, &rpc, 0, max_fee, priority_fee).await?;
    info!(contract = %erc20.contract_address, "ERC-20 deployed");

    // Send ETH transfers
    info!("sending ETH transfers...");
    let wallet_count = tx_engine.wallet_count();
    for i in 0..3u64 {
        let from_idx = (i as usize) % wallet_count;
        let to_idx = ((i as usize) + 1) % wallet_count;
        let to_addr = tx_engine.address(to_idx);
        let amount = U256::from(10_000_000_000_000_000u128); // 0.01 ETH
        let (_, raw_tx) =
            tx_engine.build_transfer(from_idx, to_addr, amount, max_fee, priority_fee, 21000)?;
        rpc.send_raw_transaction(&raw_tx).await?;
    }

    // Send ERC-20 transfers
    info!("sending ERC-20 transfers...");
    for i in 0..3u64 {
        let from_idx = 0; // deployer holds all tokens
        let to_idx = ((i as usize) + 1) % wallet_count;
        let to_addr = tx_engine.address(to_idx);
        let amount = U256::from(1_000_000u128); // 1 USDT
        let (_, raw_tx) = erc20.build_transfer_tx(
            &mut tx_engine,
            from_idx,
            to_addr,
            amount,
            max_fee,
            priority_fee,
        )?;
        rpc.send_raw_transaction(&raw_tx).await?;
    }

    info!("all transactions sent, waiting for inclusion in blocks...");

    // ─── 5. Receive VerificationPackets via QUIC and verify ───
    let chain_spec = n42_chainspec::n42_dev_chainspec();
    let mut code_cache = CodeCache::new(500);
    let mut verified_count = 0u32;
    let mut verified_with_txs = false;
    let mut receipts_sent = 0u32;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);

    info!("waiting for VerificationPackets via QUIC...");

    while tokio::time::Instant::now() < deadline {
        // Try to receive a packet with 8s timeout (slightly longer than block time)
        match receive_packet(&quic_conn, Duration::from_secs(8)).await {
            Ok(packet_data) => {
                if let Ok(packet) = decode_stream_packet(&packet_data) {
                    let (block_number, _) = match packet.header_info() {
                        Some(info) => info,
                        None => {
                            warn!("failed to decode stream packet header, skipping");
                            continue;
                        }
                    };

                    info!(
                        block_number,
                        block_hash = %packet.block_hash,
                        tx_count = packet.transactions.len(),
                        bytecodes = packet.bytecodes.len(),
                        read_log_bytes = packet.read_log_data.len(),
                        "received StreamPacket"
                    );

                    let verify_start = std::time::Instant::now();
                    let result = verify_block_stream(&packet, &mut code_cache, chain_spec.clone());
                    let verify_ms = verify_start.elapsed().as_millis();

                    match result {
                        Ok(vr) => {
                            info!(
                                block_number,
                                computed = %vr.computed_receipts_root,
                                verify_ms,
                                "verify_block_stream() completed"
                            );

                            update_cache_after_stream_verify(&packet, &mut code_cache);

                            let timestamp_ms = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_millis() as u64;

                            let receipt = sign_receipt(
                                packet.block_hash,
                                block_number,
                                vr.computed_receipts_root,
                                timestamp_ms,
                                &mobile_key,
                            );

                            assert!(
                                receipt.verify_signature().is_ok(),
                                "BLS signature should verify"
                            );

                            send_receipt(&quic_conn, &receipt).await?;
                            receipts_sent += 1;

                            info!(block_number, receipts_sent, "signed receipt sent back to node");

                            verified_count += 1;
                            if !packet.transactions.is_empty() {
                                verified_with_txs = true;
                            }
                        }
                        Err(e) => {
                            warn!(
                                block_number,
                                error = %e,
                                "verify_block_stream() failed"
                            );
                        }
                    }
                } else {
                    // Decode the legacy VerificationPacket when V2 decode fails.
                    let packet = match decode_packet(&packet_data) {
                        Ok(p) => p,
                        Err(e) => {
                            warn!(error = %e, "failed to decode packet, skipping");
                            continue;
                        }
                    };

                    info!(
                        block_number = packet.block_number,
                        block_hash = %packet.block_hash,
                        tx_count = packet.transactions.len(),
                        witness_accounts = packet.witness_accounts.len(),
                        uncached_bytecodes = packet.uncached_bytecodes.len(),
                        "received VerificationPacket"
                    );

                    let verify_start = std::time::Instant::now();
                    let result = verify_block(&packet, &mut code_cache, chain_spec.clone());
                    let verify_ms = verify_start.elapsed().as_millis();

                    match result {
                        Ok(vr) => {
                            info!(
                                block_number = packet.block_number,
                                computed = %vr.computed_receipts_root,
                                verify_ms,
                                "verify_block() completed"
                            );

                            update_cache_after_verify(&packet, &mut code_cache);

                            let timestamp_ms = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_millis() as u64;

                            let receipt = sign_receipt(
                                packet.block_hash,
                                packet.block_number,
                                vr.computed_receipts_root,
                                timestamp_ms,
                                &mobile_key,
                            );

                            assert!(
                                receipt.verify_signature().is_ok(),
                                "BLS signature should verify"
                            );

                            send_receipt(&quic_conn, &receipt).await?;
                            receipts_sent += 1;

                            info!(
                                block_number = packet.block_number,
                                receipts_sent,
                                "signed receipt sent back to node"
                            );

                            verified_count += 1;
                            if !packet.transactions.is_empty() {
                                verified_with_txs = true;
                            }
                        }
                        Err(e) => {
                            warn!(
                                block_number = packet.block_number,
                                error = %e,
                                "verify_block() failed"
                            );
                        }
                    }
                }

                // Stop after verifying at least one block with transactions.
                if verified_with_txs && verified_count >= 2 {
                    info!(
                        "verified {} blocks (including blocks with txs), stopping",
                        verified_count
                    );
                    break;
                }
            }
            Err(e) => {
                // Timeout or stream error — check if we've verified enough
                if verified_count > 0 {
                    info!(verified_count, "no more packets, proceeding with results");
                    break;
                }
                warn!(error = %e, "waiting for packet...");
            }
        }
    }

    // ─── 9. Validate results ───
    assert!(
        verified_count > 0,
        "should have verified at least one block via QUIC pipeline"
    );

    info!(
        verified_count,
        verified_with_txs,
        receipts_sent,
        code_cache_size = code_cache.len(),
        "mobile verification pipeline results"
    );

    // ─── 10. Check node health ───
    let final_height = rpc.block_number().await?;
    info!(final_height, "node still producing blocks");

    match rpc.consensus_status().await {
        Ok(status) => {
            info!(
                has_qc = status.has_committed_qc,
                validators = status.validator_count,
                "consensus status OK"
            );
        }
        Err(e) => {
            info!(error = %e, "consensus status query failed (non-fatal)");
        }
    }

    // Close QUIC connection gracefully
    quic_conn.close(0u32.into(), b"test complete");

    // ─── 10. Verify node received receipts ───
    // Wait briefly for the node to process the receipts we sent back.
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Read the node log and verify receipt processing messages.
    let log_path = node.stdout_log_path().to_path_buf();
    let log_contents = std::fs::read_to_string(&log_path).unwrap_or_default();
    let receipt_log_count = log_contents
        .lines()
        .filter(|line| line.contains("verification receipt received from mobile verifier"))
        .count();

    info!(
        receipt_log_count,
        receipts_sent, "node-side receipt processing verification"
    );

    assert!(
        receipt_log_count > 0,
        "node should have logged at least one receipt reception, but found 0 in node log"
    );
    assert_eq!(
        receipt_log_count as u32, receipts_sent,
        "node should have received all {} receipts we sent, but log shows {}",
        receipts_sent, receipt_log_count
    );

    info!("=== Scenario 8 PASSED ===");
    info!("  Blocks verified via QUIC: {}", verified_count);
    info!("  Blocks with transactions verified: {}", verified_with_txs);
    info!("  BLS-signed receipts sent: {}", receipts_sent);
    info!("  Code cache entries: {}", code_cache.len());

    let _ = node.stop();
    Ok(())
}

/// Receives a single VerificationPacket from the QUIC connection.
///
/// Reads a uni stream from the server. The first byte is the message type
/// (0x01 = packet, 0x02 = cache sync). Returns the packet data (without prefix).
async fn receive_packet(conn: &quinn::Connection, timeout: Duration) -> eyre::Result<Vec<u8>> {
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(eyre::eyre!("timeout waiting for uni stream"));
        }

        let mut recv = tokio::time::timeout(remaining, conn.accept_uni())
            .await
            .map_err(|_| eyre::eyre!("timeout waiting for uni stream"))??;

        // Read all data (type prefix + payload)
        let data = recv.read_to_end(10 * 1024 * 1024).await?; // 10MB max

        if data.is_empty() {
            return Err(eyre::eyre!("received empty stream"));
        }

        let msg_type = data[0];
        let payload = &data[1..];

        match msg_type {
            0x01 => return Ok(payload.to_vec()),
            0x03 => {
                return zstd::bulk::decompress(payload, 16 * 1024 * 1024)
                    .map_err(|e| eyre::eyre!("zstd decompress failed: {e}"));
            }
            0x02 | 0x04 => {
                // CacheSyncMessage — skip and keep waiting for the next packet.
                continue;
            }
            other => return Err(eyre::eyre!("unknown message type: 0x{:02x}", other)),
        }
    }
}

/// Sends a signed VerificationReceipt back to the node via QUIC.
async fn send_receipt(conn: &quinn::Connection, receipt: &VerificationReceipt) -> eyre::Result<()> {
    let data = bincode::serialize(receipt)
        .map_err(|e| eyre::eyre!("receipt serialization failed: {e}"))?;

    let mut send = conn.open_uni().await?;
    send.write_all(&data).await?;
    send.finish()?;

    Ok(())
}
