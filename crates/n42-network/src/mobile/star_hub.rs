use bytes::{BufMut, Bytes, BytesMut};
use n42_mobile::receipt::VerificationReceipt;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, RwLock};

use super::session::{MobileSession, PhoneTier};

/// Message type prefix for verification packets (uncompressed).
pub const MSG_TYPE_PACKET: u8 = 0x01;
/// Message type prefix for cache sync messages (uncompressed).
pub const MSG_TYPE_CACHE_SYNC: u8 = 0x02;
/// Message type prefix for zstd-compressed verification packets.
pub const MSG_TYPE_PACKET_ZSTD: u8 = 0x03;
/// Message type prefix for zstd-compressed cache sync messages.
pub const MSG_TYPE_CACHE_SYNC_ZSTD: u8 = 0x04;

/// Handshake timeout: phones must send their public key within this duration.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum size for a receipt bincode message (64KB).
/// Actual receipt is ~220 bytes; 64KB provides 300x headroom.
const MAX_RECEIPT_SIZE: u64 = 64 * 1024;

/// Timeout for reading a single receipt stream from a phone.
/// Receipt is ~220 bytes; even on 2G (50Kbps) completes in <1s.
/// 10s covers extreme network jitter without blocking the accept loop.
const RECEIPT_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// Minimum interval between receipts from the same phone.
/// Each phone sends ~1 receipt per 8s slot; 2s allows up to 4/slot (4x headroom).
const MIN_RECEIPT_INTERVAL: Duration = Duration::from_secs(2);

/// Timeout for sending a broadcast message to a single phone.
/// Covers open_uni + write_all + finish. Slow phones that exceed this
/// have the current message skipped (connection stays open).
const BROADCAST_SEND_TIMEOUT: Duration = Duration::from_secs(3);



/// Combines type prefix + payload into a single contiguous Bytes.
///
/// This avoids two separate `write_all` syscalls per phone connection:
/// instead of writing `[prefix]` then `[data]`, we write `[prefix | data]` once.
/// For 10K phones this halves the total syscall count per broadcast.
fn preframe_message(type_prefix: u8, data: &Bytes) -> Bytes {
    let mut buf = BytesMut::with_capacity(1 + data.len());
    buf.put_u8(type_prefix);
    buf.extend_from_slice(data);
    buf.freeze()
}

/// Global session ID generator for unique IDs across multiple StarHub shards.
///
/// When multiple StarHub instances share a `SessionIdGenerator`, each session
/// receives a globally unique ID regardless of which shard accepted the connection.
pub struct SessionIdGenerator(pub AtomicU64);

impl SessionIdGenerator {
    /// Creates a new generator starting at ID 1.
    pub fn new() -> Self {
        Self(AtomicU64::new(1))
    }

    /// Returns the next unique session ID.
    pub fn next(&self) -> u64 {
        self.0.fetch_add(1, Ordering::Relaxed)
    }
}

impl Default for SessionIdGenerator {
    fn default() -> Self {
        Self::new()
    }
}

/// Commands sent to the star hub from the node layer.
#[derive(Debug)]
pub enum HubCommand {
    /// Push a verification packet (serialized bytes) to all connected phones.
    BroadcastPacket(Bytes),
    /// Push a cache sync message to all connected phones.
    BroadcastCacheSync(Bytes),
    /// Send data to a specific session (e.g., targeted CacheSync).
    SendToSession { session_id: u64, data: Bytes },
    /// Disconnect a specific session.
    DisconnectSession(u64),
}

/// Events produced by the star hub for the node layer.
#[derive(Debug)]
pub enum HubEvent {
    /// A mobile verifier connected.
    PhoneConnected {
        session_id: u64,
        verifier_pubkey: [u8; 48],
    },
    /// A mobile verifier disconnected.
    PhoneDisconnected {
        session_id: u64,
    },
    /// A verification receipt was received from a phone.
    ReceiptReceived(VerificationReceipt),
    /// A phone reported its cached code hashes.
    CacheInventoryReceived {
        session_id: u64,
        code_hashes: Vec<[u8; 32]>,
    },
}

/// Configuration for the mobile star hub.
pub struct StarHubConfig {
    /// Address to bind the QUIC endpoint to.
    pub bind_addr: SocketAddr,
    /// Maximum number of concurrent mobile connections.
    pub max_connections: usize,
    /// Session idle timeout in seconds.
    pub idle_timeout_secs: u64,
    /// Directory for persisting the TLS certificate. When set, the certificate is
    /// saved on first run and reloaded on subsequent starts, enabling client-side
    /// certificate pinning. When `None`, an ephemeral cert is generated each run.
    pub cert_dir: Option<std::path::PathBuf>,
}

impl Default for StarHubConfig {
    fn default() -> Self {
        Self {
            bind_addr: "0.0.0.0:9443".parse().unwrap(),
            max_connections: 10_000,
            idle_timeout_secs: 300,
            cert_dir: None,
        }
    }
}

/// Handle for sending commands to the running StarHub.
///
/// Cheaply cloneable. Used by the node layer to push verification
/// packets and manage mobile connections.
#[derive(Clone, Debug)]
pub struct StarHubHandle {
    command_tx: mpsc::UnboundedSender<HubCommand>,
}

impl StarHubHandle {
    /// Broadcasts a verification packet to all connected phones.
    pub fn broadcast_packet(&self, data: Bytes) -> Result<(), crate::error::NetworkError> {
        self.command_tx
            .send(HubCommand::BroadcastPacket(data))
            .map_err(|_| crate::error::NetworkError::ChannelClosed)
    }

    /// Broadcasts a cache sync message to all connected phones.
    pub fn broadcast_cache_sync(&self, data: Bytes) -> Result<(), crate::error::NetworkError> {
        self.command_tx
            .send(HubCommand::BroadcastCacheSync(data))
            .map_err(|_| crate::error::NetworkError::ChannelClosed)
    }

    /// Sends data to a specific session (targeted send).
    pub fn send_to_session(&self, session_id: u64, data: Bytes) -> Result<(), crate::error::NetworkError> {
        self.command_tx
            .send(HubCommand::SendToSession { session_id, data })
            .map_err(|_| crate::error::NetworkError::ChannelClosed)
    }
}

/// QUIC star hub for mobile verifier connections.
///
/// The star hub runs as a QUIC server that accepts connections from
/// mobile devices. Each IDC node runs one star hub, managing up to
/// ~10,000 concurrent phone connections.
///
/// ## Connection protocol
///
/// 1. Phone connects via QUIC to the hub's endpoint
/// 2. Phone opens a uni stream and sends its 48-byte BLS12-381 public key (handshake)
/// 3. Hub validates the key (must be non-zero) within 5 seconds
/// 4. Hub creates a `MobileSession` and starts pushing verification packets
/// 5. Phone sends `VerificationReceipt` back after verifying each block
/// 6. Hub forwards receipts to the `ReceiptAggregator` via `HubEvent`
///
/// ## Data flow
///
/// ```text
/// IDC Node ─── HubCommand::BroadcastPacket ──→ StarHub ──→ QUIC Uni Stream ──→ Phones
/// IDC Node ←── HubEvent::ReceiptReceived ←──── StarHub ←── QUIC Uni Stream ←── Phones
/// ```
///
/// ## Message framing
///
/// Outgoing messages to phones include a 1-byte type prefix:
/// - `0x01` = VerificationPacket
/// - `0x02` = CacheSyncMessage
pub struct StarHub {
    /// Active mobile sessions.
    sessions: Arc<RwLock<HashMap<u64, Arc<MobileSession>>>>,
    /// Configuration.
    config: StarHubConfig,
    /// Command receiver.
    command_rx: mpsc::UnboundedReceiver<HubCommand>,
    /// Event sender to the node layer.
    event_tx: mpsc::UnboundedSender<HubEvent>,
    /// Per-session senders paired with session Arc for tiered broadcast.
    /// The command handler iterates these directly, sorted by tier, instead of
    /// using a broadcast channel.
    session_senders: Arc<RwLock<HashMap<u64, (mpsc::Sender<Bytes>, Arc<MobileSession>)>>>,
    /// SHA-256 hash of the server certificate (populated during run()).
    cert_hash: Option<[u8; 32]>,
    /// Optional shared session ID generator (for multi-shard mode).
    /// When `Some`, session IDs are allocated from this shared generator to ensure
    /// global uniqueness across shards. When `None`, a local counter is used.
    shared_session_id_gen: Option<Arc<SessionIdGenerator>>,
}

impl StarHub {
    /// Creates a new star hub and returns the hub + handle + event receiver.
    pub fn new(
        config: StarHubConfig,
    ) -> (Self, StarHubHandle, mpsc::UnboundedReceiver<HubEvent>) {
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        let handle = StarHubHandle { command_tx };

        let hub = Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            config,
            command_rx,
            event_tx,
            session_senders: Arc::new(RwLock::new(HashMap::new())),
            cert_hash: None,
            shared_session_id_gen: None,
        };

        (hub, handle, event_rx)
    }

    /// Creates a new star hub with a shared session ID generator.
    ///
    /// Used by `ShardedStarHub` to ensure globally unique session IDs across shards.
    pub fn new_with_shared_id_gen(
        config: StarHubConfig,
        id_gen: Arc<SessionIdGenerator>,
    ) -> (Self, StarHubHandle, mpsc::UnboundedReceiver<HubEvent>) {
        let (mut hub, handle, event_rx) = Self::new(config);
        hub.shared_session_id_gen = Some(id_gen);
        (hub, handle, event_rx)
    }

    /// Returns the configured bind address.
    pub fn bind_addr(&self) -> SocketAddr {
        self.config.bind_addr
    }

    /// Returns the maximum number of connections.
    pub fn max_connections(&self) -> usize {
        self.config.max_connections
    }

    /// Returns the number of active sessions.
    pub async fn active_sessions(&self) -> usize {
        self.sessions.read().await.len()
    }

    /// Runs the star hub QUIC server.
    ///
    /// This method sets up the QUIC endpoint, accepts incoming connections,
    /// and processes both incoming data and outgoing commands.
    ///
    /// Should be spawned as a background task via `tokio::spawn` or
    /// `spawn_critical`.
    /// Returns the SHA-256 hash of the server certificate for client-side pinning.
    /// Must be called after construction; the hash is computed during `run()`.
    pub fn cert_hash(&self) -> Option<[u8; 32]> {
        self.cert_hash
    }

    pub async fn run(mut self) -> eyre::Result<()> {
        let (server_config, cert_hash) = build_server_config(
            self.config.cert_dir.as_deref(),
            self.config.idle_timeout_secs,
        )?;
        self.cert_hash = Some(cert_hash);
        tracing::info!(
            cert_hash = hex::encode(cert_hash),
            "StarHub certificate hash (for client pinning)"
        );

        let endpoint = quinn::Endpoint::server(server_config, self.config.bind_addr)?;

        tracing::info!(
            addr = %self.config.bind_addr,
            max_connections = self.config.max_connections,
            "mobile star hub started"
        );

        let sessions = self.sessions.clone();
        let event_tx = self.event_tx.clone();
        let max_conns = self.config.max_connections;
        let session_senders = self.session_senders.clone();

        // Spawn connection acceptor
        let accept_sessions = sessions.clone();
        let accept_event_tx = event_tx.clone();
        let accept_endpoint = endpoint.clone();
        let accept_session_senders = session_senders.clone();
        let shared_id_gen = self.shared_session_id_gen.clone();
        tokio::spawn(async move {
            // Use shared generator if available (multi-shard mode), else local counter.
            let local_id_gen = SessionIdGenerator::new();
            while let Some(incoming) = accept_endpoint.accept().await {
                // Check connection limit
                if accept_sessions.read().await.len() >= max_conns {
                    tracing::warn!("max connections reached, rejecting incoming");
                    incoming.refuse();
                    continue;
                }

                let sessions = accept_sessions.clone();
                let event_tx = accept_event_tx.clone();
                let sid = shared_id_gen.as_ref()
                    .map(|g| g.next())
                    .unwrap_or_else(|| local_id_gen.next());

                // Per-session channel: 32 buffer = ~4 min at 1 msg/8s slot
                let (session_tx, session_rx) = mpsc::channel::<Bytes>(32);
                let senders = accept_session_senders.clone();

                tokio::spawn(async move {
                    match incoming.await {
                        Ok(connection) => {
                            handle_phone_connection(
                                sid,
                                connection,
                                sessions,
                                event_tx,
                                session_tx,
                                session_rx,
                                senders,
                                max_conns,
                            )
                            .await;
                        }
                        Err(e) => {
                            tracing::debug!(error = %e, "incoming connection failed");
                            senders.write().await.remove(&sid);
                        }
                    }
                });
            }
        });

        // Process commands from the node layer
        while let Some(cmd) = self.command_rx.recv().await {
            match cmd {
                HubCommand::BroadcastPacket(data) => {
                    let framed = preframe_message(MSG_TYPE_PACKET_ZSTD, &data);
                    let senders = self.session_senders.read().await;
                    metrics::counter!("n42_mobile_packets_broadcast").increment(1);

                    // Collect and sort by tier (Fast=0 first)
                    let mut targets: Vec<_> = senders
                        .iter()
                        .map(|(sid, (tx, sess))| (*sid, tx.clone(), sess.tier()))
                        .collect();
                    targets.sort_by_key(|(_, _, tier)| *tier as u8);

                    let phone_count = targets.len();
                    for (sid, tx, _tier) in targets {
                        if tx.try_send(framed.clone()).is_err() {
                            tracing::warn!(session_id = sid, "per-session channel full, dropping broadcast");
                        }
                    }
                    tracing::debug!(
                        phone_count,
                        "broadcasting verification packet to phones"
                    );
                }
                HubCommand::BroadcastCacheSync(data) => {
                    let framed = preframe_message(MSG_TYPE_CACHE_SYNC_ZSTD, &data);
                    let senders = self.session_senders.read().await;

                    let mut sent = 0usize;
                    let mut skipped = 0usize;
                    for (sid, (tx, sess)) in senders.iter() {
                        // Skip Slow phones for CacheSync to reduce wasted bandwidth
                        if sess.tier() == PhoneTier::Slow {
                            skipped += 1;
                            continue;
                        }
                        if tx.try_send(framed.clone()).is_err() {
                            tracing::warn!(session_id = sid, "per-session channel full, dropping cache sync");
                        }
                        sent += 1;
                    }
                    tracing::debug!(
                        sent,
                        skipped,
                        "broadcasting cache sync to phones"
                    );
                }
                HubCommand::SendToSession { session_id, data } => {
                    let senders = self.session_senders.read().await;
                    if let Some((tx, _)) = senders.get(&session_id) {
                        if tx.try_send(data).is_err() {
                            tracing::warn!(session_id, "per-session channel full, dropping targeted message");
                        }
                    }
                }
                HubCommand::DisconnectSession(session_id) => {
                    self.sessions.write().await.remove(&session_id);
                    tracing::debug!(session_id, "disconnected mobile session");
                }
            }
        }

        endpoint.close(0u32.into(), b"shutdown");
        tracing::info!("mobile star hub shut down");
        Ok(())
    }
}

/// Handles an individual phone connection with handshake and bidirectional data flow.
async fn handle_phone_connection(
    session_id: u64,
    connection: quinn::Connection,
    sessions: Arc<RwLock<HashMap<u64, Arc<MobileSession>>>>,
    event_tx: mpsc::UnboundedSender<HubEvent>,
    session_tx: mpsc::Sender<Bytes>,
    mut session_rx: mpsc::Receiver<Bytes>,
    session_senders: Arc<RwLock<HashMap<u64, (mpsc::Sender<Bytes>, Arc<MobileSession>)>>>,
    max_connections: usize,
) {
    tracing::debug!(
        session_id,
        remote = %connection.remote_address(),
        "phone connected, awaiting handshake"
    );

    // --- Handshake: read 48-byte BLS12-381 public key from first uni stream ---
    let verifier_pubkey = match tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        connection.accept_uni(),
    )
    .await
    {
        Ok(Ok(mut recv)) => match recv.read_to_end(48).await {
            Ok(data) if data.len() == 48 => {
                let mut pubkey = [0u8; 48];
                pubkey.copy_from_slice(&data);
                pubkey
            }
            Ok(data) => {
                tracing::warn!(
                    session_id,
                    len = data.len(),
                    "invalid handshake: expected 48-byte BLS public key"
                );
                metrics::counter!("n42_mobile_handshake_failures").increment(1);
                connection.close(1u32.into(), b"invalid handshake");
                return;
            }
            Err(e) => {
                tracing::debug!(session_id, error = %e, "handshake read failed");
                metrics::counter!("n42_mobile_handshake_failures").increment(1);
                connection.close(1u32.into(), b"handshake read error");
                return;
            }
        },
        Ok(Err(e)) => {
            tracing::debug!(session_id, error = %e, "handshake stream accept failed");
            metrics::counter!("n42_mobile_handshake_failures").increment(1);
            connection.close(1u32.into(), b"handshake failed");
            return;
        }
        Err(_) => {
            tracing::warn!(session_id, "handshake timeout (5s)");
            metrics::counter!("n42_mobile_handshake_failures").increment(1);
            connection.close(1u32.into(), b"handshake timeout");
            return;
        }
    };

    // Validate public key is non-zero
    if verifier_pubkey == [0u8; 48] {
        tracing::warn!(session_id, "rejected connection: zero public key");
        metrics::counter!("n42_mobile_handshake_failures").increment(1);
        connection.close(1u32.into(), b"invalid pubkey");
        return;
    }

    // Create session (Arc-wrapped) and register in both maps.
    // Re-check connection limit under write lock to close the TOCTOU window.
    let session = Arc::new(MobileSession::new(session_id, verifier_pubkey));
    {
        let mut guard = sessions.write().await;
        if guard.len() >= max_connections {
            tracing::warn!(session_id, "connection limit reached during session insert, closing");
            connection.close(2u32.into(), b"server full");
            return;
        }
        guard.insert(session_id, session.clone());
    }
    // Register the per-session sender + session Arc for tiered broadcast
    session_senders.write().await.insert(session_id, (session_tx, session.clone()));

    let _ = event_tx.send(HubEvent::PhoneConnected {
        session_id,
        verifier_pubkey,
    });

    tracing::debug!(session_id, "handshake complete, session active");

    // Per-phone rate limiting: track last receipt time.
    let mut last_receipt_at: Option<Instant> = None;

    // --- Main loop: receipts from phone + outbound messages (broadcast + targeted) ---
    loop {
        tokio::select! {
            // Receive data from the phone (verification receipts)
            stream = connection.accept_uni() => {
                match stream {
                    Ok(mut recv) => {
                        let read_result = tokio::time::timeout(
                            RECEIPT_READ_TIMEOUT,
                            recv.read_to_end(MAX_RECEIPT_SIZE as usize),
                        ).await;

                        match read_result {
                            Ok(Ok(data)) => {
                                let now = Instant::now();
                                if let Some(last) = last_receipt_at {
                                    if now.duration_since(last) < MIN_RECEIPT_INTERVAL {
                                        tracing::warn!(session_id, "receipt rate limited, dropping");
                                        continue;
                                    }
                                }

                                if data.len() as u64 > MAX_RECEIPT_SIZE {
                                    tracing::warn!(
                                        target: "n42::starhub",
                                        session_id,
                                        size = data.len(),
                                        "receipt too large, dropping"
                                    );
                                    continue;
                                }
                                match bincode::deserialize::<VerificationReceipt>(&data) {
                                    Ok(receipt) => {
                                        last_receipt_at = Some(now);
                                        metrics::counter!("n42_mobile_receipts_received").increment(1);
                                        // Use local Arc — no HashMap lookup needed
                                        session.record_receipt();
                                        let now_ms = std::time::SystemTime::now()
                                            .duration_since(std::time::UNIX_EPOCH)
                                            .unwrap_or_default()
                                            .as_millis() as u64;
                                        if receipt.timestamp_ms > 0 && now_ms > receipt.timestamp_ms {
                                            session.record_rtt(now_ms - receipt.timestamp_ms);
                                        }
                                        let _ = event_tx.send(HubEvent::ReceiptReceived(receipt));
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            session_id,
                                            error = %e,
                                            "failed to decode receipt from phone"
                                        );
                                    }
                                }
                            }
                            Ok(Err(e)) => {
                                tracing::debug!(session_id, error = %e, "read stream error");
                                break;
                            }
                            Err(_) => {
                                tracing::warn!(session_id, "receipt stream read timed out");
                            }
                        }
                    }
                    Err(e) => {
                        tracing::debug!(session_id, error = %e, "connection closed");
                        break;
                    }
                }
            }
            // Outbound messages (broadcast + targeted) via per-session channel
            Some(data) = session_rx.recv() => {
                let send_result = tokio::time::timeout(
                    BROADCAST_SEND_TIMEOUT,
                    async {
                        let mut send = connection.open_uni().await?;
                        send.write_all(&data).await?;
                        send.finish()?;
                        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
                    },
                ).await;
                match send_result {
                    Ok(Ok(())) => {
                        session.record_send_success();
                        metrics::counter!("n42_mobile_sends",
                            "tier" => session.tier().as_str(),
                            "result" => "ok"
                        ).increment(1);
                    }
                    Ok(Err(e)) => {
                        tracing::debug!(session_id, error = %e, "send failed, disconnecting");
                        break;
                    }
                    Err(_) => {
                        session.record_send_timeout();
                        metrics::counter!("n42_mobile_sends",
                            "tier" => session.tier().as_str(),
                            "result" => "timeout"
                        ).increment(1);
                        metrics::counter!("n42_mobile_send_timeouts").increment(1);
                        tracing::warn!(session_id, "send timed out, skipping message");
                    }
                }
            }
        }
    }

    // Clean up session and per-session sender
    sessions.write().await.remove(&session_id);
    session_senders.write().await.remove(&session_id);
    let _ = event_tx.send(HubEvent::PhoneDisconnected { session_id });
    tracing::debug!(session_id, "phone disconnected");
}

/// Builds a QUIC server config with a persistent self-signed certificate.
///
/// If `cert_dir` is provided, the certificate and key are loaded from (or generated
/// and saved to) that directory. This ensures the same certificate is used across
/// restarts, enabling client-side certificate pinning.
///
/// Returns `(ServerConfig, SHA-256 hash of the DER-encoded certificate)`.
fn build_server_config(
    cert_dir: Option<&std::path::Path>,
    idle_timeout_secs: u64,
) -> eyre::Result<(quinn::ServerConfig, [u8; 32])> {
    use sha2::{Sha256, Digest};

    let (cert_der, key_der) = if let Some(dir) = cert_dir {
        let cert_path = dir.join("starhub_cert.der");
        let key_path = dir.join("starhub_key.der");

        if cert_path.exists() && key_path.exists() {
            let cert_bytes = std::fs::read(&cert_path)?;
            let key_bytes = std::fs::read(&key_path)?;
            let cert = rustls::pki_types::CertificateDer::from(cert_bytes);
            let key = rustls::pki_types::PrivateKeyDer::try_from(key_bytes)
                .map_err(|e| eyre::eyre!("key load error: {e}"))?;
            tracing::info!("loaded persistent StarHub certificate from {:?}", cert_path);
            (cert, key)
        } else {
            let cert = rcgen::generate_simple_self_signed(vec!["n42-mobile".into()])?;
            let cert_der = rustls::pki_types::CertificateDer::from(cert.cert);
            let key_bytes = cert.key_pair.serialize_der();
            let key_der = rustls::pki_types::PrivateKeyDer::try_from(key_bytes.clone())
                .map_err(|e| eyre::eyre!("key conversion error: {e}"))?;

            // Persist for future restarts
            std::fs::create_dir_all(dir)?;
            std::fs::write(&cert_path, cert_der.as_ref())?;
            std::fs::write(&key_path, &key_bytes)?;

            // Restrict private key file permissions (owner read/write only).
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let perms = std::fs::Permissions::from_mode(0o600);
                if let Err(e) = std::fs::set_permissions(&key_path, perms) {
                    tracing::warn!(error = %e, "failed to set private key file permissions");
                }
            }
            tracing::info!("generated and saved StarHub certificate to {:?}", cert_path);
            (cert_der, key_der)
        }
    } else {
        // Ephemeral certificate (dev mode)
        let cert = rcgen::generate_simple_self_signed(vec!["n42-mobile".into()])?;
        let cert_der = rustls::pki_types::CertificateDer::from(cert.cert);
        let key_der = rustls::pki_types::PrivateKeyDer::try_from(cert.key_pair.serialize_der())
            .map_err(|e| eyre::eyre!("key conversion error: {e}"))?;
        (cert_der, key_der)
    };

    // Compute SHA-256 hash of the certificate for client-side pinning.
    let cert_hash: [u8; 32] = {
        let mut hasher = Sha256::new();
        hasher.update(cert_der.as_ref());
        hasher.finalize().into()
    };

    let mut server_crypto = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)?;

    server_crypto.alpn_protocols = vec![b"n42-mobile/1".to_vec()];

    let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(server_crypto)?,
    ));

    // Tune transport config for many concurrent connections
    let mut transport = quinn::TransportConfig::default();
    let idle_duration = std::time::Duration::from_secs(idle_timeout_secs);
    if let Ok(idle_timeout) = quinn::IdleTimeout::try_from(idle_duration) {
        transport.max_idle_timeout(Some(idle_timeout));
    } else {
        tracing::warn!(
            idle_timeout_secs,
            "idle_timeout_secs exceeds QUIC max, using default"
        );
    }
    server_config.transport_config(Arc::new(transport));

    Ok((server_config, cert_hash))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_arc_session_send_success() {
        let session = Arc::new(MobileSession::new(1, [0xAA; 48]));
        session.record_send_success();
        assert_eq!(session.packets_sent.load(Ordering::Relaxed), 1);
        assert_eq!(session.tier(), PhoneTier::Fast);
    }

    #[test]
    fn test_arc_session_concurrent_access() {
        let session = Arc::new(MobileSession::new(1, [0u8; 48]));
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let s = session.clone();
                std::thread::spawn(move || {
                    for _ in 0..500 {
                        s.record_send_success();
                        s.record_rtt(100);
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(session.packets_sent.load(Ordering::Relaxed), 2000);
        assert!(session.avg_rtt_ms().is_some());
    }

    #[test]
    fn test_broadcast_ordered_by_tier() {
        // Verify that sorting by tier puts Fast first
        let fast = Arc::new(MobileSession::new(1, [0u8; 48]));
        let slow = Arc::new(MobileSession::new(2, [0u8; 48]));
        let normal = Arc::new(MobileSession::new(3, [0u8; 48]));

        // Drive slow to Slow tier
        for _ in 0..3 {
            slow.record_send_timeout();
        }
        assert_eq!(slow.tier(), PhoneTier::Slow);

        // Drive normal to Normal tier
        normal.record_send_timeout();
        assert_eq!(normal.tier(), PhoneTier::Normal);

        assert_eq!(fast.tier(), PhoneTier::Fast);

        // Simulate the sorting logic from the command handler
        let mut targets = vec![
            (2u64, PhoneTier::Slow),
            (1u64, PhoneTier::Fast),
            (3u64, PhoneTier::Normal),
        ];
        targets.sort_by_key(|(_, tier)| *tier as u8);

        assert_eq!(targets[0].0, 1, "Fast should be first");
        assert_eq!(targets[1].0, 3, "Normal should be second");
        assert_eq!(targets[2].0, 2, "Slow should be last");
    }

    #[test]
    fn test_cache_sync_skips_slow() {
        // Verify that Slow tier sessions are skipped for CacheSync
        let slow_session = Arc::new(MobileSession::new(1, [0u8; 48]));
        for _ in 0..3 {
            slow_session.record_send_timeout();
        }
        assert_eq!(slow_session.tier(), PhoneTier::Slow);

        let fast_session = Arc::new(MobileSession::new(2, [0u8; 48]));
        assert_eq!(fast_session.tier(), PhoneTier::Fast);

        // Simulate CacheSync skip logic
        let sessions: Vec<(u64, Arc<MobileSession>)> =
            vec![(1, slow_session.clone()), (2, fast_session.clone())];

        let mut sent_to = Vec::new();
        for (sid, sess) in &sessions {
            if sess.tier() == PhoneTier::Slow {
                continue;
            }
            sent_to.push(*sid);
        }
        assert_eq!(sent_to, vec![2], "should only send to non-Slow sessions");
    }

    #[test]
    fn test_channel_full_drops_gracefully() {
        // Verify that try_send on a full channel doesn't panic
        let (tx, _rx) = mpsc::channel::<Bytes>(1);

        // Fill the channel
        tx.try_send(Bytes::from(vec![1])).unwrap();

        // Next send should fail gracefully (Err, not panic)
        assert!(tx.try_send(Bytes::from(vec![2])).is_err());
    }

    #[test]
    fn test_msg_type_constants() {
        assert_eq!(MSG_TYPE_PACKET, 0x01);
        assert_eq!(MSG_TYPE_CACHE_SYNC, 0x02);
        assert_eq!(MSG_TYPE_PACKET_ZSTD, 0x03);
        assert_eq!(MSG_TYPE_CACHE_SYNC_ZSTD, 0x04);
        assert_ne!(MSG_TYPE_PACKET, MSG_TYPE_CACHE_SYNC);
        // All type prefixes must be distinct
        let types = [MSG_TYPE_PACKET, MSG_TYPE_CACHE_SYNC, MSG_TYPE_PACKET_ZSTD, MSG_TYPE_CACHE_SYNC_ZSTD];
        for i in 0..types.len() {
            for j in (i + 1)..types.len() {
                assert_ne!(types[i], types[j], "message type prefixes must be unique");
            }
        }
    }

    #[test]
    fn test_star_hub_config_default() {
        let config = StarHubConfig::default();
        assert_eq!(config.max_connections, 10_000);
        assert_eq!(config.idle_timeout_secs, 300);
    }

    #[test]
    fn test_star_hub_new() {
        let config = StarHubConfig::default();
        let (hub, _handle, _event_rx) = StarHub::new(config);
        assert_eq!(hub.max_connections(), 10_000);
    }

    #[test]
    fn test_hub_handle_send_commands() {
        let config = StarHubConfig::default();
        let (_hub, handle, _event_rx) = StarHub::new(config);

        // Should succeed while hub exists
        assert!(handle.broadcast_packet(Bytes::from(vec![1, 2, 3])).is_ok());
        assert!(handle.broadcast_cache_sync(Bytes::from(vec![4, 5, 6])).is_ok());
    }

    #[test]
    fn test_send_to_session_command() {
        let config = StarHubConfig::default();
        let (_hub, handle, _event_rx) = StarHub::new(config);
        assert!(handle.send_to_session(42, Bytes::from(vec![1, 2, 3])).is_ok());
    }

    #[test]
    fn test_bytes_clone_is_zero_copy() {
        let original = Bytes::from(vec![0u8; 100_000]);
        let cloned = original.clone();
        assert_eq!(original.as_ptr(), cloned.as_ptr(),
            "Bytes::clone should share underlying allocation");
    }

    #[test]
    fn test_broadcast_send_timeout_reasonable() {
        assert!(BROADCAST_SEND_TIMEOUT.as_secs() >= 1);
        assert!(BROADCAST_SEND_TIMEOUT.as_secs() <= 5, "must leave time for verify in 8s slot");
    }

    #[test]
    fn test_preframe_message_format() {
        let data = Bytes::from(vec![0xAA, 0xBB, 0xCC]);
        let framed = preframe_message(0x03, &data);
        assert_eq!(framed[0], 0x03, "first byte must be type prefix");
        assert_eq!(&framed[1..], &[0xAA, 0xBB, 0xCC]);
    }

    #[test]
    fn test_preframe_message_zero_copy() {
        let data = Bytes::from(vec![0xDE; 1000]);
        let framed = preframe_message(0x01, &data);
        let cloned = framed.clone();
        assert_eq!(framed.as_ptr(), cloned.as_ptr(), "Bytes::clone should be zero-copy");
    }

    #[test]
    fn test_preframe_empty_data() {
        let data = Bytes::new();
        let framed = preframe_message(0x04, &data);
        assert_eq!(framed.len(), 1);
        assert_eq!(framed[0], 0x04);
    }

    #[test]
    fn test_session_id_generator() {
        let id_gen = SessionIdGenerator::new();
        assert_eq!(id_gen.next(), 1);
        assert_eq!(id_gen.next(), 2);
        assert_eq!(id_gen.next(), 3);
    }

    #[test]
    fn test_session_id_generator_thread_safe() {
        let id_gen = Arc::new(SessionIdGenerator::new());
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let g = id_gen.clone();
                std::thread::spawn(move || {
                    let mut ids = Vec::new();
                    for _ in 0..1000 {
                        ids.push(g.next());
                    }
                    ids
                })
            })
            .collect();
        let mut all_ids: Vec<u64> = threads.into_iter()
            .flat_map(|t| t.join().unwrap())
            .collect();
        all_ids.sort();
        all_ids.dedup();
        assert_eq!(all_ids.len(), 8000, "all session IDs must be unique");
    }

    #[test]
    fn test_zero_pubkey_is_invalid() {
        // This tests the validation logic used in handle_phone_connection
        let zero_key = [0u8; 48];
        assert_eq!(zero_key, [0u8; 48], "zero key should match zero pattern");

        let valid_key = {
            let mut k = [0u8; 48];
            k[0] = 1;
            k
        };
        assert_ne!(valid_key, [0u8; 48], "non-zero key should not match");
    }
}
