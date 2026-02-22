use bytes::Bytes;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

use super::star_hub::{HubEvent, SessionIdGenerator, StarHub, StarHubConfig, StarHubHandle};

/// Configuration for a sharded star hub.
///
/// Multiple QUIC endpoints share load across different ports, enabling linear
/// scaling beyond single-socket limits (~10K connections per socket).
pub struct ShardedStarHubConfig {
    /// Base port for the first shard; subsequent shards use base_port+N.
    pub base_port: u16,
    /// Number of QUIC endpoint shards (minimum: 1).
    pub shard_count: usize,
    pub max_connections_per_shard: usize,
    pub idle_timeout_secs: u64,
    /// Shared TLS certificate directory across shards (for client-side pinning).
    pub cert_dir: Option<PathBuf>,
}

impl Default for ShardedStarHubConfig {
    fn default() -> Self {
        Self {
            base_port: 9443,
            shard_count: 1,
            max_connections_per_shard: 10_000,
            idle_timeout_secs: 300,
            cert_dir: None,
        }
    }
}

/// A collection of `StarHub` instances, each on a different port.
///
/// `shard_count=1` degrades to exactly one `StarHub` with no overhead.
/// All shards share a `SessionIdGenerator` for globally unique IDs.
pub struct ShardedStarHub {
    shards: Vec<(StarHub, u16)>,
}

/// Handle for sending commands to all shards.
///
/// Mirrors `StarHubHandle` so callers are unaware of shard topology.
#[derive(Clone, Debug)]
pub struct ShardedStarHubHandle {
    shard_handles: Vec<StarHubHandle>,
}

impl ShardedStarHubHandle {
    /// Broadcasts a verification packet to all shards.
    ///
    /// `Bytes::clone()` is O(1), so fan-out to N shards copies no payload.
    pub fn broadcast_packet(&self, data: Bytes) -> Result<(), crate::error::NetworkError> {
        for handle in &self.shard_handles {
            handle.broadcast_packet(data.clone())?;
        }
        Ok(())
    }

    pub fn broadcast_cache_sync(&self, data: Bytes) -> Result<(), crate::error::NetworkError> {
        for handle in &self.shard_handles {
            handle.broadcast_cache_sync(data.clone())?;
        }
        Ok(())
    }

    /// Sends data to a specific session by fan-outing to all shards.
    ///
    /// Only the shard owning the session ID will match; others silently drop.
    pub fn send_to_session(
        &self,
        session_id: u64,
        data: Bytes,
    ) -> Result<(), crate::error::NetworkError> {
        for handle in &self.shard_handles {
            handle.send_to_session(session_id, data.clone())?;
        }
        Ok(())
    }
}

impl ShardedStarHub {
    /// Creates N shards, returning the hub, a unified handle, and merged event receiver.
    pub fn new(
        config: ShardedStarHubConfig,
    ) -> (Self, ShardedStarHubHandle, mpsc::UnboundedReceiver<HubEvent>) {
        let shard_count = config.shard_count.max(1);
        let shared_id_gen = Arc::new(SessionIdGenerator::new());

        let mut shards = Vec::with_capacity(shard_count);
        let mut shard_handles = Vec::with_capacity(shard_count);
        let (merged_event_tx, merged_event_rx) = mpsc::unbounded_channel();

        for i in 0..shard_count {
            let port = config.base_port + i as u16;
            let shard_config = StarHubConfig {
                bind_addr: format!("0.0.0.0:{port}").parse().unwrap(),
                max_connections: config.max_connections_per_shard,
                idle_timeout_secs: config.idle_timeout_secs,
                cert_dir: config.cert_dir.clone(),
            };

            let (hub, handle, mut event_rx) =
                StarHub::new_with_shared_id_gen(shard_config, shared_id_gen.clone());

            let fwd_tx = merged_event_tx.clone();
            tokio::spawn(async move {
                while let Some(event) = event_rx.recv().await {
                    if fwd_tx.send(event).is_err() {
                        break;
                    }
                }
            });

            shards.push((hub, port));
            shard_handles.push(handle);
        }

        (Self { shards }, ShardedStarHubHandle { shard_handles }, merged_event_rx)
    }

    pub fn ports(&self) -> Vec<u16> {
        self.shards.iter().map(|(_, port)| *port).collect()
    }

    pub fn shard_count(&self) -> usize {
        self.shards.len()
    }

    /// Runs all shards concurrently. Returns when any shard exits.
    pub async fn run(self) -> eyre::Result<()> {
        let mut join_set = tokio::task::JoinSet::new();

        for (hub, port) in self.shards {
            join_set.spawn(async move {
                tracing::info!(port, "starting StarHub shard");
                hub.run().await
            });
        }

        if let Some(result) = join_set.join_next().await {
            match result {
                Ok(Ok(())) => tracing::info!("a StarHub shard exited cleanly"),
                Ok(Err(e)) => {
                    tracing::error!(error = %e, "a StarHub shard exited with error");
                    join_set.abort_all();
                    return Err(e);
                }
                Err(e) => {
                    tracing::error!(error = %e, "a StarHub shard task panicked");
                    join_set.abort_all();
                    return Err(eyre::eyre!("shard task panicked: {e}"));
                }
            }
        }

        join_set.abort_all();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sharded_hub_config_default() {
        let config = ShardedStarHubConfig::default();
        assert_eq!(config.base_port, 9443);
        assert_eq!(config.shard_count, 1);
        assert_eq!(config.max_connections_per_shard, 10_000);
    }

    #[tokio::test]
    async fn test_sharded_hub_single_shard() {
        let config = ShardedStarHubConfig { base_port: 19443, shard_count: 1, ..Default::default() };
        let (hub, handle, _event_rx) = ShardedStarHub::new(config);
        assert_eq!(hub.shard_count(), 1);
        assert_eq!(hub.ports(), vec![19443]);
        assert!(handle.broadcast_packet(Bytes::from(vec![1, 2, 3])).is_ok());
    }

    #[tokio::test]
    async fn test_sharded_hub_port_assignment() {
        let config = ShardedStarHubConfig { base_port: 19443, shard_count: 3, ..Default::default() };
        let (hub, _handle, _event_rx) = ShardedStarHub::new(config);
        assert_eq!(hub.shard_count(), 3);
        assert_eq!(hub.ports(), vec![19443, 19444, 19445]);
    }

    #[tokio::test]
    async fn test_sharded_hub_broadcast_fans_out() {
        let config = ShardedStarHubConfig { base_port: 19446, shard_count: 3, ..Default::default() };
        let (_hub, handle, _event_rx) = ShardedStarHub::new(config);
        assert!(handle.broadcast_packet(Bytes::from(vec![0xAA])).is_ok());
        assert!(handle.broadcast_cache_sync(Bytes::from(vec![0xBB])).is_ok());
    }

    #[tokio::test]
    async fn test_sharded_handle_clone_cheap() {
        let config = ShardedStarHubConfig { base_port: 19449, shard_count: 2, ..Default::default() };
        let (_hub, handle, _event_rx) = ShardedStarHub::new(config);
        let cloned = handle.clone();
        assert!(handle.broadcast_packet(Bytes::from(vec![1])).is_ok());
        assert!(cloned.broadcast_packet(Bytes::from(vec![2])).is_ok());
    }

    #[test]
    fn test_session_id_uniqueness() {
        let id_gen = Arc::new(SessionIdGenerator::new());
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let g = id_gen.clone();
                std::thread::spawn(move || (0..1000).map(|_| g.next()).collect::<Vec<_>>())
            })
            .collect();
        let mut all_ids: Vec<u64> =
            handles.into_iter().flat_map(|h| h.join().unwrap()).collect();
        all_ids.sort();
        all_ids.dedup();
        assert_eq!(all_ids.len(), 4000, "session IDs must be globally unique");
    }

    #[tokio::test]
    async fn test_sharded_hub_zero_shard_count_clamps_to_one() {
        let config = ShardedStarHubConfig { base_port: 19460, shard_count: 0, ..Default::default() };
        let (hub, _handle, _event_rx) = ShardedStarHub::new(config);
        assert_eq!(hub.shard_count(), 1);
        assert_eq!(hub.ports(), vec![19460]);
    }
}
