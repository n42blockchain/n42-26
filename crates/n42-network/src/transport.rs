use libp2p::gossipsub::{self, PeerScoreParams, PeerScoreThresholds, TopicScoreParams};
use libp2p::identity::Keypair;
use libp2p::swarm::NetworkBehaviour;
use libp2p::swarm::behaviour::toggle::Toggle;
use libp2p::Swarm;
use std::time::Duration;

use crate::gossipsub::message_id_fn;
use crate::gossipsub::topics::{consensus_topic, block_announce_topic, mempool_topic};
use crate::state_sync::StateSyncCodec;

/// The composite network behaviour for N42 nodes.
///
/// Combines GossipSub (for pub/sub consensus messaging), Identify
/// (for peer identification), request-response (for block sync),
/// and optionally mDNS (for LAN peer discovery in dev/test).
#[derive(NetworkBehaviour)]
pub struct N42Behaviour {
    /// GossipSub for consensus message pub/sub.
    pub gossipsub: gossipsub::Behaviour,
    /// Identify protocol for peer discovery metadata.
    pub identify: libp2p::identify::Behaviour,
    /// Request-response protocol for state sync (block catch-up).
    pub state_sync: libp2p::request_response::Behaviour<StateSyncCodec>,
    /// mDNS for automatic LAN peer discovery (dev/test only).
    /// Wrapped in Toggle so it can be disabled in production.
    pub mdns: Toggle<libp2p::mdns::tokio::Behaviour>,
    /// Kademlia DHT for WAN peer discovery (production).
    /// Wrapped in Toggle so it can be disabled in dev/test.
    pub kademlia: Toggle<libp2p::kad::Behaviour<libp2p::kad::store::MemoryStore>>,
}

/// Configuration for the N42 network transport.
pub struct TransportConfig {
    /// GossipSub heartbeat interval (default: 1s).
    pub heartbeat_interval: Duration,
    /// Idle connection timeout (default: 120s).
    pub idle_connection_timeout: Duration,
    /// GossipSub mesh degree D (default: 8 for 100-500 nodes).
    pub mesh_d: usize,
    /// GossipSub low watermark D_low (default: 6).
    pub mesh_d_low: usize,
    /// GossipSub high watermark D_high (default: 12).
    pub mesh_d_high: usize,
    /// GossipSub outbound minimum D_out (default: 2).
    /// Must satisfy: mesh_outbound_min <= mesh_d / 2 AND mesh_outbound_min < mesh_d_low.
    pub mesh_outbound_min: usize,
    /// Enable mDNS for automatic LAN peer discovery (dev/test only).
    pub enable_mdns: bool,
    /// Enable Kademlia DHT for WAN peer discovery (production).
    pub enable_kademlia: bool,
}

impl TransportConfig {
    /// Creates a transport config with GossipSub mesh parameters adapted
    /// to the actual network size.
    ///
    /// GossipSub uses three mesh parameters:
    /// - `mesh_d` (D): target number of peers in the mesh
    /// - `mesh_d_low` (D_low): heartbeat tries to GRAFT peers when below this
    /// - `mesh_d_high` (D_high): heartbeat prunes peers when above this
    ///
    /// For small networks (< 8 nodes), these are scaled down so that the mesh
    /// can actually form. Without this, a 3-node network (2 peers each) would
    /// never satisfy D_low=6 and GossipSub logs continuous "Mesh low" warnings.
    pub fn for_network_size(node_count: usize) -> Self {
        let max_peers = if node_count > 1 { node_count - 1 } else { 1 };

        // Standard params for large networks: D=8, D_low=6, D_high=12, D_out=2
        // Scale down for small networks where max_peers < standard values
        let mesh_d = 8.min(max_peers);
        let mesh_d_low = 6.min(max_peers);
        let mesh_d_high = 12.min(max_peers).max(mesh_d);

        // GossipSub requires: mesh_outbound_min <= mesh_d / 2 AND mesh_outbound_min < mesh_d_low
        let mesh_outbound_min = 2.min(mesh_d / 2).min(if mesh_d_low > 0 { mesh_d_low - 1 } else { 0 });

        Self {
            heartbeat_interval: Duration::from_secs(1),
            idle_connection_timeout: Duration::from_secs(120),
            mesh_d,
            mesh_d_low,
            mesh_d_high,
            mesh_outbound_min,
            enable_mdns: false,
            enable_kademlia: false,
        }
    }
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
            enable_mdns: false,
            enable_kademlia: false,
        }
    }
}

/// Builds the libp2p Swarm with QUIC transport, GossipSub, and Identify.
///
/// Uses QUIC for low-latency encrypted transport (TLS 1.3 built-in,
/// no need for separate Noise handshake). GossipSub is configured
/// for the consensus network topology.
///
/// `validator_index` is included in the Identify agent_version field
/// (format: `n42/1.0.0/v{index}`) to enable directed messaging.
pub fn build_swarm(
    keypair: Keypair,
    config: TransportConfig,
) -> eyre::Result<Swarm<N42Behaviour>> {
    build_swarm_with_validator_index(keypair, config, None)
}

/// Builds the swarm with an optional validator index for directed messaging.
pub fn build_swarm_with_validator_index(
    keypair: Keypair,
    config: TransportConfig,
    validator_index: Option<u32>,
) -> eyre::Result<Swarm<N42Behaviour>> {
    let gossipsub_config = gossipsub::ConfigBuilder::default()
        .heartbeat_interval(config.heartbeat_interval)
        .validation_mode(gossipsub::ValidationMode::Strict)
        // 4MB max message size: block data broadcasts via /n42/blocks/1 topic
        // can reach several MB. Without this, libp2p's default (~65KB) would
        // silently drop block data messages.
        .max_transmit_size(4 * 1024 * 1024)
        .mesh_n(config.mesh_d)
        .mesh_n_low(config.mesh_d_low)
        .mesh_n_high(config.mesh_d_high)
        .mesh_outbound_min(config.mesh_outbound_min)
        .message_id_fn(message_id_fn)
        .build()
        .map_err(|e| eyre::eyre!("gossipsub config error: {e}"))?;

    let peer_id = keypair.public().to_peer_id();

    let swarm = libp2p::SwarmBuilder::with_existing_identity(keypair.clone())
        .with_tokio()
        .with_quic()
        .with_behaviour(|key| {
            // Configure per-topic scoring parameters.
            // Invalid messages give a penalty; first deliveries give a reward.
            let mut peer_score_params = PeerScoreParams::default();

            let mut consensus_score = TopicScoreParams::default();
            consensus_score.topic_weight = 1.0;
            consensus_score.first_message_deliveries_weight = 1.0;
            consensus_score.first_message_deliveries_cap = 100.0;
            consensus_score.first_message_deliveries_decay = 0.99;
            consensus_score.invalid_message_deliveries_weight = -10.0;
            consensus_score.invalid_message_deliveries_decay = 0.9;
            peer_score_params.topics.insert(consensus_topic().hash(), consensus_score);

            let mut block_score = TopicScoreParams::default();
            block_score.topic_weight = 0.5;
            block_score.first_message_deliveries_weight = 1.0;
            block_score.first_message_deliveries_cap = 50.0;
            block_score.first_message_deliveries_decay = 0.99;
            block_score.invalid_message_deliveries_weight = -5.0;
            block_score.invalid_message_deliveries_decay = 0.9;
            peer_score_params.topics.insert(block_announce_topic().hash(), block_score);

            let mut mempool_score = TopicScoreParams::default();
            mempool_score.topic_weight = 0.2;
            mempool_score.invalid_message_deliveries_weight = -2.0;
            mempool_score.invalid_message_deliveries_decay = 0.95;
            peer_score_params.topics.insert(mempool_topic().hash(), mempool_score);

            let thresholds = PeerScoreThresholds {
                gossip_threshold: -50.0,
                publish_threshold: -100.0,
                graylist_threshold: -200.0,
                ..Default::default()
            };

            let mut gossipsub = gossipsub::Behaviour::new(
                gossipsub::MessageAuthenticity::Signed(key.clone()),
                gossipsub_config,
            )
            .map_err(|e| eyre::eyre!("gossipsub behaviour error: {e}"))?;

            gossipsub.with_peer_score(peer_score_params, thresholds)
                .map_err(|e| eyre::eyre!("gossipsub peer scoring error: {e}"))?;

            // Include validator index in agent_version for directed messaging.
            // Format: "n42/1.0.0/v{index}" or "n42/1.0.0" if no index.
            let agent_version = match validator_index {
                Some(idx) => format!("n42/1.0.0/v{idx}"),
                None => "n42/1.0.0".to_string(),
            };
            let identify = libp2p::identify::Behaviour::new(
                libp2p::identify::Config::new(
                    "/n42/1.0.0".into(),
                    key.public(),
                ).with_agent_version(agent_version),
            );

            let state_sync = libp2p::request_response::Behaviour::new(
                [(
                    libp2p::StreamProtocol::new(crate::state_sync::SYNC_PROTOCOL),
                    libp2p::request_response::ProtocolSupport::Full,
                )],
                libp2p::request_response::Config::default(),
            );

            let mdns = if config.enable_mdns {
                let mdns_config = libp2p::mdns::Config {
                    ttl: Duration::from_secs(300),
                    query_interval: Duration::from_secs(60),
                    enable_ipv6: false,
                };
                match libp2p::mdns::tokio::Behaviour::new(mdns_config, key.public().to_peer_id()) {
                    Ok(m) => {
                        tracing::info!("mDNS peer discovery enabled (dev/test mode)");
                        Toggle::from(Some(m))
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "failed to initialize mDNS, continuing without it");
                        Toggle::from(None)
                    }
                }
            } else {
                Toggle::from(None)
            };

            let kademlia = if config.enable_kademlia {
                let local_peer_id = key.public().to_peer_id();
                let store = libp2p::kad::store::MemoryStore::new(local_peer_id);
                let mut kad_config = libp2p::kad::Config::new(
                    libp2p::StreamProtocol::new("/n42/kad/1.0.0"),
                );
                kad_config.set_query_timeout(Duration::from_secs(60));
                let kad = libp2p::kad::Behaviour::with_config(local_peer_id, store, kad_config);
                tracing::info!("Kademlia DHT peer discovery enabled");
                Toggle::from(Some(kad))
            } else {
                Toggle::from(None)
            };

            Ok(N42Behaviour { gossipsub, identify, state_sync, mdns, kademlia })
        })
        .map_err(|e| eyre::eyre!("swarm builder error: {e}"))?
        .with_swarm_config(|cfg| {
            cfg.with_idle_connection_timeout(config.idle_connection_timeout)
        })
        .build();

    tracing::info!(%peer_id, "swarm built with QUIC transport");

    // Subscribe to consensus topics
    // (done in NetworkService::new after construction)

    Ok(swarm)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_for_network_size_3_nodes() {
        let config = TransportConfig::for_network_size(3);
        // 3 nodes → max_peers = 2
        assert_eq!(config.mesh_d, 2);
        assert_eq!(config.mesh_d_low, 2);
        assert_eq!(config.mesh_d_high, 2);
        assert_eq!(config.mesh_outbound_min, 1);
    }

    #[test]
    fn test_for_network_size_21_nodes() {
        let config = TransportConfig::for_network_size(21);
        // 21 nodes → max_peers = 20, standard params apply
        assert_eq!(config.mesh_d, 8);
        assert_eq!(config.mesh_d_low, 6);
        assert_eq!(config.mesh_d_high, 12);
        assert_eq!(config.mesh_outbound_min, 2);
    }

    #[test]
    fn test_for_network_size_100_nodes() {
        let config = TransportConfig::for_network_size(100);
        // Large network → standard params
        assert_eq!(config.mesh_d, 8);
        assert_eq!(config.mesh_d_low, 6);
        assert_eq!(config.mesh_d_high, 12);
        assert_eq!(config.mesh_outbound_min, 2);
    }

    #[test]
    fn test_for_network_size_1_node() {
        let config = TransportConfig::for_network_size(1);
        // Solo node → max_peers = 1 (degenerate case)
        assert_eq!(config.mesh_d, 1);
        assert_eq!(config.mesh_d_low, 1);
        assert_eq!(config.mesh_d_high, 1);
        assert_eq!(config.mesh_outbound_min, 0);
    }

    #[test]
    fn test_for_network_size_invariants() {
        // Verify GossipSub constraints for all reasonable sizes:
        // 1. D_low <= D <= D_high
        // 2. mesh_outbound_min <= D / 2
        // 3. mesh_outbound_min < D_low
        for n in 1..=500 {
            let c = TransportConfig::for_network_size(n);
            assert!(
                c.mesh_d_low <= c.mesh_d,
                "D_low ({}) > D ({}) for n={}", c.mesh_d_low, c.mesh_d, n
            );
            assert!(
                c.mesh_d <= c.mesh_d_high,
                "D ({}) > D_high ({}) for n={}", c.mesh_d, c.mesh_d_high, n
            );
            assert!(
                c.mesh_outbound_min <= c.mesh_d / 2,
                "D_out ({}) > D/2 ({}) for n={}", c.mesh_outbound_min, c.mesh_d / 2, n
            );
            assert!(
                c.mesh_outbound_min < c.mesh_d_low || c.mesh_d_low == 0,
                "D_out ({}) >= D_low ({}) for n={}", c.mesh_outbound_min, c.mesh_d_low, n
            );
        }
    }
}
