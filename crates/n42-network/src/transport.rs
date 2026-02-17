use libp2p::gossipsub;
use libp2p::identity::Keypair;
use libp2p::swarm::NetworkBehaviour;
use libp2p::Swarm;
use std::time::Duration;

use crate::gossipsub::message_id_fn;

/// The composite network behaviour for N42 nodes.
///
/// Combines GossipSub (for pub/sub consensus messaging) and Identify
/// (for peer identification and protocol negotiation).
#[derive(NetworkBehaviour)]
pub struct N42Behaviour {
    /// GossipSub for consensus message pub/sub.
    pub gossipsub: gossipsub::Behaviour,
    /// Identify protocol for peer discovery metadata.
    pub identify: libp2p::identify::Behaviour,
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
        }
    }
}

/// Builds the libp2p Swarm with QUIC transport, GossipSub, and Identify.
///
/// Uses QUIC for low-latency encrypted transport (TLS 1.3 built-in,
/// no need for separate Noise handshake). GossipSub is configured
/// for the consensus network topology.
pub fn build_swarm(
    keypair: Keypair,
    config: TransportConfig,
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
            let gossipsub = gossipsub::Behaviour::new(
                gossipsub::MessageAuthenticity::Signed(key.clone()),
                gossipsub_config,
            )
            .map_err(|e| eyre::eyre!("gossipsub behaviour error: {e}"))?;

            let identify = libp2p::identify::Behaviour::new(
                libp2p::identify::Config::new(
                    "/n42/1.0.0".into(),
                    key.public(),
                ),
            );

            Ok(N42Behaviour { gossipsub, identify })
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
