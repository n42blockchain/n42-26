use std::net::SocketAddr;
use std::time::Duration;

use clap::Parser;
use n42_mobile::code_cache::{CodeCache, decode_cache_sync};
use n42_mobile::packet::decode_stream_packet;
use n42_mobile::quic_client::{QuicMobileClient, ReceivedMessage};
use n42_mobile::receipt::sign_receipt;
use n42_mobile::verifier::{update_cache_after_stream_verify, verify_block_stream};
use n42_primitives::BlsSecretKey;
use tracing::{error, info, warn};

#[derive(Parser, Debug)]
#[command(
    name = "n42-mobile-sim",
    about = "N42 mobile phone verification simulator"
)]
struct Args {
    /// Comma-separated StarHub ports (e.g. 9500,9501,9502)
    #[arg(long, value_delimiter = ',')]
    starhub_ports: Vec<u16>,

    /// Number of simulated phones
    #[arg(long, default_value_t = 1)]
    phone_count: usize,

    /// Duration in seconds (0 = infinite)
    #[arg(long, default_value_t = 0)]
    duration: u64,
}

fn deterministic_bls_key(index: usize) -> BlsSecretKey {
    let seed = alloy_primitives::keccak256(format!("n42-mobile-key-{index}").as_bytes());
    BlsSecretKey::key_gen(&seed.0).expect("BLS key generation from deterministic seed")
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();

    if args.starhub_ports.is_empty() {
        return Err(eyre::eyre!("--starhub-ports is required"));
    }

    info!(
        ports = ?args.starhub_ports,
        phones = args.phone_count,
        "starting mobile verification simulator"
    );

    let mut handles = Vec::new();

    for phone_idx in 0..args.phone_count {
        let ports = args.starhub_ports.clone();
        let duration = args.duration;

        handles.push(tokio::spawn(async move {
            if let Err(e) = run_phone(phone_idx, &ports, duration).await {
                error!(phone = phone_idx, error = %e, "phone task exited with error");
            }
        }));
    }

    for handle in handles {
        let _ = handle.await;
    }

    Ok(())
}

async fn run_phone(phone_idx: usize, ports: &[u16], duration_secs: u64) -> eyre::Result<()> {
    let bls_key = deterministic_bls_key(phone_idx);
    let pubkey = bls_key.public_key().to_bytes();
    let eth_addr = n42_mobile::bls_pubkey_to_address(&pubkey);

    info!(
        phone = phone_idx,
        pubkey = hex::encode(pubkey),
        %eth_addr,
        "phone identity created"
    );

    let target_port = ports[phone_idx % ports.len()];
    let addr: SocketAddr = format!("127.0.0.1:{target_port}").parse()?;

    let chain_spec = n42_chainspec::n42_dev_chainspec();
    let mut code_cache = CodeCache::new(500);
    let mut verified_count = 0u64;

    let deadline = if duration_secs > 0 {
        Some(tokio::time::Instant::now() + Duration::from_secs(duration_secs))
    } else {
        None
    };

    let deadline_reached = || deadline.is_some_and(|dl| tokio::time::Instant::now() >= dl);

    loop {
        if deadline_reached() {
            info!(
                phone = phone_idx,
                verified_count, "duration reached, stopping"
            );
            return Ok(());
        }

        info!(phone = phone_idx, %addr, "connecting to StarHub");

        let client = match QuicMobileClient::connect(addr, bls_key.clone()).await {
            Ok(c) => c,
            Err(e) => {
                warn!(phone = phone_idx, error = %e, "connection failed, retrying in 5s");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };
        info!(phone = phone_idx, "QUIC handshake complete");

        loop {
            if deadline_reached() {
                client.close();
                info!(
                    phone = phone_idx,
                    verified_count, "duration reached, stopping"
                );
                return Ok(());
            }

            if client.is_closed() {
                warn!(phone = phone_idx, "connection closed, will reconnect");
                break;
            }

            match client.receive_message(Duration::from_secs(8)).await {
                Ok(ReceivedMessage::CacheSync(sync_data)) => match decode_cache_sync(&sync_data) {
                    Ok(sync_msg) => {
                        let added = sync_msg.codes.len();
                        let evicted = sync_msg.evict_hints.len();
                        for (hash, code) in &sync_msg.codes {
                            code_cache.insert(*hash, code.clone());
                        }
                        for hash in &sync_msg.evict_hints {
                            code_cache.remove(hash);
                        }
                        info!(
                            phone = phone_idx,
                            added,
                            evicted,
                            cache_size = code_cache.len(),
                            "cache sync applied"
                        );
                    }
                    Err(e) => {
                        warn!(phone = phone_idx, error = %e, "failed to decode cache sync");
                    }
                },
                Ok(ReceivedMessage::Packet(packet_data)) => {
                    let packet = match decode_stream_packet(&packet_data) {
                        Ok(p) => p,
                        Err(e) => {
                            warn!(phone = phone_idx, error = %e, "failed to decode packet, skipping");
                            continue;
                        }
                    };

                    let (block_number, receipts_root) = match packet.header_info() {
                        Some(info) => info,
                        None => {
                            warn!(phone = phone_idx, "failed to extract header info, skipping");
                            continue;
                        }
                    };
                    let tx_count = packet.transactions.len();

                    let verify_start = std::time::Instant::now();
                    let result = verify_block_stream(&packet, &mut code_cache, chain_spec.clone());
                    let verify_ms = verify_start.elapsed().as_millis();

                    match result {
                        Ok(vr) => {
                            if vr.computed_receipts_root != receipts_root {
                                error!(
                                    phone = phone_idx,
                                    block_number,
                                    computed = %vr.computed_receipts_root,
                                    expected = %receipts_root,
                                    "receipts_root MISMATCH"
                                );
                            }

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
                                client.bls_key(),
                            );

                            if let Err(e) = client.send_receipt(&receipt).await {
                                warn!(phone = phone_idx, block_number, error = %e, "failed to send receipt, reconnecting");
                                break;
                            }

                            verified_count += 1;
                            let receipts_match = vr.computed_receipts_root == receipts_root;
                            info!(
                                phone = phone_idx,
                                block_number,
                                tx_count,
                                verify_ms,
                                receipts_match,
                                verified_count,
                                "block verified & attested"
                            );
                        }
                        Err(e) => {
                            warn!(
                                phone = phone_idx,
                                block_number,
                                error = %e,
                                "verify_block_stream() failed, skipping attestation"
                            );
                        }
                    }
                }
                Err(e) => {
                    let err_str = e.to_string();
                    if (err_str.contains("timed out") || err_str.contains("timeout"))
                        && !client.is_closed()
                    {
                        // Normal timeout waiting for next block — just retry
                        continue;
                    }
                    // Connection-level error — break to reconnect
                    warn!(phone = phone_idx, error = %e, "connection error, will reconnect");
                    break;
                }
            }
        }

        info!(phone = phone_idx, "reconnecting in 3s...");
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}
