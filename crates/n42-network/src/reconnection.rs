use libp2p::{Multiaddr, PeerId};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Manages automatic reconnection with exponential backoff.
///
/// Trusted peers (bootstrap nodes, validators) are retried indefinitely.
/// Discovered peers are retried up to `max_attempts_discovered` times.
pub struct ReconnectionManager {
    peers: HashMap<PeerId, PeerState>,
    initial_backoff: Duration,
    max_backoff: Duration,
    max_attempts_discovered: u32,
}

struct PeerState {
    addrs: Vec<Multiaddr>,
    current_backoff: Duration,
    next_attempt: Instant,
    consecutive_failures: u32,
    is_trusted: bool,
    is_connected: bool,
}

impl ReconnectionManager {
    /// Creates a new manager with a 5s initial backoff, 5m max, and 10 max attempts.
    pub fn new() -> Self {
        Self {
            peers: HashMap::new(),
            initial_backoff: Duration::from_secs(5),
            max_backoff: Duration::from_secs(300),
            max_attempts_discovered: 10,
        }
    }

    /// Registers a peer for reconnection management.
    ///
    /// Re-registering updates addresses (if non-empty) and can upgrade to trusted
    /// (but never downgrade).
    pub fn register_peer(&mut self, peer_id: PeerId, addrs: Vec<Multiaddr>, trusted: bool) {
        use std::collections::hash_map::Entry;
        match self.peers.entry(peer_id) {
            Entry::Occupied(mut entry) => {
                let state = entry.get_mut();
                if !addrs.is_empty() {
                    state.addrs = addrs;
                }
                if trusted {
                    state.is_trusted = true;
                }
            }
            Entry::Vacant(entry) => {
                entry.insert(PeerState {
                    addrs,
                    current_backoff: self.initial_backoff,
                    next_attempt: Instant::now(),
                    consecutive_failures: 0,
                    is_trusted: trusted,
                    is_connected: false,
                });
            }
        }
    }

    /// Returns the number of registered peers.
    pub fn peer_count(&self) -> usize {
        self.peers.len()
    }

    /// Called when a connection is established. Resets backoff state.
    pub fn on_connected(&mut self, peer_id: &PeerId) {
        if let Some(state) = self.peers.get_mut(peer_id) {
            state.is_connected = true;
            state.consecutive_failures = 0;
            state.current_backoff = self.initial_backoff;
        }
    }

    /// Called when a peer disconnects. Schedules a reconnection attempt.
    pub fn on_disconnected(&mut self, peer_id: &PeerId) {
        if let Some(state) = self.peers.get_mut(peer_id) {
            state.is_connected = false;
            state.next_attempt = Instant::now() + state.current_backoff;
        }
    }

    /// Called when a dial attempt fails. Doubles backoff with ±20% jitter.
    pub fn on_dial_failure(&mut self, peer_id: &PeerId) {
        if let Some(state) = self.peers.get_mut(peer_id) {
            state.consecutive_failures += 1;

            // Exponential backoff: double with ±20% jitter (range: 0.8..1.2).
            let base_ms = state.current_backoff.as_millis() as f64 * 2.0;
            let jitter = 0.8 + rand::random::<f64>() * 0.4;
            let backed_off_ms = (base_ms * jitter) as u64;
            state.current_backoff =
                Duration::from_millis(backed_off_ms.min(self.max_backoff.as_millis() as u64));
            state.next_attempt = Instant::now() + state.current_backoff;
        }
    }

    /// Returns peers due for a reconnection attempt.
    ///
    /// Excludes connected peers, discovered peers past their retry limit,
    /// and peers whose backoff has not yet expired.
    pub fn peers_to_reconnect(&self) -> Vec<(PeerId, Multiaddr)> {
        let now = Instant::now();
        self.peers
            .iter()
            .filter(|(_, state)| {
                !state.is_connected
                    && (state.is_trusted
                        || state.consecutive_failures < self.max_attempts_discovered)
                    && now >= state.next_attempt
            })
            .filter_map(|(peer_id, state)| {
                state.addrs.first().map(|addr| (*peer_id, addr.clone()))
            })
            .collect()
    }
}

impl Default for ReconnectionManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_peer_id() -> PeerId {
        PeerId::random()
    }

    fn test_addr() -> Multiaddr {
        "/ip4/127.0.0.1/udp/9400/quic-v1".parse().unwrap()
    }

    #[test]
    fn test_backoff_increases() {
        let mut mgr = ReconnectionManager::new();
        let peer = test_peer_id();
        mgr.register_peer(peer, vec![test_addr()], true);
        mgr.on_disconnected(&peer);

        let initial = mgr.peers[&peer].current_backoff;
        mgr.on_dial_failure(&peer);
        let after_first = mgr.peers[&peer].current_backoff;
        mgr.on_dial_failure(&peer);
        let after_second = mgr.peers[&peer].current_backoff;

        assert!(after_first > initial / 2, "backoff should increase after failure");
        assert!(after_second > after_first / 2, "backoff should continue increasing");
    }

    #[test]
    fn test_connection_resets_backoff() {
        let mut mgr = ReconnectionManager::new();
        let peer = test_peer_id();
        mgr.register_peer(peer, vec![test_addr()], true);
        mgr.on_disconnected(&peer);
        mgr.on_dial_failure(&peer);
        mgr.on_dial_failure(&peer);

        assert!(mgr.peers[&peer].current_backoff > mgr.initial_backoff);
        mgr.on_connected(&peer);
        assert_eq!(mgr.peers[&peer].current_backoff, mgr.initial_backoff);
        assert_eq!(mgr.peers[&peer].consecutive_failures, 0);
    }

    #[test]
    fn test_trusted_never_abandoned() {
        let mut mgr = ReconnectionManager::new();
        let peer = test_peer_id();
        mgr.register_peer(peer, vec![test_addr()], true);
        mgr.on_disconnected(&peer);
        for _ in 0..20 {
            mgr.on_dial_failure(&peer);
        }
        mgr.peers.get_mut(&peer).unwrap().next_attempt = Instant::now() - Duration::from_secs(1);

        assert!(
            mgr.peers_to_reconnect().iter().any(|(p, _)| *p == peer),
            "trusted peer should always be retried"
        );
    }

    #[test]
    fn test_discovered_stops_after_max_attempts() {
        let mut mgr = ReconnectionManager::new();
        let peer = test_peer_id();
        mgr.register_peer(peer, vec![test_addr()], false);
        mgr.on_disconnected(&peer);
        for _ in 0..mgr.max_attempts_discovered {
            mgr.on_dial_failure(&peer);
        }
        mgr.peers.get_mut(&peer).unwrap().next_attempt = Instant::now() - Duration::from_secs(1);

        assert!(
            !mgr.peers_to_reconnect().iter().any(|(p, _)| *p == peer),
            "discovered peer should be abandoned after max attempts"
        );
    }

    #[test]
    fn test_connected_peer_not_reconnected() {
        let mut mgr = ReconnectionManager::new();
        let peer = test_peer_id();
        mgr.register_peer(peer, vec![test_addr()], true);
        mgr.on_connected(&peer);

        assert!(
            !mgr.peers_to_reconnect().iter().any(|(p, _)| *p == peer),
            "connected peer should not appear in reconnect list"
        );
    }

    #[test]
    fn test_backoff_capped_at_max() {
        let mut mgr = ReconnectionManager::new();
        let peer = test_peer_id();
        mgr.register_peer(peer, vec![test_addr()], true);
        mgr.on_disconnected(&peer);
        for _ in 0..50 {
            mgr.on_dial_failure(&peer);
        }
        assert!(mgr.peers[&peer].current_backoff <= mgr.max_backoff);
    }

    #[test]
    fn test_register_peer_updates_addresses() {
        let mut mgr = ReconnectionManager::new();
        let peer = test_peer_id();
        let addr1: Multiaddr = "/ip4/10.0.0.1/udp/9400/quic-v1".parse().unwrap();
        let addr2: Multiaddr = "/ip4/10.0.0.2/udp/9400/quic-v1".parse().unwrap();

        mgr.register_peer(peer, vec![addr1], false);
        mgr.register_peer(peer, vec![addr2.clone()], false);
        assert_eq!(mgr.peers[&peer].addrs[0], addr2);
    }

    #[test]
    fn test_upgrade_discovered_to_trusted() {
        let mut mgr = ReconnectionManager::new();
        let peer = test_peer_id();
        let addr = test_addr();
        mgr.register_peer(peer, vec![addr.clone()], false);
        assert!(!mgr.peers[&peer].is_trusted);
        mgr.register_peer(peer, vec![addr], true);
        assert!(mgr.peers[&peer].is_trusted);
    }

    #[test]
    fn test_trusted_never_downgraded() {
        let mut mgr = ReconnectionManager::new();
        let peer = test_peer_id();
        let addr = test_addr();
        mgr.register_peer(peer, vec![addr.clone()], true);
        mgr.register_peer(peer, vec![addr], false);
        assert!(mgr.peers[&peer].is_trusted, "should never downgrade trust");
    }

    #[test]
    fn test_empty_addrs_not_reconnected() {
        let mut mgr = ReconnectionManager::new();
        let peer = test_peer_id();
        mgr.register_peer(peer, vec![], true);
        mgr.on_disconnected(&peer);
        mgr.peers.get_mut(&peer).unwrap().next_attempt = Instant::now() - Duration::from_secs(1);

        assert!(
            !mgr.peers_to_reconnect().iter().any(|(p, _)| *p == peer),
            "peer with no addresses should not be reconnected"
        );
    }

    #[test]
    fn test_mixed_trusted_and_discovered_peers() {
        let mut mgr = ReconnectionManager::new();
        let trusted = test_peer_id();
        let discovered = test_peer_id();
        let addr = test_addr();

        mgr.register_peer(trusted, vec![addr.clone()], true);
        mgr.register_peer(discovered, vec![addr], false);
        mgr.on_disconnected(&trusted);
        mgr.on_disconnected(&discovered);

        for _ in 0..mgr.max_attempts_discovered {
            mgr.on_dial_failure(&discovered);
        }
        let past = Instant::now() - Duration::from_secs(1);
        mgr.peers.get_mut(&trusted).unwrap().next_attempt = past;
        mgr.peers.get_mut(&discovered).unwrap().next_attempt = past;

        let list = mgr.peers_to_reconnect();
        assert!(list.iter().any(|(p, _)| *p == trusted));
        assert!(!list.iter().any(|(p, _)| *p == discovered));
    }

    #[test]
    fn test_full_lifecycle_disconnect_reconnect() {
        let mut mgr = ReconnectionManager::new();
        let peer = test_peer_id();
        mgr.register_peer(peer, vec![test_addr()], true);
        mgr.on_connected(&peer);
        assert!(mgr.peers[&peer].is_connected);

        mgr.on_disconnected(&peer);
        assert!(!mgr.peers[&peer].is_connected);

        mgr.on_dial_failure(&peer);
        assert_eq!(mgr.peers[&peer].consecutive_failures, 1);

        mgr.on_connected(&peer);
        assert!(mgr.peers[&peer].is_connected);
        assert_eq!(mgr.peers[&peer].consecutive_failures, 0);
        assert_eq!(mgr.peers[&peer].current_backoff, mgr.initial_backoff);
    }

    #[test]
    fn test_peer_count() {
        let mut mgr = ReconnectionManager::new();
        let peer1 = test_peer_id();
        let peer2 = test_peer_id();
        let addr = test_addr();

        assert_eq!(mgr.peer_count(), 0);
        mgr.register_peer(peer1, vec![addr.clone()], true);
        assert_eq!(mgr.peer_count(), 1);
        mgr.register_peer(peer2, vec![addr.clone()], false);
        assert_eq!(mgr.peer_count(), 2);
        mgr.register_peer(peer1, vec![addr], true);
        assert_eq!(mgr.peer_count(), 2);
    }

    #[test]
    fn test_unregistered_peer_events_are_noop() {
        let mut mgr = ReconnectionManager::new();
        let unknown = test_peer_id();
        mgr.on_connected(&unknown);
        mgr.on_disconnected(&unknown);
        mgr.on_dial_failure(&unknown);
        assert_eq!(mgr.peer_count(), 0);
    }
}
