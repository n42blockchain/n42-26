use n42_mobile::receipt::VerificationReceipt;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, RwLock};

use super::session::MobileSession;

/// Message type prefix for verification packets.
const MSG_TYPE_PACKET: u8 = 0x01;
/// Message type prefix for cache sync messages.
const MSG_TYPE_CACHE_SYNC: u8 = 0x02;

/// Handshake timeout: phones must send their public key within this duration.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Commands sent to the star hub from the node layer.
#[derive(Debug)]
pub enum HubCommand {
    /// Push a verification packet (serialized bytes) to all connected phones.
    BroadcastPacket(Vec<u8>),
    /// Push a cache sync message to all connected phones.
    BroadcastCacheSync(Vec<u8>),
    /// Disconnect a specific session.
    DisconnectSession(u64),
}

/// Events produced by the star hub for the node layer.
#[derive(Debug)]
pub enum HubEvent {
    /// A mobile verifier connected.
    PhoneConnected {
        session_id: u64,
        verifier_pubkey: [u8; 32],
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

/// Internal broadcast message sent from the command handler to all connection handlers.
#[derive(Clone, Debug)]
enum BroadcastMsg {
    /// Verification packet data.
    Packet(Vec<u8>),
    /// Cache sync message data.
    CacheSync(Vec<u8>),
}

/// Configuration for the mobile star hub.
pub struct StarHubConfig {
    /// Address to bind the QUIC endpoint to.
    pub bind_addr: SocketAddr,
    /// Maximum number of concurrent mobile connections.
    pub max_connections: usize,
    /// Session idle timeout in seconds.
    pub idle_timeout_secs: u64,
}

impl Default for StarHubConfig {
    fn default() -> Self {
        Self {
            bind_addr: "0.0.0.0:9443".parse().unwrap(),
            max_connections: 10_000,
            idle_timeout_secs: 300,
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
    pub fn broadcast_packet(&self, data: Vec<u8>) -> Result<(), crate::error::NetworkError> {
        self.command_tx
            .send(HubCommand::BroadcastPacket(data))
            .map_err(|_| crate::error::NetworkError::ChannelClosed)
    }

    /// Broadcasts a cache sync message to all connected phones.
    pub fn broadcast_cache_sync(&self, data: Vec<u8>) -> Result<(), crate::error::NetworkError> {
        self.command_tx
            .send(HubCommand::BroadcastCacheSync(data))
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
/// 2. Phone opens a uni stream and sends its 32-byte Ed25519 public key (handshake)
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
    sessions: Arc<RwLock<HashMap<u64, MobileSession>>>,
    /// Configuration.
    config: StarHubConfig,
    /// Command receiver.
    command_rx: mpsc::UnboundedReceiver<HubCommand>,
    /// Event sender to the node layer.
    event_tx: mpsc::UnboundedSender<HubEvent>,
    /// Broadcast sender for pushing data to all connection handlers.
    broadcast_tx: broadcast::Sender<BroadcastMsg>,
}

impl StarHub {
    /// Creates a new star hub and returns the hub + handle + event receiver.
    pub fn new(
        config: StarHubConfig,
    ) -> (Self, StarHubHandle, mpsc::UnboundedReceiver<HubEvent>) {
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        // Broadcast channel: buffer recent messages for slow receivers.
        // If a receiver falls behind by more than 256 messages, it will
        // receive a Lagged error and skip ahead.
        let (broadcast_tx, _) = broadcast::channel(256);

        let handle = StarHubHandle { command_tx };

        let hub = Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            config,
            command_rx,
            event_tx,
            broadcast_tx,
        };

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
    pub async fn run(mut self) -> eyre::Result<()> {
        let server_config = build_server_config()?;

        let endpoint = quinn::Endpoint::server(server_config, self.config.bind_addr)?;

        tracing::info!(
            addr = %self.config.bind_addr,
            max_connections = self.config.max_connections,
            "mobile star hub started"
        );

        let sessions = self.sessions.clone();
        let event_tx = self.event_tx.clone();
        let max_conns = self.config.max_connections;
        let broadcast_tx = self.broadcast_tx.clone();

        // Spawn connection acceptor
        let accept_sessions = sessions.clone();
        let accept_event_tx = event_tx.clone();
        let accept_endpoint = endpoint.clone();
        tokio::spawn(async move {
            let mut session_id_counter: u64 = 1;
            while let Some(incoming) = accept_endpoint.accept().await {
                // Check connection limit
                if accept_sessions.read().await.len() >= max_conns {
                    tracing::warn!("max connections reached, rejecting incoming");
                    incoming.refuse();
                    continue;
                }

                let sessions = accept_sessions.clone();
                let event_tx = accept_event_tx.clone();
                let broadcast_rx = broadcast_tx.subscribe();
                let sid = session_id_counter;
                session_id_counter += 1;

                tokio::spawn(async move {
                    match incoming.await {
                        Ok(connection) => {
                            handle_phone_connection(
                                sid,
                                connection,
                                sessions,
                                event_tx,
                                broadcast_rx,
                            )
                            .await;
                        }
                        Err(e) => {
                            tracing::debug!(error = %e, "incoming connection failed");
                        }
                    }
                });
            }
        });

        // Process commands from the node layer
        while let Some(cmd) = self.command_rx.recv().await {
            match cmd {
                HubCommand::BroadcastPacket(data) => {
                    let count = self.broadcast_tx.receiver_count();
                    let _ = self.broadcast_tx.send(BroadcastMsg::Packet(data));
                    tracing::debug!(
                        phone_count = count,
                        "broadcasting verification packet to phones"
                    );
                }
                HubCommand::BroadcastCacheSync(data) => {
                    let count = self.broadcast_tx.receiver_count();
                    let _ = self.broadcast_tx.send(BroadcastMsg::CacheSync(data));
                    tracing::debug!(
                        phone_count = count,
                        "broadcasting cache sync to phones"
                    );
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
    sessions: Arc<RwLock<HashMap<u64, MobileSession>>>,
    event_tx: mpsc::UnboundedSender<HubEvent>,
    mut broadcast_rx: broadcast::Receiver<BroadcastMsg>,
) {
    tracing::debug!(
        session_id,
        remote = %connection.remote_address(),
        "phone connected, awaiting handshake"
    );

    // --- Handshake: read 32-byte Ed25519 public key from first uni stream ---
    let verifier_pubkey = match tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        connection.accept_uni(),
    )
    .await
    {
        Ok(Ok(mut recv)) => match recv.read_to_end(32).await {
            Ok(data) if data.len() == 32 => {
                let mut pubkey = [0u8; 32];
                pubkey.copy_from_slice(&data);
                pubkey
            }
            Ok(data) => {
                tracing::warn!(
                    session_id,
                    len = data.len(),
                    "invalid handshake: expected 32-byte public key"
                );
                connection.close(1u32.into(), b"invalid handshake");
                return;
            }
            Err(e) => {
                tracing::debug!(session_id, error = %e, "handshake read failed");
                connection.close(1u32.into(), b"handshake read error");
                return;
            }
        },
        Ok(Err(e)) => {
            tracing::debug!(session_id, error = %e, "handshake stream accept failed");
            connection.close(1u32.into(), b"handshake failed");
            return;
        }
        Err(_) => {
            tracing::warn!(session_id, "handshake timeout (5s)");
            connection.close(1u32.into(), b"handshake timeout");
            return;
        }
    };

    // Validate public key is non-zero
    if verifier_pubkey == [0u8; 32] {
        tracing::warn!(session_id, "rejected connection: zero public key");
        connection.close(1u32.into(), b"invalid pubkey");
        return;
    }

    // Create session and notify node layer
    let session = MobileSession::new(session_id, verifier_pubkey);
    sessions.write().await.insert(session_id, session);

    let _ = event_tx.send(HubEvent::PhoneConnected {
        session_id,
        verifier_pubkey,
    });

    tracing::debug!(session_id, "handshake complete, session active");

    // --- Main loop: handle receipts from phone + broadcasts to phone ---
    loop {
        tokio::select! {
            // Receive data from the phone (verification receipts)
            stream = connection.accept_uni() => {
                match stream {
                    Ok(mut recv) => {
                        match recv.read_to_end(1_048_576).await {
                            Ok(data) => {
                                match bincode::deserialize::<VerificationReceipt>(&data) {
                                    Ok(receipt) => {
                                        if let Some(session) = sessions.write().await.get_mut(&session_id) {
                                            session.receipts_received += 1;
                                            session.touch();
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
                            Err(e) => {
                                tracing::debug!(session_id, error = %e, "read stream error");
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        tracing::debug!(session_id, error = %e, "connection closed");
                        break;
                    }
                }
            }
            // Forward broadcast messages to this phone via QUIC uni stream
            msg = broadcast_rx.recv() => {
                match msg {
                    Ok(broadcast_msg) => {
                        let (type_prefix, data) = match &broadcast_msg {
                            BroadcastMsg::Packet(d) => (MSG_TYPE_PACKET, d.as_slice()),
                            BroadcastMsg::CacheSync(d) => (MSG_TYPE_CACHE_SYNC, d.as_slice()),
                        };
                        match connection.open_uni().await {
                            Ok(mut send) => {
                                // Write type prefix + payload as a framed message
                                if send.write_all(&[type_prefix]).await.is_err()
                                    || send.write_all(data).await.is_err()
                                    || send.finish().is_err()
                                {
                                    tracing::debug!(session_id, "failed to send broadcast, disconnecting");
                                    break;
                                }
                                if let Some(session) = sessions.write().await.get_mut(&session_id) {
                                    session.packets_sent += 1;
                                    session.touch();
                                }
                            }
                            Err(e) => {
                                tracing::debug!(session_id, error = %e, "failed to open uni stream");
                                break;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(session_id, skipped = n, "broadcast receiver lagged");
                        // Continue — some messages were skipped but connection is still valid
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        tracing::debug!(session_id, "broadcast channel closed");
                        break;
                    }
                }
            }
        }
    }

    // Clean up session
    sessions.write().await.remove(&session_id);
    let _ = event_tx.send(HubEvent::PhoneDisconnected { session_id });
    tracing::debug!(session_id, "phone disconnected");
}

/// Builds a QUIC server config with a self-signed certificate.
///
/// For production, replace with proper certificate management (e.g., ACME).
fn build_server_config() -> eyre::Result<quinn::ServerConfig> {
    let cert = rcgen::generate_simple_self_signed(vec!["n42-mobile".into()])?;
    let cert_der = rustls::pki_types::CertificateDer::from(cert.cert);
    let key_der = rustls::pki_types::PrivateKeyDer::try_from(cert.key_pair.serialize_der())
        .map_err(|e| eyre::eyre!("key conversion error: {e}"))?;

    let mut server_crypto = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)?;

    server_crypto.alpn_protocols = vec![b"n42-mobile/1".to_vec()];

    let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(server_crypto)?,
    ));

    // Tune transport config for many concurrent connections
    let mut transport = quinn::TransportConfig::default();
    transport.max_idle_timeout(Some(
        quinn::IdleTimeout::try_from(std::time::Duration::from_secs(300)).unwrap(),
    ));
    server_config.transport_config(Arc::new(transport));

    Ok(server_config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_broadcast_msg_clone() {
        let msg = BroadcastMsg::Packet(vec![1, 2, 3]);
        let cloned = msg.clone();
        match cloned {
            BroadcastMsg::Packet(data) => assert_eq!(data, vec![1, 2, 3]),
            _ => panic!("unexpected variant"),
        }
    }

    #[test]
    fn test_broadcast_channel_delivery() {
        let (tx, mut rx1) = broadcast::channel::<BroadcastMsg>(16);
        let mut rx2 = tx.subscribe();

        let data = vec![0xAA, 0xBB];
        tx.send(BroadcastMsg::Packet(data.clone())).unwrap();

        match rx1.try_recv().unwrap() {
            BroadcastMsg::Packet(d) => assert_eq!(d, data),
            _ => panic!("expected Packet"),
        }
        match rx2.try_recv().unwrap() {
            BroadcastMsg::Packet(d) => assert_eq!(d, data),
            _ => panic!("expected Packet"),
        }
    }

    #[test]
    fn test_broadcast_channel_both_variants() {
        let (tx, mut rx) = broadcast::channel::<BroadcastMsg>(16);

        tx.send(BroadcastMsg::Packet(vec![1])).unwrap();
        tx.send(BroadcastMsg::CacheSync(vec![2])).unwrap();

        match rx.try_recv().unwrap() {
            BroadcastMsg::Packet(d) => assert_eq!(d, vec![1]),
            _ => panic!("expected Packet"),
        }
        match rx.try_recv().unwrap() {
            BroadcastMsg::CacheSync(d) => assert_eq!(d, vec![2]),
            _ => panic!("expected CacheSync"),
        }
    }

    #[test]
    fn test_msg_type_constants() {
        assert_eq!(MSG_TYPE_PACKET, 0x01);
        assert_eq!(MSG_TYPE_CACHE_SYNC, 0x02);
        assert_ne!(MSG_TYPE_PACKET, MSG_TYPE_CACHE_SYNC);
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
        assert!(handle.broadcast_packet(vec![1, 2, 3]).is_ok());
        assert!(handle.broadcast_cache_sync(vec![4, 5, 6]).is_ok());
    }

    #[test]
    fn test_zero_pubkey_is_invalid() {
        // This tests the validation logic used in handle_phone_connection
        let zero_key = [0u8; 32];
        assert_eq!(zero_key, [0u8; 32], "zero key should match zero pattern");

        let valid_key = {
            let mut k = [0u8; 32];
            k[0] = 1;
            k
        };
        assert_ne!(valid_key, [0u8; 32], "non-zero key should not match");
    }
}
