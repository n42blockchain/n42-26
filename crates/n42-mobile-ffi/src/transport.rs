//! QUIC transport layer: connection, receive loop, and TLS certificate pinning.

use n42_mobile::code_cache::{CacheSyncMessage, CodeCache, decode_cache_sync};
use n42_mobile::wire::MAGIC as WIRE_MAGIC;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{debug, info, warn};

use crate::context::{lock_or_recover, MAX_PENDING_PACKETS};

/// Detects whether `data` uses the V2 wire format (magic bytes `N2` = `0x4E 0x32`).
///
/// Returns `true` for V2 StreamPacket / CacheSyncMessage, `false` for legacy bincode.
pub(crate) fn is_v2_wire_format(data: &[u8]) -> bool {
    data.len() >= 2 && data[0] == WIRE_MAGIC[0] && data[1] == WIRE_MAGIC[1]
}

/// Establishes a QUIC connection to the StarHub and performs the BLS pubkey handshake.
///
/// Returns the connection and an empty pending-packets queue on success.
pub(crate) async fn connect_quic(
    host: &str,
    port: u16,
    pubkey: &[u8; 48],
    expected_cert_hash: Option<[u8; 32]>,
) -> Result<
    (quinn::Connection, Arc<Mutex<VecDeque<Vec<u8>>>>),
    Box<dyn std::error::Error + Send + Sync>,
> {
    let crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedCertVerification { expected_hash: expected_cert_hash }))
        .with_no_client_auth();

    let mut transport = quinn::TransportConfig::default();
    transport.max_idle_timeout(Some(
        quinn::IdleTimeout::try_from(Duration::from_secs(300)).unwrap(),
    ));
    transport.keep_alive_interval(Some(Duration::from_secs(15)));

    let mut client_config = quinn::ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(crypto)?,
    ));
    client_config.transport_config(Arc::new(transport));

    let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse()?)?;
    endpoint.set_default_client_config(client_config);

    let addr = format!("{}:{}", host, port);
    let connection = tokio::time::timeout(
        Duration::from_secs(10),
        endpoint.connect(addr.parse()?, "n42-starhub")?,
    )
    .await
    .map_err(|_| -> Box<dyn std::error::Error + Send + Sync> {
        "QUIC connect timed out after 10s".into()
    })??;

    info!("QUIC connection established, sending BLS pubkey handshake");

    let mut handshake_stream = connection.open_uni().await?;
    handshake_stream.write_all(pubkey).await?;
    handshake_stream.finish()?;

    let pending = Arc::new(Mutex::new(VecDeque::with_capacity(MAX_PENDING_PACKETS)));
    Ok((connection, pending))
}

/// Applies a `CacheSyncMessage` to the code cache.
///
/// Returns `(added, evicted)` counts. Evict hints for missing keys are silently ignored.
pub(crate) fn apply_cache_sync(msg: CacheSyncMessage, cache: &Mutex<CodeCache>) -> (usize, usize) {
    let mut guard = lock_or_recover(cache);
    let added = msg.codes.len();
    for (hash, code) in msg.codes {
        guard.insert(hash, code);
    }
    let evicted = msg.evict_hints.len();
    for hash in &msg.evict_hints {
        guard.remove(hash);
    }
    (added, evicted)
}

/// Enqueues a verification packet into the pending queue, dropping the oldest if full.
fn enqueue_packet(
    payload: Vec<u8>,
    pending_packets: &Arc<Mutex<VecDeque<Vec<u8>>>>,
    dropped_count: &Arc<AtomicU64>,
) {
    let mut queue = lock_or_recover(pending_packets);
    if queue.len() >= MAX_PENDING_PACKETS {
        dropped_count.fetch_add(1, Ordering::Relaxed);
        warn!(
            queue_len = queue.len(),
            dropped = dropped_count.load(Ordering::Relaxed),
            "packet queue full, dropping oldest packet"
        );
        queue.pop_front();
    }
    queue.push_back(payload);
}

/// Processes a cache sync payload (after optional decompression).
fn handle_cache_sync(
    payload: &[u8],
    code_cache: &Arc<Mutex<CodeCache>>,
    label: &str,
) {
    let msg = if is_v2_wire_format(payload) {
        decode_cache_sync(payload).map_err(|e| format!("V2: {e}"))
    } else {
        bincode::deserialize::<CacheSyncMessage>(payload).map_err(|e| format!("V1: {e}"))
    };
    match msg {
        Ok(msg) => {
            let (added, evicted) = apply_cache_sync(msg, code_cache);
            let cache_size = lock_or_recover(code_cache).len();
            info!(added, evicted, cache_size, "applied {} cache sync message", label);
        }
        Err(e) => warn!("failed to decode CacheSyncMessage ({}): {}", label, e),
    }
}

/// Background task that receives packets from the StarHub via QUIC uni-streams.
///
/// StarHub sends data with a 1-byte type prefix:
/// - `0x01`: VerificationPacket (uncompressed)
/// - `0x02`: CacheSyncMessage (uncompressed)
/// - `0x03`: VerificationPacket (zstd compressed)
/// - `0x04`: CacheSyncMessage (zstd compressed)
///
/// After decompression, V1 (bincode) vs V2 (wire header `0x4E32`) is auto-detected.
pub(crate) async fn recv_loop(
    connection: quinn::Connection,
    pending_packets: Arc<Mutex<VecDeque<Vec<u8>>>>,
    code_cache: Arc<Mutex<CodeCache>>,
    dropped_count: Arc<AtomicU64>,
) {
    const MAX_STREAM_SIZE: usize = 16 * 1024 * 1024; // 16MB

    loop {
        match connection.accept_uni().await {
            Ok(mut stream) => {
                match stream.read_to_end(MAX_STREAM_SIZE).await {
                    Ok(data) if !data.is_empty() => {
                        let msg_type = data[0];
                        let payload = &data[1..];

                        match msg_type {
                            0x01 => {
                                debug!(size = payload.len(), "received verification packet");
                                enqueue_packet(payload.to_vec(), &pending_packets, &dropped_count);
                            }
                            0x03 => {
                                match zstd::bulk::decompress(payload, MAX_STREAM_SIZE) {
                                    Ok(decompressed) => {
                                        debug!(
                                            compressed = payload.len(),
                                            decompressed = decompressed.len(),
                                            "received verification packet (zstd)"
                                        );
                                        enqueue_packet(decompressed, &pending_packets, &dropped_count);
                                    }
                                    Err(e) => warn!("zstd decompress failed: {}", e),
                                }
                            }
                            0x02 => {
                                const MAX_CACHE_SYNC_SIZE: usize = 16 * 1024 * 1024;
                                if payload.len() > MAX_CACHE_SYNC_SIZE {
                                    warn!("CacheSyncMessage too large ({} bytes), dropping", payload.len());
                                } else {
                                    handle_cache_sync(payload, &code_cache, "uncompressed");
                                }
                            }
                            0x04 => {
                                match zstd::bulk::decompress(payload, MAX_STREAM_SIZE) {
                                    Ok(decompressed) => {
                                        handle_cache_sync(&decompressed, &code_cache, "compressed");
                                    }
                                    Err(e) => warn!("zstd decompress CacheSync failed: {}", e),
                                }
                            }
                            _ => warn!(msg_type, "unknown message type from StarHub"),
                        }
                    }
                    Ok(_) => {} // empty stream, skip
                    Err(e) => debug!("stream read error: {}", e),
                }
            }
            Err(e) => {
                warn!("connection closed: {}", e);
                break;
            }
        }
    }
}

/// Certificate verifier that validates the server certificate against a pinned SHA-256 hash.
///
/// When `expected_hash` is `None` (dev mode), all certificates are accepted.
/// In production, use the SHA-256 of the StarHub's DER-encoded certificate,
/// obtained via an out-of-band channel or initial trust-on-first-use (TOFU).
#[derive(Debug)]
pub(crate) struct PinnedCertVerification {
    pub expected_hash: Option<[u8; 32]>,
}

impl rustls::client::danger::ServerCertVerifier for PinnedCertVerification {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if let Some(expected) = &self.expected_hash {
            use sha2::{Digest, Sha256};
            let actual: [u8; 32] = Sha256::digest(end_entity.as_ref()).into();
            if actual != *expected {
                return Err(rustls::Error::General(format!(
                    "certificate hash mismatch: expected {}, got {}",
                    hex::encode(expected),
                    hex::encode(actual),
                )));
            }
        }
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
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::RSA_PKCS1_SHA384,
            rustls::SignatureScheme::RSA_PKCS1_SHA512,
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
            rustls::SignatureScheme::ED25519,
            rustls::SignatureScheme::RSA_PSS_SHA256,
            rustls::SignatureScheme::RSA_PSS_SHA384,
            rustls::SignatureScheme::RSA_PSS_SHA512,
        ]
    }
}
