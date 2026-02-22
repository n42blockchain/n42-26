use libp2p::gossipsub::{self, PeerScoreParams, PeerScoreThresholds, TopicScoreParams};
use libp2p::identity::Keypair;
use libp2p::swarm::NetworkBehaviour;
use libp2p::swarm::behaviour::toggle::Toggle;
use libp2p::Swarm;
use std::time::Duration;

use crate::gossipsub::message_id_fn;
use crate::gossipsub::topics::{blob_sidecar_topic, block_announce_topic, consensus_topic, mempool_topic};
use crate::state_sync::StateSyncCodec;

/// The composite network behaviour for N42 nodes.
///
/// Combines GossipSub (pub/sub consensus), Identify (peer metadata),
/// request-response (block sync), optional mDNS (LAN discovery),
/// optional Kademlia (WAN discovery), and connection limits.
#[derive(NetworkBehaviour)]
pub struct N42Behaviour {
    pub gossipsub: gossipsub::Behaviour,
    pub identify: libp2p::identify::Behaviour,
    pub state_sync: libp2p::request_response::Behaviour<StateSyncCodec>,
    /// Disabled in production; enabled in dev/test via `enable_mdns`.
    pub mdns: Toggle<libp2p::mdns::tokio::Behaviour>,
    /// Disabled in dev/test; enabled in production via `enable_kademlia`.
    pub kademlia: Toggle<libp2p::kad::Behaviour<libp2p::kad::store::MemoryStore>>,
    /// Prevents fd exhaustion from excessive peer connections.
    pub connection_limits: libp2p::connection_limits::Behaviour,
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
    /// Enable mDNS for automatic LAN peer discovery (dev/test only).
    pub enable_mdns: bool,
    /// Enable Kademlia DHT for WAN peer discovery (production).
    pub enable_kademlia: bool,
    pub max_established_incoming: u32,
    pub max_established_outgoing: u32,
    pub max_established_total: u32,
}

impl TransportConfig {
    /// Creates a config with GossipSub mesh parameters scaled for the network size.
    ///
    /// Standard params (D=8, D_low=6, D_high=12, D_out=2) apply when there are
    /// enough peers. For small networks (< 8 nodes), parameters are scaled down
    /// so the mesh can form without continuous "Mesh low" warnings.
    pub fn for_network_size(node_count: usize) -> Self {
        let max_peers = if node_count > 1 { node_count - 1 } else { 1 };

        let mesh_d = 8.min(max_peers);
        let mesh_d_low = 6.min(max_peers);
        let mesh_d_high = 12.min(max_peers).max(mesh_d);
        let mesh_outbound_min =
            2.min(mesh_d / 2).min(if mesh_d_low > 0 { mesh_d_low - 1 } else { 0 });

        Self {
            heartbeat_interval: Duration::from_secs(1),
            idle_connection_timeout: Duration::from_secs(120),
            mesh_d,
            mesh_d_low,
            mesh_d_high,
            mesh_outbound_min,
            enable_mdns: false,
            enable_kademlia: false,
            max_established_incoming: 128,
            max_established_outgoing: 64,
            max_established_total: 192,
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
            max_established_incoming: 128,
            max_established_outgoing: 64,
            max_established_total: 192,
        }
    }
}

/// Builds the libp2p Swarm with QUIC transport and N42Behaviour.
///
/// Uses QUIC for low-latency encrypted transport (TLS 1.3 built-in).
pub fn build_swarm(keypair: Keypair, config: TransportConfig) -> eyre::Result<Swarm<N42Behaviour>> {
    build_swarm_with_validator_index(keypair, config, None)
}

/// Builds the swarm with an optional validator index for directed messaging.
///
/// When set, the validator index is embedded in the Identify `agent_version`
/// field (format: `n42/1.0.0/v{index}`) so peers can map index → PeerId.
pub fn build_swarm_with_validator_index(
    keypair: Keypair,
    config: TransportConfig,
    validator_index: Option<u32>,
) -> eyre::Result<Swarm<N42Behaviour>> {
    let gossipsub_config = gossipsub::ConfigBuilder::default()
        .heartbeat_interval(config.heartbeat_interval)
        // Permissive: messages forwarded automatically after delivery.
        // Application-level validation is in handle_gossipsub_message.
        .validation_mode(gossipsub::ValidationMode::Permissive)
        // 4MB max: block broadcasts can reach several MB.
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
            let peer_score_params = build_peer_score_params();
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

            gossipsub
                .with_peer_score(peer_score_params, thresholds)
                .map_err(|e| eyre::eyre!("gossipsub peer scoring error: {e}"))?;

            let agent_version = match validator_index {
                Some(idx) => format!("n42/1.0.0/v{idx}"),
                None => "n42/1.0.0".to_string(),
            };
            let identify = libp2p::identify::Behaviour::new(
                libp2p::identify::Config::new("/n42/1.0.0".into(), key.public())
                    .with_agent_version(agent_version),
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
                mdns,
                kademlia,
                connection_limits: libp2p::connection_limits::Behaviour::new(limits),
            })
        })
        .map_err(|e| eyre::eyre!("swarm builder error: {e}"))?
        .with_swarm_config(|cfg| cfg.with_idle_connection_timeout(config.idle_connection_timeout))
        .build();

    tracing::info!(%peer_id, "swarm built with QUIC transport");
    Ok(swarm)
}

/// Builds per-topic peer scoring parameters.
fn build_peer_score_params() -> PeerScoreParams {
    let mut params = PeerScoreParams::default();

    params.topics.insert(
        consensus_topic().hash(),
        TopicScoreParams {
            topic_weight: 1.0,
            first_message_deliveries_weight: 1.0,
            first_message_deliveries_cap: 100.0,
            first_message_deliveries_decay: 0.99,
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
            invalid_message_deliveries_weight: -5.0,
            invalid_message_deliveries_decay: 0.9,
            ..Default::default()
        },
    );

    params.topics.insert(
        mempool_topic().hash(),
        TopicScoreParams {
            topic_weight: 0.2,
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

    #[test]
    fn test_for_network_size_3_nodes() {
        let config = TransportConfig::for_network_size(3);
        assert_eq!(config.mesh_d, 2);
        assert_eq!(config.mesh_d_low, 2);
        assert_eq!(config.mesh_d_high, 2);
        assert_eq!(config.mesh_outbound_min, 1);
    }

    #[test]
    fn test_for_network_size_21_nodes() {
        let config = TransportConfig::for_network_size(21);
        assert_eq!(config.mesh_d, 8);
        assert_eq!(config.mesh_d_low, 6);
        assert_eq!(config.mesh_d_high, 12);
        assert_eq!(config.mesh_outbound_min, 2);
    }

    #[test]
    fn test_for_network_size_100_nodes() {
        let config = TransportConfig::for_network_size(100);
        assert_eq!(config.mesh_d, 8);
        assert_eq!(config.mesh_d_low, 6);
        assert_eq!(config.mesh_d_high, 12);
        assert_eq!(config.mesh_outbound_min, 2);
    }

    #[test]
    fn test_for_network_size_1_node() {
        let config = TransportConfig::for_network_size(1);
        assert_eq!(config.mesh_d, 1);
        assert_eq!(config.mesh_d_low, 1);
        assert_eq!(config.mesh_d_high, 1);
        assert_eq!(config.mesh_outbound_min, 0);
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
        let small = TransportConfig::for_network_size(3);
        let large = TransportConfig::for_network_size(500);
        let default = TransportConfig::default();

        assert_eq!(small.max_established_incoming, default.max_established_incoming);
        assert_eq!(small.max_established_outgoing, default.max_established_outgoing);
        assert_eq!(small.max_established_total, default.max_established_total);
        assert_eq!(large.max_established_incoming, default.max_established_incoming);
        assert_eq!(large.max_established_outgoing, default.max_established_outgoing);
        assert_eq!(large.max_established_total, default.max_established_total);
    }
}
