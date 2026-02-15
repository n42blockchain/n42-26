use n42_mobile::receipt::VerificationReceipt;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};

use super::session::MobileSession;

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
/// 2. Phone sends its Ed25519 public key + cached code hash inventory
/// 3. Hub creates a `MobileSession` and starts pushing verification packets
/// 4. Phone sends `VerificationReceipt` back after verifying each block
/// 5. Hub forwards receipts to the `ReceiptAggregator` via `HubEvent`
///
/// ## Data flow
///
/// ```text
/// IDC Node ─── HubCommand::BroadcastPacket ──→ StarHub ──→ QUIC Uni Stream ──→ Phones
/// IDC Node ←── HubEvent::ReceiptReceived ←──── StarHub ←── QUIC Uni Stream ←── Phones
/// ```
pub struct StarHub {
    /// Active mobile sessions.
    sessions: Arc<RwLock<HashMap<u64, MobileSession>>>,
    /// Configuration.
    config: StarHubConfig,
    /// Command receiver.
    command_rx: mpsc::UnboundedReceiver<HubCommand>,
    /// Event sender to the node layer.
    event_tx: mpsc::UnboundedSender<HubEvent>,
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
                let sid = session_id_counter;
                session_id_counter += 1;

                tokio::spawn(async move {
                    match incoming.await {
                        Ok(connection) => {
                            handle_phone_connection(sid, connection, sessions, event_tx).await;
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
                HubCommand::BroadcastPacket(_data) => {
                    // In a full implementation, iterate over all sessions
                    // and send the packet via each connection's uni stream.
                    // For now, log the broadcast.
                    let count = self.sessions.read().await.len();
                    tracing::debug!(
                        phone_count = count,
                        "broadcasting verification packet to phones"
                    );
                }
                HubCommand::BroadcastCacheSync(_data) => {
                    let count = self.sessions.read().await.len();
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

/// Handles an individual phone connection.
async fn handle_phone_connection(
    session_id: u64,
    connection: quinn::Connection,
    sessions: Arc<RwLock<HashMap<u64, MobileSession>>>,
    event_tx: mpsc::UnboundedSender<HubEvent>,
) {
    tracing::debug!(
        session_id,
        remote = %connection.remote_address(),
        "phone connected"
    );

    // Create session with a placeholder pubkey.
    // In production, the phone's first message would contain its Ed25519 pubkey.
    let session = MobileSession::new(session_id, [0u8; 32]);
    sessions.write().await.insert(session_id, session);

    let _ = event_tx.send(HubEvent::PhoneConnected {
        session_id,
        verifier_pubkey: [0u8; 32],
    });

    // Read incoming uni streams (receipts from the phone)
    loop {
        match connection.accept_uni().await {
            Ok(mut recv) => {
                match recv.read_to_end(1_048_576).await {
                    Ok(data) => {
                        // Try to decode as VerificationReceipt
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
