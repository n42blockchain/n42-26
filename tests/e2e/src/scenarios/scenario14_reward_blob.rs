use std::path::{Path, PathBuf};
use std::time::Duration;

use alloy_primitives::{Address, B256, U256};
use n42_mobile::code_cache::CodeCache;
use n42_mobile::packet::{decode_packet, decode_stream_packet};
use n42_mobile::receipt::{VerificationReceipt, sign_receipt};
use n42_mobile::verifier::{
    update_cache_after_stream_verify, update_cache_after_verify, verify_block, verify_block_stream,
};
use n42_primitives::BlsSecretKey;
use serde_json::Value;
use tracing::{debug, info, warn};

use crate::genesis::{self, TEST_CHAIN_ID};
use crate::node_manager::{NodeConfig, NodeProcess};
use crate::quic_test_client;
use crate::rpc_client::RpcClient;
use crate::test_helpers::{compute_peer_id, wait_for_sync};
use crate::tx_engine::TxEngine;

/// Scenario 14: Reward-in-block and blob-injection proof harness.
///
/// This is an explicit Caplin validation scenario for the two ports that the
/// 1/3/4 correctness suite does not exercise:
/// - WithdrawalSource: real mobile QUIC receipts produce EIP-4895 withdrawals.
/// - BlobStorePort: a real EIP-4844 tx sidecar is stored, broadcast, and imported.
pub async fn run(binary_path: PathBuf) -> eyre::Result<()> {
    info!("=== Scenario 14: Reward-in-block + Blob injection E2E ===");

    let reward = run_reward_in_block(&binary_path).await?;
    info!(
        block = reward.block_number,
        recipient = %reward.recipient,
        amount_gwei = reward.amount_gwei,
        balance = %reward.balance,
        receipts_sent = reward.receipts_sent,
        "reward-in-block proof complete"
    );

    let blob = run_blob_injection(&binary_path).await?;
    info!(
        tx_hash = %blob.tx_hash,
        block = blob.block_number,
        blob_gas_used = blob.blob_gas_used,
        broadcast_logs = blob.broadcast_logs,
        processed_logs = blob.processed_logs,
        "blob injection proof complete"
    );

    info!("=== Scenario 14 PASSED ===");
    Ok(())
}

struct RewardEvidence {
    block_number: u64,
    recipient: Address,
    amount_gwei: u64,
    balance: U256,
    receipts_sent: u32,
}

struct BlobEvidence {
    tx_hash: B256,
    block_number: u64,
    blob_gas_used: u64,
    broadcast_logs: usize,
    processed_logs: usize,
}

async fn run_reward_in_block(binary_path: &Path) -> eyre::Result<RewardEvidence> {
    info!("scenario14/reward: starting single-node reward-in-block proof");

    let accounts = genesis::generate_test_accounts();
    let tmp_dir = tempfile::tempdir()?;
    let genesis_path = genesis::write_genesis_file(tmp_dir.path(), &accounts);

    let config = NodeConfig {
        binary_path: binary_path.to_path_buf(),
        genesis_path,
        validator_index: 0,
        validator_count: 1,
        block_interval_ms: 1000,
        port_offset: 1700,
        trusted_peers: vec![],
        base_timeout_ms: Some(8000),
        max_timeout_ms: Some(16000),
        startup_delay_ms: None,
    };

    let node = NodeProcess::start_with_env(
        &config,
        vec![
            (
                "RUST_LOG",
                "info,n42::mobile=debug,n42::reward=debug,n42::cl::exec_bridge=debug",
            ),
            ("N42_OPEN_VERIFICATION", "1"),
            ("N42_MIN_ATTESTATION_THRESHOLD", "1"),
            ("N42_REWARD_EPOCH_BLOCKS", "1"),
            ("N42_DAILY_BASE_REWARD_GWEI", "100000000"),
            ("N42_REWARD_CURVE_K", "4.0"),
            ("N42_MAX_REWARDS_PER_BLOCK", "8"),
            ("N42_UNSTAKED_REWARD_RATIO", "1.0"),
        ],
    )
    .await?;
    let rpc = RpcClient::new(node.http_url());

    wait_for_height(&rpc, 2, Duration::from_secs(20)).await?;

    let mobile_key = BlsSecretKey::random()?;
    let mobile_pubkey = mobile_key.public_key();
    let pubkey_bytes = mobile_pubkey.to_bytes();
    let reward_address = n42_mobile::bls_pubkey_to_address(&pubkey_bytes);

    info!(
        starhub_port = node.starhub_port,
        reward_address = %reward_address,
        pubkey = hex::encode(pubkey_bytes),
        "scenario14/reward: connecting real QUIC verifier"
    );
    let quic_conn = quic_test_client::connect_to_starhub(node.starhub_port, &pubkey_bytes).await?;

    let receipts_sent =
        sign_mobile_receipts(&quic_conn, &mobile_key, 8, Duration::from_secs(75)).await?;
    quic_conn.close(0u32.into(), b"scenario14 reward complete");

    let evidence =
        wait_for_reward_withdrawal(&rpc, reward_address, receipts_sent, Duration::from_secs(45))
            .await?;

    let log = node_logs(&node);
    if !log.contains("verification receipt received from mobile verifier") {
        return Err(eyre::eyre!(
            "reward proof missing node-side mobile receipt log"
        ));
    }
    if !log.contains("injecting mobile rewards as withdrawals") {
        return Err(eyre::eyre!(
            "reward proof missing WithdrawalSource injection log"
        ));
    }

    let _ = node.stop();
    Ok(evidence)
}

async fn run_blob_injection(binary_path: &Path) -> eyre::Result<BlobEvidence> {
    info!("scenario14/blob: starting 3-node blob injection proof");

    let accounts = genesis::generate_test_accounts();
    let tmp_dir = tempfile::tempdir()?;
    let genesis_path = genesis::write_genesis_file(tmp_dir.path(), &accounts);
    let mut nodes = start_blob_nodes(binary_path, genesis_path).await?;

    wait_for_height(&nodes[0].rpc, 2, Duration::from_secs(30)).await?;

    let mut tx_engine = TxEngine::new(&accounts, TEST_CHAIN_ID);
    tx_engine.sync_nonces(&nodes[0].rpc).await?;

    let gas_price = nodes[0].rpc.gas_price().await.unwrap_or(20_000_000_000);
    let max_fee = gas_price.saturating_mul(4).max(20_000_000_000);
    let priority_fee = (gas_price / 10).max(1_000_000_000);
    let max_fee_per_blob_gas = 15_000_000_000u128;
    let recipient = tx_engine.address(1);

    let (tx_hash, raw_tx) = tx_engine
        .build_blob_tx(
            0,
            recipient,
            U256::from(123u64),
            max_fee,
            priority_fee,
            max_fee_per_blob_gas,
        )
        .await?;
    let submitted = nodes[0].rpc.send_raw_transaction(&raw_tx).await?;
    if submitted != tx_hash {
        return Err(eyre::eyre!(
            "blob tx hash mismatch: built={tx_hash:?} submitted={submitted:?}"
        ));
    }
    info!(%tx_hash, "scenario14/blob: submitted EIP-4844 transaction");

    let receipt = nodes[0]
        .rpc
        .wait_for_receipt(tx_hash, Duration::from_secs(120))
        .await?;
    if receipt.status != 1 {
        return Err(eyre::eyre!(
            "blob tx failed: hash={tx_hash:?} block={} status={}",
            receipt.block_number,
            receipt.status
        ));
    }

    for node in &nodes {
        wait_for_sync(&node.rpc, receipt.block_number, Duration::from_secs(60)).await?;
    }

    let block0 = nodes[0]
        .rpc
        .get_block_by_number(receipt.block_number)
        .await?;
    let expected_hash = json_str(&block0, "hash")?;
    let blob_gas_used = parse_hex_field(&block0, "blobGasUsed")
        .ok_or_else(|| eyre::eyre!("blob block missing blobGasUsed"))?;
    if blob_gas_used == 0 {
        return Err(eyre::eyre!(
            "blob block {} has blobGasUsed=0",
            receipt.block_number
        ));
    }
    assert_block_contains_tx(&block0, tx_hash)?;

    for (idx, node) in nodes.iter().enumerate().skip(1) {
        let block = node.rpc.get_block_by_number(receipt.block_number).await?;
        let hash = json_str(&block, "hash")?;
        if hash != expected_hash {
            return Err(eyre::eyre!(
                "blob block hash mismatch on node {idx}: expected={expected_hash} got={hash}"
            ));
        }
        let follower_blob_gas = parse_hex_field(&block, "blobGasUsed").unwrap_or_default();
        if follower_blob_gas != blob_gas_used {
            return Err(eyre::eyre!(
                "blobGasUsed mismatch on node {idx}: expected={blob_gas_used} got={follower_blob_gas}"
            ));
        }
        assert_block_contains_tx(&block, tx_hash)?;
    }

    tokio::time::sleep(Duration::from_secs(2)).await;
    let logs = all_node_logs(&nodes);
    let broadcast_logs = logs.matches("broadcasting blob sidecars").count();
    let processed_logs = logs.matches("processed blob sidecar broadcast").count();
    let decode_errors = logs.matches("failed to decode blob sidecar RLP").count();
    let insert_errors = logs.matches("failed to insert blob sidecar").count();
    if decode_errors != 0 || insert_errors != 0 {
        return Err(eyre::eyre!(
            "blob sidecar adapter logged decode/insert errors: decode={decode_errors} insert={insert_errors}"
        ));
    }
    if broadcast_logs == 0 {
        return Err(eyre::eyre!("missing leader blob sidecar broadcast log"));
    }
    if processed_logs == 0 {
        return Err(eyre::eyre!("missing follower blob sidecar processed log"));
    }

    let evidence = BlobEvidence {
        tx_hash,
        block_number: receipt.block_number,
        blob_gas_used,
        broadcast_logs,
        processed_logs,
    };

    for node in nodes.drain(..) {
        let _ = node.stop();
    }

    Ok(evidence)
}

async fn start_blob_nodes(
    binary_path: &Path,
    genesis_path: PathBuf,
) -> eyre::Result<Vec<NodeProcess>> {
    let node_count = 3usize;
    let port_offset_base = 1800u16;
    let peer_ids: Vec<_> = (0..node_count).map(compute_peer_id).collect();
    let mut nodes = Vec::with_capacity(node_count);

    for i in 0..node_count {
        let port_offset = port_offset_base + (i as u16) * 10;
        let trusted_peers: Vec<String> = (0..node_count)
            .filter(|&j| j != i)
            .map(|j| {
                let peer_port = 9400 + port_offset_base + (j as u16) * 10;
                format!(
                    "/ip4/127.0.0.1/udp/{}/quic-v1/p2p/{}",
                    peer_port, peer_ids[j]
                )
            })
            .collect();

        let config = NodeConfig {
            binary_path: binary_path.to_path_buf(),
            genesis_path: genesis_path.clone(),
            validator_index: i,
            validator_count: node_count,
            block_interval_ms: 2000,
            port_offset,
            trusted_peers,
            base_timeout_ms: Some(12000),
            max_timeout_ms: Some(30000),
            startup_delay_ms: Some(2000),
        };

        match NodeProcess::start_with_env(
            &config,
            vec![(
                "RUST_LOG",
                "info,n42::cl::exec_bridge=debug,n42::observer=debug",
            )],
        )
        .await
        {
            Ok(node) => {
                info!(
                    node = i,
                    http_port = node.http_port,
                    "scenario14/blob node started"
                );
                nodes.push(node);
            }
            Err(error) => {
                for node in nodes {
                    let _ = node.stop();
                }
                return Err(eyre::eyre!("failed to start blob node {i}: {error}"));
            }
        }
    }

    Ok(nodes)
}

async fn sign_mobile_receipts(
    conn: &quinn::Connection,
    mobile_key: &BlsSecretKey,
    min_receipts: u32,
    timeout: Duration,
) -> eyre::Result<u32> {
    let chain_spec = n42_chainspec::n42_dev_chainspec();
    let mut code_cache = CodeCache::new(500);
    let mut receipts_sent = 0u32;
    let deadline = tokio::time::Instant::now() + timeout;

    while tokio::time::Instant::now() < deadline && receipts_sent < min_receipts {
        match receive_packet(conn, Duration::from_secs(8)).await {
            Ok(packet_data) => {
                if let Ok(packet) = decode_stream_packet(&packet_data) {
                    let (block_number, _) = match packet.header_info() {
                        Some(header) => header,
                        None => {
                            warn!("scenario14/reward: stream packet missing header");
                            continue;
                        }
                    };
                    let result = verify_block_stream(&packet, &mut code_cache, chain_spec.clone());
                    match result {
                        Ok(vr) => {
                            update_cache_after_stream_verify(&packet, &mut code_cache);
                            let receipt = signed_receipt(
                                packet.block_hash,
                                block_number,
                                vr.computed_receipts_root,
                                mobile_key,
                            );
                            send_receipt(conn, &receipt).await?;
                            receipts_sent += 1;
                            info!(
                                block_number,
                                receipts_sent, "scenario14/reward: signed stream receipt sent"
                            );
                        }
                        Err(error) => {
                            warn!(block_number, error = %error, "stream verification failed");
                        }
                    }
                } else {
                    let packet = match decode_packet(&packet_data) {
                        Ok(packet) => packet,
                        Err(error) => {
                            warn!(error = %error, "failed to decode mobile packet");
                            continue;
                        }
                    };
                    let result = verify_block(&packet, &mut code_cache, chain_spec.clone());
                    match result {
                        Ok(vr) => {
                            update_cache_after_verify(&packet, &mut code_cache);
                            let receipt = signed_receipt(
                                packet.block_hash,
                                packet.block_number,
                                vr.computed_receipts_root,
                                mobile_key,
                            );
                            send_receipt(conn, &receipt).await?;
                            receipts_sent += 1;
                            info!(
                                block_number = packet.block_number,
                                receipts_sent, "scenario14/reward: signed legacy receipt sent"
                            );
                        }
                        Err(error) => {
                            warn!(
                                block_number = packet.block_number,
                                error = %error,
                                "legacy verification failed"
                            );
                        }
                    }
                }
            }
            Err(error) => {
                debug!(error = %error, "waiting for reward mobile packet");
            }
        }
    }

    if receipts_sent < min_receipts {
        return Err(eyre::eyre!(
            "sent only {receipts_sent}/{min_receipts} mobile receipts"
        ));
    }

    Ok(receipts_sent)
}

fn signed_receipt(
    block_hash: B256,
    block_number: u64,
    receipts_root: B256,
    mobile_key: &BlsSecretKey,
) -> VerificationReceipt {
    let timestamp_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    sign_receipt(
        block_hash,
        block_number,
        receipts_root,
        timestamp_ms,
        mobile_key,
    )
}

async fn wait_for_reward_withdrawal(
    rpc: &RpcClient,
    expected_address: Address,
    receipts_sent: u32,
    timeout: Duration,
) -> eyre::Result<RewardEvidence> {
    let start = tokio::time::Instant::now();
    let mut next_scan = 1u64;

    loop {
        let current = rpc.block_number().await.unwrap_or_default();
        while next_scan <= current {
            let block = rpc.get_block_by_number(next_scan).await?;
            if let Some(withdrawals) = block.get("withdrawals").and_then(Value::as_array) {
                for withdrawal in withdrawals {
                    let Some(address) = withdrawal.get("address").and_then(Value::as_str) else {
                        continue;
                    };
                    if !addr_eq(address, expected_address) {
                        continue;
                    }
                    let amount_gwei = withdrawal
                        .get("amount")
                        .and_then(parse_hex_value)
                        .unwrap_or_default();
                    if amount_gwei == 0 {
                        return Err(eyre::eyre!(
                            "reward withdrawal for {expected_address:?} had zero amount"
                        ));
                    }
                    let balance = rpc.get_balance(expected_address).await?;
                    if balance == U256::ZERO {
                        return Err(eyre::eyre!(
                            "reward withdrawal for {expected_address:?} found but balance is zero"
                        ));
                    }
                    return Ok(RewardEvidence {
                        block_number: next_scan,
                        recipient: expected_address,
                        amount_gwei,
                        balance,
                        receipts_sent,
                    });
                }
            }
            next_scan += 1;
        }

        if start.elapsed() > timeout {
            return Err(eyre::eyre!(
                "timeout waiting for reward withdrawal to {expected_address:?}; scanned through block {current}"
            ));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn wait_for_height(rpc: &RpcClient, target: u64, timeout: Duration) -> eyre::Result<()> {
    let start = tokio::time::Instant::now();
    loop {
        match rpc.block_number().await {
            Ok(height) if height >= target => return Ok(()),
            Ok(height) => debug!(height, target, "waiting for height"),
            Err(error) => debug!(error = %error, "height poll failed"),
        }
        if start.elapsed() > timeout {
            return Err(eyre::eyre!("timeout waiting for block height {target}"));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

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
        let data = recv.read_to_end(10 * 1024 * 1024).await?;
        if data.is_empty() {
            return Err(eyre::eyre!("received empty mobile stream"));
        }

        let msg_type = data[0];
        let payload = &data[1..];
        match msg_type {
            0x01 => return Ok(payload.to_vec()),
            0x03 => {
                return zstd::bulk::decompress(payload, 16 * 1024 * 1024)
                    .map_err(|e| eyre::eyre!("zstd decompress failed: {e}"));
            }
            0x02 | 0x04 => continue,
            other => return Err(eyre::eyre!("unknown mobile stream type: 0x{other:02x}")),
        }
    }
}

async fn send_receipt(conn: &quinn::Connection, receipt: &VerificationReceipt) -> eyre::Result<()> {
    if let Err(error) = receipt.verify_signature() {
        return Err(eyre::eyre!("receipt signature did not verify: {error}"));
    }
    let data = bincode::serialize(receipt)
        .map_err(|e| eyre::eyre!("receipt serialization failed: {e}"))?;
    let mut send = conn.open_uni().await?;
    send.write_all(&data).await?;
    send.finish()?;
    Ok(())
}

fn assert_block_contains_tx(block: &Value, tx_hash: B256) -> eyre::Result<()> {
    let expected = format!("{tx_hash:?}").to_lowercase();
    let txs = block
        .get("transactions")
        .and_then(Value::as_array)
        .ok_or_else(|| eyre::eyre!("block missing transactions array"))?;
    if txs
        .iter()
        .filter_map(Value::as_str)
        .any(|hash| hash.eq_ignore_ascii_case(&expected))
    {
        Ok(())
    } else {
        Err(eyre::eyre!("block does not contain tx {tx_hash:?}"))
    }
}

fn parse_hex_field(block: &Value, field: &str) -> Option<u64> {
    block.get(field).and_then(parse_hex_value)
}

fn parse_hex_value(value: &Value) -> Option<u64> {
    match value {
        Value::String(s) => u64::from_str_radix(s.trim_start_matches("0x"), 16).ok(),
        Value::Number(n) => n.as_u64(),
        _ => None,
    }
}

fn json_str<'a>(value: &'a Value, field: &str) -> eyre::Result<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| eyre::eyre!("block missing string field {field}"))
}

fn addr_eq(actual: &str, expected: Address) -> bool {
    actual.eq_ignore_ascii_case(&format!("{expected:?}"))
}

fn node_logs(node: &NodeProcess) -> String {
    let mut logs = String::new();
    if let Ok(stdout) = std::fs::read_to_string(node.stdout_log_path()) {
        logs.push_str(&stdout);
        logs.push('\n');
    }
    if let Ok(stderr) = std::fs::read_to_string(node.stderr_log_path()) {
        logs.push_str(&stderr);
        logs.push('\n');
    }
    logs
}

fn all_node_logs(nodes: &[NodeProcess]) -> String {
    let mut logs = String::new();
    for node in nodes {
        logs.push_str(&node_logs(node));
        logs.push('\n');
    }
    logs
}
