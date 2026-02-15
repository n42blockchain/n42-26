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
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            heartbeat_interval: Duration::from_secs(1),
            idle_connection_timeout: Duration::from_secs(120),
            mesh_d: 8,
            mesh_d_low: 6,
            mesh_d_high: 12,
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
        .mesh_n(config.mesh_d)
        .mesh_n_low(config.mesh_d_low)
        .mesh_n_high(config.mesh_d_high)
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
