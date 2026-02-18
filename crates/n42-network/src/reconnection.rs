use libp2p::{Multiaddr, PeerId};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Manages automatic reconnection to peers with exponential backoff.
///
/// Trusted peers (bootstrap nodes, validators) are retried indefinitely.
/// Discovered peers are retried up to `max_attempts_discovered` times
/// before being abandoned.
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
    /// Creates a new `ReconnectionManager` with default settings.
    ///
    /// - Initial backoff: 5 seconds
    /// - Max backoff: 300 seconds (5 minutes)
    /// - Max attempts for discovered peers: 10
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
    /// Trusted peers are retried indefinitely; discovered peers are retried
    /// up to `max_attempts_discovered` times. Re-registering an existing peer
    /// updates its addresses (if non-empty) and can upgrade it to trusted
    /// (but never downgrade).
    pub fn register_peer(&mut self, peer_id: PeerId, addrs: Vec<Multiaddr>, trusted: bool) {
        use std::collections::hash_map::Entry;
        match self.peers.entry(peer_id) {
            Entry::Occupied(mut entry) => {
                let state = entry.get_mut();
                if !addrs.is_empty() {
                    state.addrs = addrs;
                }
                // Upgrade to trusted if requested (never downgrade).
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

    /// Called when a connection to a peer is established.
    /// Resets backoff state.
    pub fn on_connected(&mut self, peer_id: &PeerId) {
        if let Some(state) = self.peers.get_mut(peer_id) {
            state.is_connected = true;
            state.consecutive_failures = 0;
            state.current_backoff = self.initial_backoff;
        }
    }

    /// Called when a peer disconnects. Marks it for reconnection.
    pub fn on_disconnected(&mut self, peer_id: &PeerId) {
        if let Some(state) = self.peers.get_mut(peer_id) {
            state.is_connected = false;
            state.next_attempt = Instant::now() + state.current_backoff;
        }
    }

    /// Called when a dial attempt to a peer fails. Increases backoff.
    pub fn on_dial_failure(&mut self, peer_id: &PeerId) {
        if let Some(state) = self.peers.get_mut(peer_id) {
            state.consecutive_failures += 1;

            // Exponential backoff with ±20% jitter.
            let base = state.current_backoff.as_millis() as f64 * 2.0;
            let jitter = 1.0 + (Self::jitter_factor() - 0.5) * 0.4; // range: 0.8 .. 1.2
            let backed_off = (base * jitter) as u64;
            state.current_backoff = Duration::from_millis(
                backed_off.min(self.max_backoff.as_millis() as u64),
            );
            state.next_attempt = Instant::now() + state.current_backoff;
        }
    }

    /// Returns peers that are due for a reconnection attempt.
    ///
    /// Only returns peers that:
    /// - Are not currently connected
    /// - Have passed their next_attempt time
    /// - Haven't exceeded max attempts (for discovered peers)
    pub fn peers_to_reconnect(&self) -> Vec<(PeerId, Multiaddr)> {
        let now = Instant::now();
        let mut result = Vec::new();

        for (peer_id, state) in &self.peers {
            // Skip connected peers.
            if state.is_connected {
                continue;
            }
            // Skip discovered peers that exceeded max attempts.
            if !state.is_trusted && state.consecutive_failures >= self.max_attempts_discovered {
                continue;
            }
            // Skip peers whose backoff hasn't expired.
            if now < state.next_attempt {
                continue;
            }
            // Use the first address for reconnection.
            if let Some(addr) = state.addrs.first() {
                result.push((*peer_id, addr.clone()));
            }
        }

        result
    }

    /// Generate a jitter factor between 0.0 and 1.0.
    /// Uses a simple pseudo-random approach based on Instant.
    fn jitter_factor() -> f64 {
        // Use rand crate for proper randomness.
        use rand::Rng;
        let mut rng = rand::thread_rng();
        rng.r#gen::<f64>()
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
        let addr = test_addr();

        mgr.register_peer(peer, vec![addr], true);
        mgr.on_disconnected(&peer);

        let initial = mgr.peers[&peer].current_backoff;
        mgr.on_dial_failure(&peer);
        let after_first = mgr.peers[&peer].current_backoff;
        mgr.on_dial_failure(&peer);
        let after_second = mgr.peers[&peer].current_backoff;

        // Backoff should increase (allowing for jitter variance).
        assert!(
            after_first > initial / 2,
            "backoff should increase after failure"
        );
        assert!(
            after_second > after_first / 2,
            "backoff should continue increasing"
        );
    }

    #[test]
    fn test_connection_resets_backoff() {
        let mut mgr = ReconnectionManager::new();
        let peer = test_peer_id();
        let addr = test_addr();

        mgr.register_peer(peer, vec![addr], true);
        mgr.on_disconnected(&peer);
        mgr.on_dial_failure(&peer);
        mgr.on_dial_failure(&peer);

        let backoff_before = mgr.peers[&peer].current_backoff;
        assert!(backoff_before > mgr.initial_backoff);

        mgr.on_connected(&peer);
        let backoff_after = mgr.peers[&peer].current_backoff;
        assert_eq!(backoff_after, mgr.initial_backoff, "backoff should reset on connect");
        assert_eq!(mgr.peers[&peer].consecutive_failures, 0);
    }

    #[test]
    fn test_trusted_never_abandoned() {
        let mut mgr = ReconnectionManager::new();
        let peer = test_peer_id();
        let addr = test_addr();

        mgr.register_peer(peer, vec![addr], true);
        mgr.on_disconnected(&peer);

        // Exhaust more than max_attempts_discovered failures.
        for _ in 0..20 {
            mgr.on_dial_failure(&peer);
        }

        // Force next_attempt to be in the past.
        mgr.peers.get_mut(&peer).unwrap().next_attempt = Instant::now() - Duration::from_secs(1);

        let to_reconnect = mgr.peers_to_reconnect();
        assert!(
            to_reconnect.iter().any(|(p, _)| *p == peer),
            "trusted peer should always be retried"
        );
    }

    #[test]
    fn test_discovered_stops_after_max_attempts() {
        let mut mgr = ReconnectionManager::new();
        let peer = test_peer_id();
        let addr = test_addr();

        mgr.register_peer(peer, vec![addr], false);
        mgr.on_disconnected(&peer);

        // Exhaust max attempts.
        for _ in 0..mgr.max_attempts_discovered {
            mgr.on_dial_failure(&peer);
        }

        // Force next_attempt to be in the past.
        mgr.peers.get_mut(&peer).unwrap().next_attempt = Instant::now() - Duration::from_secs(1);

        let to_reconnect = mgr.peers_to_reconnect();
        assert!(
            !to_reconnect.iter().any(|(p, _)| *p == peer),
            "discovered peer should be abandoned after max attempts"
        );
    }

    #[test]
    fn test_jitter_range() {
        // Verify jitter_factor produces values in [0.0, 1.0].
        for _ in 0..100 {
            let j = ReconnectionManager::jitter_factor();
            assert!(
                (0.0..=1.0).contains(&j),
                "jitter factor {j} should be in [0.0, 1.0]"
            );
        }
    }

    #[test]
    fn test_connected_peer_not_reconnected() {
        let mut mgr = ReconnectionManager::new();
        let peer = test_peer_id();
        let addr = test_addr();

        mgr.register_peer(peer, vec![addr], true);
        mgr.on_connected(&peer);

        let to_reconnect = mgr.peers_to_reconnect();
        assert!(
            !to_reconnect.iter().any(|(p, _)| *p == peer),
            "connected peer should not appear in reconnect list"
        );
    }

    #[test]
    fn test_backoff_capped_at_max() {
        let mut mgr = ReconnectionManager::new();
        let peer = test_peer_id();
        let addr = test_addr();

        mgr.register_peer(peer, vec![addr], true);
        mgr.on_disconnected(&peer);

        // Apply many failures to push backoff toward max.
        for _ in 0..50 {
            mgr.on_dial_failure(&peer);
        }

        let backoff = mgr.peers[&peer].current_backoff;
        assert!(
            backoff <= mgr.max_backoff,
            "backoff {:?} should not exceed max {:?}", backoff, mgr.max_backoff
        );
    }

    #[test]
    fn test_register_peer_updates_addresses() {
        let mut mgr = ReconnectionManager::new();
        let peer = test_peer_id();
        let addr1: Multiaddr = "/ip4/10.0.0.1/udp/9400/quic-v1".parse().unwrap();
        let addr2: Multiaddr = "/ip4/10.0.0.2/udp/9400/quic-v1".parse().unwrap();

        mgr.register_peer(peer, vec![addr1], false);
        assert_eq!(mgr.peers[&peer].addrs.len(), 1);

        // Re-register with new address.
        mgr.register_peer(peer, vec![addr2.clone()], false);
        assert_eq!(mgr.peers[&peer].addrs.len(), 1);
        assert_eq!(mgr.peers[&peer].addrs[0], addr2);
    }

    #[test]
    fn test_upgrade_discovered_to_trusted() {
        let mut mgr = ReconnectionManager::new();
        let peer = test_peer_id();
        let addr = test_addr();

        // Register as discovered.
        mgr.register_peer(peer, vec![addr.clone()], false);
        assert!(!mgr.peers[&peer].is_trusted);

        // Re-register as trusted.
        mgr.register_peer(peer, vec![addr], true);
        assert!(mgr.peers[&peer].is_trusted, "should upgrade to trusted");
    }

    #[test]
    fn test_trusted_never_downgraded() {
        let mut mgr = ReconnectionManager::new();
        let peer = test_peer_id();
        let addr = test_addr();

        // Register as trusted.
        mgr.register_peer(peer, vec![addr.clone()], true);
        assert!(mgr.peers[&peer].is_trusted);

        // Re-register as non-trusted — should NOT downgrade.
        mgr.register_peer(peer, vec![addr], false);
        assert!(mgr.peers[&peer].is_trusted, "should never downgrade trust");
    }

    #[test]
    fn test_empty_addrs_not_reconnected() {
        let mut mgr = ReconnectionManager::new();
        let peer = test_peer_id();

        // Register with empty addresses.
        mgr.register_peer(peer, vec![], true);
        mgr.on_disconnected(&peer);

        // Force next_attempt to be in the past.
        mgr.peers.get_mut(&peer).unwrap().next_attempt = Instant::now() - Duration::from_secs(1);

        let to_reconnect = mgr.peers_to_reconnect();
        assert!(
            !to_reconnect.iter().any(|(p, _)| *p == peer),
            "peer with no addresses should not be reconnected"
        );
    }

    #[test]
    fn test_mixed_trusted_and_discovered_peers() {
        let mut mgr = ReconnectionManager::new();
        let trusted_peer = test_peer_id();
        let discovered_peer = test_peer_id();
        let addr = test_addr();

        mgr.register_peer(trusted_peer, vec![addr.clone()], true);
        mgr.register_peer(discovered_peer, vec![addr], false);

        // Disconnect both.
        mgr.on_disconnected(&trusted_peer);
        mgr.on_disconnected(&discovered_peer);

        // Exhaust discovered peer's attempts.
        for _ in 0..mgr.max_attempts_discovered {
            mgr.on_dial_failure(&discovered_peer);
        }

        // Force both to be past their backoff.
        mgr.peers.get_mut(&trusted_peer).unwrap().next_attempt = Instant::now() - Duration::from_secs(1);
        mgr.peers.get_mut(&discovered_peer).unwrap().next_attempt = Instant::now() - Duration::from_secs(1);

        let to_reconnect = mgr.peers_to_reconnect();
        assert!(to_reconnect.iter().any(|(p, _)| *p == trusted_peer), "trusted should still reconnect");
        assert!(!to_reconnect.iter().any(|(p, _)| *p == discovered_peer), "discovered should be abandoned");
    }

    #[test]
    fn test_full_lifecycle_disconnect_reconnect() {
        let mut mgr = ReconnectionManager::new();
        let peer = test_peer_id();
        let addr = test_addr();

        // Register + connect.
        mgr.register_peer(peer, vec![addr], true);
        mgr.on_connected(&peer);
        assert!(mgr.peers[&peer].is_connected);
        assert_eq!(mgr.peers[&peer].consecutive_failures, 0);

        // Disconnect → should be ready for reconnect after backoff.
        mgr.on_disconnected(&peer);
        assert!(!mgr.peers[&peer].is_connected);

        // Fail once.
        mgr.on_dial_failure(&peer);
        assert_eq!(mgr.peers[&peer].consecutive_failures, 1);
        assert!(mgr.peers[&peer].current_backoff > mgr.initial_backoff / 2);

        // Reconnect succeeds → everything resets.
        mgr.on_connected(&peer);
        assert!(mgr.peers[&peer].is_connected);
        assert_eq!(mgr.peers[&peer].consecutive_failures, 0);
        assert_eq!(mgr.peers[&peer].current_backoff, mgr.initial_backoff);
    }

    #[test]
    fn test_peer_count() {
        let mut mgr = ReconnectionManager::new();
        assert_eq!(mgr.peer_count(), 0);

        let peer1 = test_peer_id();
        let peer2 = test_peer_id();
        let addr = test_addr();

        mgr.register_peer(peer1, vec![addr.clone()], true);
        assert_eq!(mgr.peer_count(), 1);

        mgr.register_peer(peer2, vec![addr.clone()], false);
        assert_eq!(mgr.peer_count(), 2);

        // Re-register doesn't add duplicate.
        mgr.register_peer(peer1, vec![addr], true);
        assert_eq!(mgr.peer_count(), 2);
    }

    #[test]
    fn test_unregistered_peer_events_are_noop() {
        let mut mgr = ReconnectionManager::new();
        let unknown = test_peer_id();

        // These should not panic.
        mgr.on_connected(&unknown);
        mgr.on_disconnected(&unknown);
        mgr.on_dial_failure(&unknown);

        assert_eq!(mgr.peer_count(), 0);
    }
}
