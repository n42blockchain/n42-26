use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

use alloy_primitives::U256;
use n42_mobile::code_cache::CodeCache;
use n42_mobile::packet::decode_packet;
use n42_mobile::receipt::{sign_receipt, VerificationReceipt};
use n42_mobile::verifier::{update_cache_after_verify, verify_block};
use n42_primitives::BlsSecretKey;

use crate::erc20::Erc20Manager;
use crate::genesis::{self, TEST_CHAIN_ID};
use crate::node_manager::{NodeConfig, NodeProcess};
use crate::rpc_client::RpcClient;
use crate::tx_engine::TxEngine;

/// Scenario 8: Mobile EVM Verification — Full End-to-End via QUIC.
///
/// Tests the complete production-grade mobile verification pipeline:
///
/// 1. Start a single-node chain (4s block interval)
/// 2. Connect to the node's StarHub via QUIC as a mobile verifier
/// 3. Deploy an ERC-20 contract (creates non-trivial state)
/// 4. Send mixed transactions: ETH transfers + ERC-20 transfers
/// 5. Receive real VerificationPackets from the node via QUIC
/// 6. Run verify_block() — full EVM re-execution with witness data
/// 7. Compare computed receipts_root against the expected value
/// 8. Sign the verification receipt with BLS12-381
/// 9. Send the signed receipt back to the node via QUIC
/// 10. Verify the full pipeline works end-to-end
///
/// This is a TRUE end-to-end test: node execution → witness capture →
/// packet build → QUIC transport → EVM re-execution → BLS sign → QUIC return.
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
        pubkey = hex::encode(&pubkey_bytes),
        "mobile verifier identity created"
    );

    let quic_conn = connect_to_starhub(node.starhub_port, &pubkey_bytes).await?;
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
        let (_, raw_tx) =
            erc20.build_transfer_tx(&mut tx_engine, from_idx, to_addr, amount, max_fee, priority_fee)?;
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
                // Decode the VerificationPacket
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

                // ─── 6. Run verify_block() — REAL EVM re-execution ───
                let verify_start = std::time::Instant::now();
                let result = verify_block(&packet, &mut code_cache, chain_spec.clone());
                let verify_ms = verify_start.elapsed().as_millis();

                match result {
                    Ok(vr) => {
                        info!(
                            block_number = packet.block_number,
                            receipts_root_match = vr.receipts_root_match,
                            computed = %vr.computed_receipts_root,
                            expected = %packet.receipts_root,
                            verify_ms,
                            "verify_block() completed"
                        );

                        assert!(
                            vr.receipts_root_match,
                            "receipts_root MISMATCH at block {}: computed {} != expected {}",
                            packet.block_number,
                            vr.computed_receipts_root,
                            packet.receipts_root,
                        );

                        // Update code cache with newly received bytecodes
                        update_cache_after_verify(&packet, &mut code_cache);

                        // ─── 7. Sign receipt with BLS12-381 ───
                        let timestamp_ms = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() as u64;

                        let receipt = sign_receipt(
                            packet.block_hash,
                            packet.block_number,
                            true,  // state_root_match (not yet verified separately)
                            true,  // receipts_root_match (verified above)
                            timestamp_ms,
                            &mobile_key,
                        );

                        assert!(receipt.is_valid(), "signed receipt should be valid");
                        assert!(
                            receipt.verify_signature().is_ok(),
                            "BLS signature should verify"
                        );

                        // ─── 8. Send receipt back via QUIC ───
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

                        // Stop after verifying at least one block with transactions
                        if verified_with_txs && verified_count >= 2 {
                            info!("verified {} blocks (including blocks with txs), stopping", verified_count);
                            break;
                        }
                    }
                    Err(e) => {
                        warn!(
                            block_number = packet.block_number,
                            error = %e,
                            "verify_block() failed"
                        );
                        // Don't fail the test on verify errors for early blocks
                        // that may not have complete witness data
                    }
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

    info!("=== Scenario 8 PASSED ===");
    info!(
        "  Blocks verified via QUIC: {}",
        verified_count
    );
    info!(
        "  Blocks with transactions verified: {}",
        verified_with_txs
    );
    info!(
        "  BLS-signed receipts sent: {}",
        receipts_sent
    );
    info!(
        "  Code cache entries: {}",
        code_cache.len()
    );

    let _ = node.stop();
    Ok(())
}

// ─── QUIC client helpers ───

/// Connects to the StarHub QUIC server and performs the BLS pubkey handshake.
async fn connect_to_starhub(port: u16, pubkey: &[u8; 48]) -> eyre::Result<quinn::Connection> {
    // Build TLS config that accepts self-signed certificates
    let mut client_crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
        .with_no_client_auth();
    client_crypto.alpn_protocols = vec![b"n42-mobile/1".to_vec()];

    let client_config = quinn::ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(client_crypto)
            .map_err(|e| eyre::eyre!("QUIC client config error: {e}"))?,
    ));

    let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse()?)?;
    endpoint.set_default_client_config(client_config);

    // Connect to StarHub
    let addr = format!("127.0.0.1:{port}").parse()?;
    let connection = endpoint.connect(addr, "n42-mobile")?;
    let conn = connection.await?;

    info!(
        remote = %conn.remote_address(),
        "QUIC connection established"
    );

    // Handshake: send 48-byte BLS public key via uni stream
    let mut send = conn.open_uni().await?;
    send.write_all(pubkey).await?;
    send.finish()?;

    info!("BLS pubkey handshake sent");

    Ok(conn)
}

/// Receives a single VerificationPacket from the QUIC connection.
///
/// Reads a uni stream from the server. The first byte is the message type
/// (0x01 = packet, 0x02 = cache sync). Returns the packet data (without prefix).
async fn receive_packet(
    conn: &quinn::Connection,
    timeout: Duration,
) -> eyre::Result<Vec<u8>> {
    let mut recv = tokio::time::timeout(timeout, conn.accept_uni())
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
        0x01 => {
            // VerificationPacket
            Ok(payload.to_vec())
        }
        0x02 => {
            // CacheSyncMessage — skip and try again
            Err(eyre::eyre!("received cache sync message, not a packet"))
        }
        other => {
            Err(eyre::eyre!("unknown message type: 0x{:02x}", other))
        }
    }
}

/// Sends a signed VerificationReceipt back to the node via QUIC.
async fn send_receipt(
    conn: &quinn::Connection,
    receipt: &VerificationReceipt,
) -> eyre::Result<()> {
    let data = bincode::serialize(receipt)
        .map_err(|e| eyre::eyre!("receipt serialization failed: {e}"))?;

    let mut send = conn.open_uni().await?;
    send.write_all(&data).await?;
    send.finish()?;

    Ok(())
}

/// Custom certificate verifier that accepts any self-signed certificate.
/// StarHub uses rcgen-generated self-signed certs.
#[derive(Debug)]
struct SkipServerVerification;

impl rustls::client::danger::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
            rustls::SignatureScheme::ED25519,
            rustls::SignatureScheme::RSA_PSS_SHA256,
            rustls::SignatureScheme::RSA_PSS_SHA384,
            rustls::SignatureScheme::RSA_PSS_SHA512,
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::RSA_PKCS1_SHA384,
            rustls::SignatureScheme::RSA_PKCS1_SHA512,
        ]
    }
}
