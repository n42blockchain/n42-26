use alloy_primitives::B256;
use libp2p::gossipsub::{self, PeerScoreParams, PeerScoreThresholds, TopicScoreParams};
use libp2p::identity::Keypair;
use libp2p::swarm::NetworkBehaviour;
use libp2p::swarm::behaviour::toggle::Toggle;
use libp2p::{PeerId, Swarm};
use std::io;
use std::time::Duration;

use crate::block_direct::BlockDirectCodec;
use crate::consensus_direct::ConsensusDirectCodec;
use crate::gossipsub::message_id_fn;
use crate::gossipsub::topics::{
    blob_sidecar_topic, block_announce_topic, consensus_topic, mempool_topic,
};
use crate::gov5_rpc::{
    GOV5_BLOCK_BY_HASH_PROTOCOL, GOV5_BLOCK_PUSH_PROTOCOL, GOV5_HOTSTUFF_DIRECT_PROTOCOL,
    GOV5_STATUS_PROTOCOL, Gov5BlockByHashCodec, Gov5BlockPushCodec, Gov5HotstuffDirectCodec,
    Gov5StatusCodec,
};
use crate::state_sync::StateSyncCodec;
use crate::tx_forward::TxForwardCodec;

/// Largest message GossipSub will publish or accept.
///
/// A 90k-transfer compact block is already about 8.9 MiB on the wire. Keeping
/// the old 8 MiB ceiling made GossipSub silently drop the fallback while only a
/// subset of validators received the direct push. The block could still reach
/// quorum, but a later leader that missed it could not build on the committed
/// parent and remained `Syncing`. Sixteen MiB leaves measured headroom for
/// 120k-transfer blocks while retaining a bounded receiver allocation.
///
/// This is public so block producers and receiver validation share one limit.
pub const MAX_GOSSIP_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

/// Largest blob-sidecar message GossipSub receivers will accept.
///
/// This is the blob topic's Reject threshold; the publisher packs sidecars
/// into frames no larger than this instead of one all-or-nothing message. One
/// EIP-4844 sidecar is ~137 KiB per blob, so a full 6-blob transaction
/// (~825 KiB) fits in a frame with room to spare. Public for the same reason
/// as [`MAX_GOSSIP_MESSAGE_SIZE`]: a second hard-coded copy on the publish
/// side is exactly what let blob broadcasts outgrow the receivers unnoticed.
pub const MAX_BLOB_GOSSIP_MESSAGE_SIZE: usize = 1024 * 1024;

const DEFAULT_QUIC_MAX_STREAM_DATA: u32 = 40 * 1024 * 1024;
const DEFAULT_QUIC_MAX_CONNECTION_DATA: u32 = 96 * 1024 * 1024;
const DEFAULT_QUIC_MAX_CONCURRENT_STREAMS: u32 = 256;
const DEFAULT_QUIC_UDP_SOCKET_BUFFER_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_BLOCK_DIRECT_REQUEST_TIMEOUT_MS: u64 = 10_000;
const MIN_QUIC_FLOW_WINDOW: u32 = 1024 * 1024;
const MAX_QUIC_FLOW_WINDOW: u32 = 512 * 1024 * 1024;

/// Effective UDP socket buffer sizes after applying the local kernel limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuicUdpSocketBuffers {
    pub matched_sockets: usize,
    pub requested_bytes: usize,
    pub receive_bytes: usize,
    pub send_bytes: usize,
}

/// The composite network behaviour for N42 nodes.
///
/// Combines GossipSub (pub/sub consensus), Identify (peer metadata),
/// request-response (block sync), optional mDNS (LAN discovery),
/// optional Kademlia (WAN discovery), and connection limits.
#[derive(NetworkBehaviour)]
pub struct N42Behaviour {
    /// Reject excess connections before any stateful child behaviour sees
    /// them. Request-response records a connection while constructing its
    /// handler; if a later child rejects that same connection, its cached
    /// `ConnectionId` becomes stale and outbound requests are silently sent
    /// to a handler that was never installed in the swarm.
    pub connection_limits: libp2p::connection_limits::Behaviour,
    /// Direct block data push from leader to validators (bypasses GossipSub).
    ///
    /// Keep this before GossipSub. The derived composite behaviour and its
    /// connection handler poll children in declaration order. A continuously
    /// writable multi-megabyte GossipSub stream can otherwise leave a newly
    /// queued request-response substream below it without ever being polled.
    pub block_direct: libp2p::request_response::Behaviour<BlockDirectCodec>,
    /// Point-to-point consensus messaging (votes, proposals to specific validators).
    pub consensus_direct: libp2p::request_response::Behaviour<ConsensusDirectCodec>,
    pub gossipsub: gossipsub::Behaviour,
    pub identify: libp2p::identify::Behaviour,
    pub state_sync: libp2p::request_response::Behaviour<StateSyncCodec>,
    /// Gov5 reliable block push, enabled inbound for observer compatibility.
    pub gov5_block_push: libp2p::request_response::Behaviour<Gov5BlockPushCodec>,
    /// Gov5 fetch-on-miss block retrieval, enabled outbound for observer catch-up.
    pub gov5_block_by_hash: libp2p::request_response::Behaviour<Gov5BlockByHashCodec>,
    /// Gov5 chain-status handshake, enabled only on the TCP interop observer.
    pub gov5_status: libp2p::request_response::Behaviour<Gov5StatusCodec>,
    /// Gov5 raw one-way Rotor/direct consensus stream.
    pub gov5_hotstuff_direct: libp2p::request_response::Behaviour<Gov5HotstuffDirectCodec>,
    /// Transaction forwarding from non-leader validators to current leader.
    pub tx_forward: libp2p::request_response::Behaviour<TxForwardCodec>,
    /// Disabled in production; enabled in dev/test via `enable_mdns`.
    pub mdns: Toggle<libp2p::mdns::tokio::Behaviour>,
    /// Disabled in dev/test; enabled in production via `enable_kademlia`.
    pub kademlia: Toggle<libp2p::kad::Behaviour<libp2p::kad::store::MemoryStore>>,
}

/// Configuration for the N42 network transport layer.
pub struct TransportConfig {
    /// GossipSub heartbeat interval (default: 1s).
    pub heartbeat_interval: Duration,
    /// Idle connection timeout (default: 120s).
    pub idle_connection_timeout: Duration,
    /// GossipSub mesh target degree D (default: 8).
    pub mesh_d: usize,
    /// GossipSub low watermark D_low (default: 6).
    pub mesh_d_low: usize,
    /// GossipSub high watermark D_high (default: 12).
    pub mesh_d_high: usize,
    /// GossipSub outbound minimum D_out (default: 2).
    /// Must satisfy: D_out <= D/2 AND D_out < D_low.
    pub mesh_outbound_min: usize,
    /// Number of top-scoring peers retained during mesh pruning.
    ///
    /// libp2p defaults this to 4, but that underflows in tiny meshes when the
    /// network later grows (for example 3 validators expanding to 4 at runtime).
    /// Keep it bounded by `mesh_d_high` so heartbeat pruning stays valid.
    pub retain_scores: usize,
    /// Enable mDNS for automatic LAN peer discovery (dev/test only).
    pub enable_mdns: bool,
    /// Enable Kademlia DHT for WAN peer discovery (production).
    pub enable_kademlia: bool,
    pub max_established_incoming: u32,
    pub max_established_outgoing: u32,
    pub max_established_total: u32,
    /// QUIC receive credit available to one stream. libp2p defaults to 10 MB,
    /// which is smaller than the current 16 MiB block propagation budget.
    pub quic_max_stream_data: u32,
    /// QUIC receive credit shared by all streams on one peer connection.
    pub quic_max_connection_data: u32,
    /// Maximum peer-initiated bidirectional QUIC streams per connection.
    pub quic_max_concurrent_streams: u32,
    /// Timeout for a negotiated block-direct request/response substream.
    pub block_direct_request_timeout: Duration,
}

impl TransportConfig {
    /// Creates a config with GossipSub mesh parameters scaled for the network size.
    ///
    /// Standard params (D=8, D_low=6, D_high=12, D_out=2) apply when there are
    /// enough peers. For small networks (< 8 nodes), parameters are scaled down
    /// so the mesh can form without continuous "Mesh low" warnings.
    ///
    /// Mesh degree may be overridden at runtime via environment variables for
    /// Phase 1 (leader direct push) deployments where block data no longer flows
    /// through GossipSub — operators can collapse the mesh to reduce duplicate
    /// gossip overhead:
    ///
    ///   - `N42_GOSSIP_MESH_D`      (target degree)
    ///   - `N42_GOSSIP_MESH_D_LOW`  (low watermark)
    ///   - `N42_GOSSIP_MESH_D_HIGH` (high watermark)
    ///
    /// Overrides are clamped to `[0, max_peers]` and validated so `D_out <= D/2`
    /// and `D_out < D_low` (libp2p invariants); invalid overrides fall back to
    /// the size-scaled defaults.
    pub fn for_network_size(node_count: usize) -> Self {
        let max_peers = if node_count > 1 { node_count - 1 } else { 1 };

        let default_d = 8.min(max_peers);
        let default_d_low = 6.min(max_peers);
        let default_d_high = 12.min(max_peers).max(default_d);

        let mesh_d = env_mesh_override("N42_GOSSIP_MESH_D", max_peers).unwrap_or(default_d);
        let mesh_d_low =
            env_mesh_override("N42_GOSSIP_MESH_D_LOW", max_peers).unwrap_or(default_d_low);
        let mesh_d_high = env_mesh_override("N42_GOSSIP_MESH_D_HIGH", max_peers)
            .unwrap_or(default_d_high)
            .max(mesh_d);
        let mesh_outbound_min =
            2.min(mesh_d / 2)
                .min(if mesh_d_low > 0 { mesh_d_low - 1 } else { 0 });
        let retain_scores = 4.min(mesh_d_high.max(1));
        let quic_max_stream_data =
            env_quic_window("N42_QUIC_MAX_STREAM_DATA", DEFAULT_QUIC_MAX_STREAM_DATA);
        let quic_max_connection_data = env_quic_window(
            "N42_QUIC_MAX_CONNECTION_DATA",
            DEFAULT_QUIC_MAX_CONNECTION_DATA,
        )
        .max(quic_max_stream_data);
        let quic_max_concurrent_streams = std::env::var("N42_QUIC_MAX_CONCURRENT_STREAMS")
            .ok()
            .and_then(|raw| raw.parse::<u32>().ok())
            .unwrap_or(DEFAULT_QUIC_MAX_CONCURRENT_STREAMS)
            .clamp(32, 4096);
        let block_direct_request_timeout = Duration::from_millis(
            std::env::var("N42_BLOCK_DIRECT_REQUEST_TIMEOUT_MS")
                .ok()
                .and_then(|raw| raw.parse::<u64>().ok())
                .unwrap_or(DEFAULT_BLOCK_DIRECT_REQUEST_TIMEOUT_MS)
                .clamp(1_000, 60_000),
        );

        Self {
            heartbeat_interval: Duration::from_secs(1),
            idle_connection_timeout: Duration::from_secs(120),
            mesh_d,
            mesh_d_low,
            mesh_d_high,
            mesh_outbound_min,
            retain_scores,
            enable_mdns: false,
            enable_kademlia: false,
            // Scale connection limits with network size so that every validator
            // can maintain direct connections to all peers when needed.
            max_established_incoming: 128u32.max(max_peers as u32 + 16),
            max_established_outgoing: 64u32.max(max_peers as u32 + 16),
            max_established_total: 192u32.max((max_peers as u32 + 16) * 2),
            quic_max_stream_data,
            quic_max_connection_data,
            quic_max_concurrent_streams,
            block_direct_request_timeout,
        }
    }
}

/// Reads a positive integer mesh parameter from the environment, clamped to
/// `[0, max_peers]`. Returns `None` if the env var is unset or unparseable.
fn env_mesh_override(name: &str, max_peers: usize) -> Option<usize> {
    let raw = std::env::var(name).ok()?;
    let parsed: usize = raw.parse().ok()?;
    Some(parsed.min(max_peers))
}

fn env_quic_window(name: &str, default: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|raw| raw.parse::<u32>().ok())
        .unwrap_or(default)
        .clamp(MIN_QUIC_FLOW_WINDOW, MAX_QUIC_FLOW_WINDOW)
}

/// Requested per-process QUIC UDP receive/send buffer. Linux may cap this at
/// `net.core.rmem_max` / `net.core.wmem_max`; the effective value is logged
/// after the listener becomes visible.
pub fn requested_quic_udp_socket_buffer_bytes() -> usize {
    std::env::var("N42_QUIC_UDP_SOCKET_BUFFER_BYTES")
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .unwrap_or(DEFAULT_QUIC_UDP_SOCKET_BUFFER_BYTES)
        .clamp(256 * 1024, 256 * 1024 * 1024)
}

/// Returns the UDP port only for a QUIC multiaddr.
pub fn quic_udp_port(address: &libp2p::Multiaddr) -> Option<u16> {
    let mut udp_port = None;
    let mut is_quic = false;
    for protocol in address.iter() {
        match protocol {
            libp2p::multiaddr::Protocol::Udp(port) => udp_port = Some(port),
            libp2p::multiaddr::Protocol::QuicV1 => is_quic = true,
            _ => {}
        }
    }
    is_quic.then_some(udp_port).flatten()
}

/// Enlarges all UDP sockets owned by this process that are bound to `port`.
///
/// libp2p creates its QUIC socket internally and currently exposes flow-control
/// settings but not `SO_RCVBUF` / `SO_SNDBUF`. On Linux, locating the already
/// bound descriptor through `/proc/self/fd` lets the node apply the setting
/// without weakening QUIC authentication or replacing the transport.
#[cfg(target_os = "linux")]
pub fn tune_quic_udp_socket_buffers(
    port: u16,
    requested_bytes: usize,
) -> io::Result<Option<QuicUdpSocketBuffers>> {
    use socket2::{SockRef, Type};
    use std::os::fd::BorrowedFd;

    let mut matched_sockets = 0usize;
    let mut receive_bytes = usize::MAX;
    let mut send_bytes = usize::MAX;

    for entry in std::fs::read_dir("/proc/self/fd")? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let Some(raw_fd) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        if raw_fd < 0 {
            continue;
        }

        // SAFETY: `raw_fd` comes from this process' live `/proc/self/fd`
        // directory and is borrowed only for the duration of this iteration.
        let borrowed = unsafe { BorrowedFd::borrow_raw(raw_fd) };
        let socket = SockRef::from(&borrowed);
        if socket.r#type().ok() != Some(Type::DGRAM) {
            continue;
        }
        let Some(local_addr) = socket.local_addr().ok().and_then(|addr| addr.as_socket()) else {
            continue;
        };
        if local_addr.port() != port {
            continue;
        }

        socket.set_recv_buffer_size(requested_bytes)?;
        socket.set_send_buffer_size(requested_bytes)?;
        receive_bytes = receive_bytes.min(socket.recv_buffer_size()?);
        send_bytes = send_bytes.min(socket.send_buffer_size()?);
        matched_sockets += 1;
    }

    if matched_sockets == 0 {
        return Ok(None);
    }
    Ok(Some(QuicUdpSocketBuffers {
        matched_sockets,
        requested_bytes,
        receive_bytes,
        send_bytes,
    }))
}

#[cfg(not(target_os = "linux"))]
pub fn tune_quic_udp_socket_buffers(
    _port: u16,
    _requested_bytes: usize,
) -> io::Result<Option<QuicUdpSocketBuffers>> {
    Ok(None)
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            heartbeat_interval: Duration::from_secs(1),
            idle_connection_timeout: Duration::from_secs(120),
            mesh_d: 8,
            mesh_d_low: 6,
            mesh_d_high: 12,
            mesh_outbound_min: 2,
            retain_scores: 4,
            enable_mdns: false,
            enable_kademlia: false,
            max_established_incoming: 128,
            max_established_outgoing: 64,
            max_established_total: 192,
            quic_max_stream_data: DEFAULT_QUIC_MAX_STREAM_DATA,
            quic_max_connection_data: DEFAULT_QUIC_MAX_CONNECTION_DATA,
            quic_max_concurrent_streams: DEFAULT_QUIC_MAX_CONCURRENT_STREAMS,
            block_direct_request_timeout: Duration::from_millis(
                DEFAULT_BLOCK_DIRECT_REQUEST_TIMEOUT_MS,
            ),
        }
    }
}

/// Builds the libp2p Swarm with QUIC transport and N42Behaviour.
///
/// Uses QUIC for low-latency encrypted transport (TLS 1.3 built-in).
pub fn build_swarm(
    keypair: Keypair,
    config: TransportConfig,
    genesis_hash: B256,
) -> eyre::Result<Swarm<N42Behaviour>> {
    build_swarm_with_validator_index(keypair, config, None, genesis_hash)
}

/// Builds the read-only interop observer swarm with both native QUIC and
/// gov5-compatible TCP/Noise/Yamux transports.
///
/// Existing validators continue to call [`build_swarm_with_validator_index`]
/// and therefore keep their QUIC-only transport surface. The TCP transport is
/// enabled only for the explicitly selected observer runtime.
pub fn build_interop_observer_swarm(
    keypair: Keypair,
    config: TransportConfig,
    genesis_hash: B256,
) -> eyre::Result<Swarm<N42Behaviour>> {
    build_swarm_with_transports(keypair, config, None, true, genesis_hash)
}

/// Builds a voting interop swarm with gov5 TCP/Noise/Yamux transport and an
/// explicit validator identity. Selection remains an opt-in node policy.
pub fn build_interop_participant_swarm(
    keypair: Keypair,
    config: TransportConfig,
    validator_index: u32,
    genesis_hash: B256,
) -> eyre::Result<Swarm<N42Behaviour>> {
    build_swarm_with_transports(keypair, config, Some(validator_index), true, genesis_hash)
}

/// Derives the deterministic libp2p keypair currently used for validator P2P identities.
pub fn deterministic_validator_keypair(index: u32) -> eyre::Result<Keypair> {
    let seed = alloy_primitives::keccak256(format!("n42-p2p-key-{index}").as_bytes());
    let mut seed_bytes: [u8; 32] = seed.0;
    let secret = libp2p::identity::ed25519::SecretKey::try_from_bytes(&mut seed_bytes)
        .map_err(|error| eyre::eyre!("failed to derive deterministic ed25519 key: {error}"))?;
    Ok(Keypair::from(libp2p::identity::ed25519::Keypair::from(
        secret,
    )))
}

/// Returns the deterministic PeerId currently assigned to a validator index.
pub fn deterministic_validator_peer_id(index: u32) -> eyre::Result<PeerId> {
    Ok(deterministic_validator_keypair(index)?
        .public()
        .to_peer_id())
}

/// Builds the swarm with an optional validator index for directed messaging.
///
/// When set, the validator index is embedded in the Identify `agent_version`
/// field (format: `n42/1.0.0/v{index}`) so peers can map index → PeerId.
pub fn build_swarm_with_validator_index(
    keypair: Keypair,
    config: TransportConfig,
    validator_index: Option<u32>,
    genesis_hash: B256,
) -> eyre::Result<Swarm<N42Behaviour>> {
    build_swarm_with_transports(keypair, config, validator_index, false, genesis_hash)
}

fn build_swarm_with_transports(
    keypair: Keypair,
    config: TransportConfig,
    validator_index: Option<u32>,
    enable_gov5_tcp: bool,
    genesis_hash: B256,
) -> eyre::Result<Swarm<N42Behaviour>> {
    let gossipsub_config = gossipsub::ConfigBuilder::default()
        .heartbeat_interval(config.heartbeat_interval)
        // Permissive: messages forwarded automatically after delivery.
        // Application-level validation is in handle_gossipsub_message.
        .validation_mode(gossipsub::ValidationMode::Permissive)
        // High-throughput compact blocks can reach several MiB. Keep this in
        // lockstep with per-topic receiver validation and the producer budget.
        .max_transmit_size(MAX_GOSSIP_MESSAGE_SIZE)
        .mesh_n(config.mesh_d)
        .mesh_n_low(config.mesh_d_low)
        .mesh_n_high(config.mesh_d_high)
        .mesh_outbound_min(config.mesh_outbound_min)
        .retain_scores(config.retain_scores)
        .message_id_fn(message_id_fn(genesis_hash))
        .build()
        .map_err(|e| eyre::eyre!("gossipsub config error: {e}"))?;

    let peer_id = keypair.public().to_peer_id();

    let build_behaviour = |key: &Keypair| {
        let peer_score_params = build_peer_score_params();
        let thresholds = PeerScoreThresholds {
            gossip_threshold: -50.0,
            publish_threshold: -100.0,
            graylist_threshold: -200.0,
            ..Default::default()
        };

        // Gov5 runs GossipSub in StrictNoSign mode. Consensus payloads already
        // carry validator BLS authentication, so an additional libp2p message
        // signature is both redundant and actively incompatible: Gov5 rejects
        // signed GossipSub envelopes and eventually prunes the Rust peer from
        // the mesh, leaving only intermittent IHAVE/IWANT recovery. The
        // anonymous envelope is therefore scoped to gov5-facing swarms only —
        // production N42 swarms keep the signed envelope they always had.
        // Both sides run Permissive validation, so mixed deployments interop.
        let message_authenticity = if enable_gov5_tcp {
            gossipsub::MessageAuthenticity::Anonymous
        } else {
            gossipsub::MessageAuthenticity::Signed(key.clone())
        };
        let mut gossipsub =
            gossipsub::Behaviour::new(message_authenticity, gossipsub_config.clone())
                .map_err(|e| eyre::eyre!("gossipsub behaviour error: {e}"))?;

        gossipsub
            .with_peer_score(peer_score_params, thresholds)
            .map_err(|e| eyre::eyre!("gossipsub peer scoring error: {e}"))?;

        let agent_version = match validator_index {
            Some(idx) => format!("n42/1.0.0/v{idx}"),
            None => "n42/1.0.0".to_string(),
        };
        let identify = libp2p::identify::Behaviour::new(
            libp2p::identify::Config::new("/n42/1.0.0".into(), key.public())
                .with_agent_version(agent_version)
                .with_interval(Duration::from_secs(10)),
        );

        let state_sync = libp2p::request_response::Behaviour::new(
            [(
                libp2p::StreamProtocol::new(crate::state_sync::SYNC_PROTOCOL),
                libp2p::request_response::ProtocolSupport::Full,
            )],
            libp2p::request_response::Config::default(),
        );

        let consensus_direct = libp2p::request_response::Behaviour::new(
            [(
                libp2p::StreamProtocol::new(crate::consensus_direct::CONSENSUS_DIRECT_PROTOCOL),
                libp2p::request_response::ProtocolSupport::Full,
            )],
            libp2p::request_response::Config::default()
                .with_request_timeout(Duration::from_secs(5)),
        );

        let block_direct = libp2p::request_response::Behaviour::new(
            [(
                libp2p::StreamProtocol::new(crate::block_direct::BLOCK_DIRECT_PROTOCOL),
                libp2p::request_response::ProtocolSupport::Full,
            )],
            libp2p::request_response::Config::default()
                .with_request_timeout(config.block_direct_request_timeout),
        );

        let gov5_block_push = libp2p::request_response::Behaviour::new(
            [(
                libp2p::StreamProtocol::new(GOV5_BLOCK_PUSH_PROTOCOL),
                libp2p::request_response::ProtocolSupport::Full,
            )],
            libp2p::request_response::Config::default()
                .with_request_timeout(Duration::from_secs(10)),
        );

        let gov5_block_by_hash = libp2p::request_response::Behaviour::new(
            [(
                libp2p::StreamProtocol::new(GOV5_BLOCK_BY_HASH_PROTOCOL),
                // A Rust validator can be the only peer that retained a block
                // received from Gov5. Advertise the fetch protocol in both
                // directions so another recovering Rust validator can use it
                // as a bounded, authenticated data source.
                libp2p::request_response::ProtocolSupport::Full,
            )],
            libp2p::request_response::Config::default()
                .with_request_timeout(Duration::from_secs(10)),
        );

        let gov5_status_protocols = enable_gov5_tcp
            .then(|| {
                (
                    libp2p::StreamProtocol::new(GOV5_STATUS_PROTOCOL),
                    libp2p::request_response::ProtocolSupport::Full,
                )
            })
            .into_iter();
        let gov5_status = libp2p::request_response::Behaviour::new(
            gov5_status_protocols,
            libp2p::request_response::Config::default()
                .with_request_timeout(Duration::from_secs(10)),
        );

        let gov5_hotstuff_direct = libp2p::request_response::Behaviour::new(
            [(
                libp2p::StreamProtocol::new(GOV5_HOTSTUFF_DIRECT_PROTOCOL),
                libp2p::request_response::ProtocolSupport::Full,
            )],
            libp2p::request_response::Config::default()
                .with_request_timeout(Duration::from_secs(10)),
        );

        let tx_forward = libp2p::request_response::Behaviour::new(
            [(
                libp2p::StreamProtocol::new(crate::tx_forward::TX_FORWARD_PROTOCOL),
                libp2p::request_response::ProtocolSupport::Full,
            )],
            libp2p::request_response::Config::default()
                .with_request_timeout(Duration::from_secs(5)),
        );

        let mdns = if config.enable_mdns {
            let mdns_config = libp2p::mdns::Config {
                ttl: Duration::from_secs(300),
                query_interval: Duration::from_secs(60),
                enable_ipv6: false,
            };
            match libp2p::mdns::tokio::Behaviour::new(mdns_config, key.public().to_peer_id()) {
                Ok(m) => {
                    tracing::info!("mDNS peer discovery enabled");
                    Toggle::from(Some(m))
                }
                Err(e) => {
                    tracing::warn!(error = %e, "mDNS init failed, continuing without it");
                    Toggle::from(None)
                }
            }
        } else {
            Toggle::from(None)
        };

        let kademlia = if config.enable_kademlia {
            let local_peer_id = key.public().to_peer_id();
            let store = libp2p::kad::store::MemoryStore::new(local_peer_id);
            let mut kad_config =
                libp2p::kad::Config::new(libp2p::StreamProtocol::new("/n42/kad/1.0.0"));
            kad_config.set_query_timeout(Duration::from_secs(60));
            let kad = libp2p::kad::Behaviour::with_config(local_peer_id, store, kad_config);
            tracing::info!("Kademlia DHT peer discovery enabled");
            Toggle::from(Some(kad))
        } else {
            Toggle::from(None)
        };

        let limits = libp2p::connection_limits::ConnectionLimits::default()
            .with_max_established_incoming(Some(config.max_established_incoming))
            .with_max_established_outgoing(Some(config.max_established_outgoing))
            .with_max_established(Some(config.max_established_total))
            .with_max_established_per_peer(Some(1));

        Ok(N42Behaviour {
            gossipsub,
            identify,
            state_sync,
            consensus_direct,
            block_direct,
            gov5_block_push,
            gov5_block_by_hash,
            gov5_status,
            gov5_hotstuff_direct,
            tx_forward,
            mdns,
            kademlia,
            connection_limits: libp2p::connection_limits::Behaviour::new(limits),
        })
    };

    let swarm = if enable_gov5_tcp {
        libp2p::SwarmBuilder::with_existing_identity(keypair.clone())
            .with_tokio()
            .with_tcp(
                libp2p::tcp::Config::default().nodelay(true),
                libp2p::noise::Config::new,
                libp2p::yamux::Config::default,
            )?
            .with_quic_config(|mut quic| {
                quic.max_stream_data = config.quic_max_stream_data;
                quic.max_connection_data = config.quic_max_connection_data;
                quic.max_concurrent_stream_limit = config.quic_max_concurrent_streams;
                quic
            })
            .with_behaviour(build_behaviour)
            .map_err(|e| eyre::eyre!("swarm builder error: {e}"))?
            .with_swarm_config(|cfg| {
                cfg.with_idle_connection_timeout(config.idle_connection_timeout)
            })
            .build()
    } else {
        libp2p::SwarmBuilder::with_existing_identity(keypair.clone())
            .with_tokio()
            .with_quic_config(|mut quic| {
                quic.max_stream_data = config.quic_max_stream_data;
                quic.max_connection_data = config.quic_max_connection_data;
                quic.max_concurrent_stream_limit = config.quic_max_concurrent_streams;
                quic
            })
            .with_behaviour(build_behaviour)
            .map_err(|e| eyre::eyre!("swarm builder error: {e}"))?
            .with_swarm_config(|cfg| {
                cfg.with_idle_connection_timeout(config.idle_connection_timeout)
            })
            .build()
    };

    if enable_gov5_tcp {
        tracing::info!(
            %peer_id,
            quic_max_stream_data = config.quic_max_stream_data,
            quic_max_connection_data = config.quic_max_connection_data,
            quic_max_concurrent_streams = config.quic_max_concurrent_streams,
            block_direct_request_timeout_ms = config.block_direct_request_timeout.as_millis(),
            "observer swarm built with QUIC and TCP transports"
        );
    } else {
        tracing::info!(
            %peer_id,
            quic_max_stream_data = config.quic_max_stream_data,
            quic_max_connection_data = config.quic_max_connection_data,
            quic_max_concurrent_streams = config.quic_max_concurrent_streams,
            block_direct_request_timeout_ms = config.block_direct_request_timeout.as_millis(),
            "swarm built with QUIC transport"
        );
    }
    Ok(swarm)
}

/// Builds per-topic peer scoring parameters.
///
/// IMPORTANT: `mesh_message_deliveries_weight` is explicitly set to 0.0 for all topics.
/// The libp2p default penalizes peers for not delivering enough mesh messages, which
/// causes a fatal deadlock on clean startup: no blocks → no messages → peers penalized →
/// peers pruned from mesh → consensus messages undeliverable → permanent chain stall.
/// In a permissioned validator network, BLS signature verification provides message
/// authenticity guarantees; GossipSub delivery scoring is unnecessary.
fn build_peer_score_params() -> PeerScoreParams {
    // Disable IP co-location penalty: in testnet all validators run on the
    // same machine (127.0.0.1), causing gossipsub to penalize every peer for
    // sharing an IP.  In production, validators are on distinct IPs so this
    // has no effect.  Setting the weight to 0 disables the penalty entirely.
    let mut params = PeerScoreParams {
        ip_colocation_factor_weight: 0.0,
        ..Default::default()
    };

    params.topics.insert(
        consensus_topic().hash(),
        TopicScoreParams {
            topic_weight: 1.0,
            first_message_deliveries_weight: 1.0,
            first_message_deliveries_cap: 100.0,
            first_message_deliveries_decay: 0.99,
            mesh_message_deliveries_weight: 0.0,
            invalid_message_deliveries_weight: -10.0,
            invalid_message_deliveries_decay: 0.9,
            ..Default::default()
        },
    );

    params.topics.insert(
        block_announce_topic().hash(),
        TopicScoreParams {
            topic_weight: 0.5,
            first_message_deliveries_weight: 1.0,
            first_message_deliveries_cap: 50.0,
            first_message_deliveries_decay: 0.99,
            mesh_message_deliveries_weight: 0.0,
            invalid_message_deliveries_weight: -5.0,
            invalid_message_deliveries_decay: 0.9,
            ..Default::default()
        },
    );

    params.topics.insert(
        mempool_topic().hash(),
        TopicScoreParams {
            topic_weight: 0.2,
            mesh_message_deliveries_weight: 0.0,
            invalid_message_deliveries_weight: -2.0,
            invalid_message_deliveries_decay: 0.95,
            ..Default::default()
        },
    );

    params.topics.insert(
        blob_sidecar_topic().hash(),
        TopicScoreParams {
            topic_weight: 0.3,
            first_message_deliveries_weight: 0.5,
            first_message_deliveries_cap: 30.0,
            first_message_deliveries_decay: 0.99,
            mesh_message_deliveries_weight: 0.0,
            invalid_message_deliveries_weight: -3.0,
            invalid_message_deliveries_decay: 0.9,
            ..Default::default()
        },
    );

    params
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use libp2p::{Multiaddr, multiaddr::Protocol, swarm::SwarmEvent};

    #[test]
    fn quic_udp_port_requires_quic_protocol() {
        let quic: Multiaddr = "/ip4/127.0.0.1/udp/9000/quic-v1".parse().unwrap();
        let plain_udp: Multiaddr = "/ip4/127.0.0.1/udp/9000".parse().unwrap();
        assert_eq!(quic_udp_port(&quic), Some(9000));
        assert_eq!(quic_udp_port(&plain_udp), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn tunes_owned_udp_socket_buffers() {
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = socket.local_addr().unwrap().port();
        let requested = 512 * 1024;
        let tuned = tune_quic_udp_socket_buffers(port, requested)
            .unwrap()
            .expect("bound UDP socket should be found through /proc/self/fd");
        assert!(tuned.matched_sockets >= 1);
        assert!(tuned.receive_bytes >= requested);
        assert!(tuned.send_bytes >= requested);
    }

    #[test]
    fn test_for_network_size_3_nodes() {
        let config = TransportConfig::for_network_size(3);
        assert_eq!(config.mesh_d, 2);
        assert_eq!(config.mesh_d_low, 2);
        assert_eq!(config.mesh_d_high, 2);
        assert_eq!(config.mesh_outbound_min, 1);
        assert_eq!(config.retain_scores, 2);
    }

    #[test]
    fn test_for_network_size_21_nodes() {
        let config = TransportConfig::for_network_size(21);
        assert_eq!(config.mesh_d, 8);
        assert_eq!(config.mesh_d_low, 6);
        assert_eq!(config.mesh_d_high, 12);
        assert_eq!(config.mesh_outbound_min, 2);
        assert_eq!(config.retain_scores, 4);
    }

    #[test]
    fn test_for_network_size_100_nodes() {
        let config = TransportConfig::for_network_size(100);
        assert_eq!(config.mesh_d, 8);
        assert_eq!(config.mesh_d_low, 6);
        assert_eq!(config.mesh_d_high, 12);
        assert_eq!(config.mesh_outbound_min, 2);
        assert_eq!(config.retain_scores, 4);
    }

    #[test]
    fn test_for_network_size_1_node() {
        let config = TransportConfig::for_network_size(1);
        assert_eq!(config.mesh_d, 1);
        assert_eq!(config.mesh_d_low, 1);
        assert_eq!(config.mesh_d_high, 1);
        assert_eq!(config.mesh_outbound_min, 0);
        assert_eq!(config.retain_scores, 1);
    }

    #[test]
    fn test_for_network_size_invariants() {
        // Verify GossipSub constraints for all reasonable sizes:
        // 1. D_low <= D <= D_high
        // 2. D_out <= D / 2
        // 3. D_out < D_low (or D_low == 0)
        // 4. incoming + outgoing >= total
        for n in 1..=500 {
            let c = TransportConfig::for_network_size(n);
            assert!(c.mesh_d_low <= c.mesh_d, "D_low > D for n={n}");
            assert!(c.mesh_d <= c.mesh_d_high, "D > D_high for n={n}");
            assert!(c.mesh_outbound_min <= c.mesh_d / 2, "D_out > D/2 for n={n}");
            assert!(
                c.retain_scores <= c.mesh_d_high,
                "retain_scores > D_high for n={n}"
            );
            assert!(
                c.mesh_outbound_min < c.mesh_d_low || c.mesh_d_low == 0,
                "D_out >= D_low for n={n}"
            );
            assert!(
                c.max_established_incoming + c.max_established_outgoing >= c.max_established_total,
                "connection limit inconsistency for n={n}"
            );
        }
    }

    #[test]
    fn test_connection_limits_defaults() {
        let config = TransportConfig::default();
        assert_eq!(config.max_established_incoming, 128);
        assert_eq!(config.max_established_outgoing, 64);
        assert_eq!(config.max_established_total, 192);
    }

    #[test]
    fn test_connection_limits_from_network_size() {
        // Small network: uses default minimums.
        let small = TransportConfig::for_network_size(3);
        assert_eq!(small.max_established_incoming, 128); // max(128, 2+16) = 128
        assert_eq!(small.max_established_outgoing, 64); // max(64, 2+16) = 64
        assert_eq!(small.max_established_total, 192); // max(192, (2+16)*2) = 192

        // Large network: scales up beyond defaults.
        let large = TransportConfig::for_network_size(500);
        assert_eq!(large.max_established_incoming, 499 + 16); // max(128, 499+16) = 515
        assert_eq!(large.max_established_outgoing, 499 + 16); // max(64, 499+16) = 515
        assert_eq!(large.max_established_total, (499 + 16) * 2); // max(192, 515*2) = 1030
    }

    #[tokio::test]
    async fn interop_observer_completes_tcp_noise_yamux_handshake() {
        let mut listener = build_interop_observer_swarm(
            Keypair::generate_ed25519(),
            TransportConfig::for_network_size(2),
            B256::repeat_byte(0x11),
        )
        .expect("build TCP-capable observer listener");
        let listener_peer_id = *listener.local_peer_id();
        listener
            .listen_on("/ip4/127.0.0.1/tcp/0".parse::<Multiaddr>().unwrap())
            .expect("listen on ephemeral TCP port");

        let listen_addr = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let SwarmEvent::NewListenAddr { address, .. } = listener.select_next_some().await
                    && address.iter().any(|part| matches!(part, Protocol::Tcp(_)))
                {
                    break address;
                }
            }
        })
        .await
        .expect("TCP listener should become ready");

        let mut dialer = build_interop_observer_swarm(
            Keypair::generate_ed25519(),
            TransportConfig::for_network_size(2),
            B256::repeat_byte(0x11),
        )
        .expect("build TCP-capable observer dialer");
        let mut dial_addr = listen_addr;
        dial_addr.push(Protocol::P2p(listener_peer_id));
        dialer.dial(dial_addr).expect("start TCP dial");

        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                tokio::select! {
                    event = listener.select_next_some() => {
                        if matches!(event, SwarmEvent::ConnectionEstablished { .. }) {
                            break;
                        }
                    }
                    event = dialer.select_next_some() => {
                        if matches!(event, SwarmEvent::ConnectionEstablished { .. }) {
                            break;
                        }
                    }
                }
            }
        })
        .await
        .expect("TCP/Noise/Yamux handshake should complete");
    }
}
