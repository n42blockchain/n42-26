//! Per-peer rate limiter for messages that would force the consensus engine
//! into the QC view-jump path.
//!
//! `ConsensusEngine::try_qc_view_jump` runs an aggregate BLS verification on
//! every message whose `view` lies more than `FUTURE_VIEW_WINDOW` ahead of the
//! local current view. A Byzantine peer can spam such messages to drown the
//! node in BLS pairings. We bound the number of view-jump attempts per peer
//! using a simple token bucket evaluated at the orchestrator boundary, before
//! the message is even handed to the engine.
//!
//! Threshold defaults: 4 tokens, 4-per-second refill, 256-peer cap with FIFO
//! eviction. Honest sync recovery rarely needs more than 1-2 view jumps per
//! second, so 4 leaves headroom for short bursts; the per-peer cap stops a
//! flood of new PeerIds from exhausting memory.
//!
//! See `Plan #3` in `~/.claude/plans/atomic-dreaming-hare.md`.

use n42_network::PeerId;
use std::collections::HashMap;
use tokio::time::Instant;

/// Default tokens granted to each peer.
const DEFAULT_BURST: u32 = 4;
/// Default refill rate per peer (tokens per second).
const DEFAULT_REFILL_PER_SEC: u32 = 4;
/// Hard cap on tracked peers; oldest entries are evicted FIFO.
const MAX_TRACKED_PEERS: usize = 256;

/// Token bucket state for one peer.
#[derive(Debug, Clone)]
struct TokenBucket {
    tokens: f64,
    last_refill: Instant,
}

impl TokenBucket {
    fn new(burst: u32, now: Instant) -> Self {
        Self {
            tokens: burst as f64,
            last_refill: now,
        }
    }

    /// Adds tokens accrued since `last_refill` (capped at `burst`), then
    /// consumes one if available. Returns `true` if a token was consumed.
    fn try_consume(&mut self, burst: u32, refill_per_sec: u32, now: Instant) -> bool {
        let elapsed = now
            .saturating_duration_since(self.last_refill)
            .as_secs_f64();
        self.tokens = (self.tokens + elapsed * refill_per_sec as f64).min(burst as f64);
        self.last_refill = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Per-peer view-jump throttle.
#[derive(Debug)]
pub(crate) struct ViewJumpThrottle {
    burst: u32,
    refill_per_sec: u32,
    buckets: HashMap<PeerId, TokenBucket>,
    /// Insertion order (oldest first) so we can evict FIFO without scanning.
    order: std::collections::VecDeque<PeerId>,
}

impl Default for ViewJumpThrottle {
    fn default() -> Self {
        Self::new(DEFAULT_BURST, DEFAULT_REFILL_PER_SEC)
    }
}

impl ViewJumpThrottle {
    pub(crate) fn new(burst: u32, refill_per_sec: u32) -> Self {
        Self {
            burst,
            refill_per_sec,
            buckets: HashMap::with_capacity(MAX_TRACKED_PEERS),
            order: std::collections::VecDeque::with_capacity(MAX_TRACKED_PEERS),
        }
    }

    /// Returns `true` if `peer` is allowed to perform a view-jump-triggering
    /// message right now. Internally accounts the consumption.
    pub(crate) fn try_consume(&mut self, peer: PeerId) -> bool {
        let now = Instant::now();
        let burst = self.burst;
        let refill = self.refill_per_sec;
        if let Some(bucket) = self.buckets.get_mut(&peer) {
            return bucket.try_consume(burst, refill, now);
        }
        // Evict oldest if at capacity before inserting.
        if self.buckets.len() >= MAX_TRACKED_PEERS
            && let Some(oldest) = self.order.pop_front()
        {
            self.buckets.remove(&oldest);
        }
        let mut bucket = TokenBucket::new(self.burst, now);
        let allowed = bucket.try_consume(self.burst, self.refill_per_sec, now);
        self.buckets.insert(peer, bucket);
        self.order.push_back(peer);
        allowed
    }

    /// Drops state for a disconnected peer to keep the map small.
    pub(crate) fn forget(&mut self, peer: &PeerId) {
        if self.buckets.remove(peer).is_some() {
            self.order.retain(|p| p != peer);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn fresh_peer(seed: u8) -> PeerId {
        // Build a deterministic PeerId via libp2p identity (re-exported by n42-network).
        use n42_network::libp2p_identity::Keypair;
        let mut bytes = [0u8; 32];
        bytes[0] = seed;
        let kp = Keypair::ed25519_from_bytes(bytes).expect("test keypair");
        kp.public().to_peer_id()
    }

    #[test]
    fn burst_then_throttle() {
        let mut t = ViewJumpThrottle::new(4, 4);
        let p = fresh_peer(1);
        // First 4 succeed, 5th fails (no time passed for refill).
        for _ in 0..4 {
            assert!(t.try_consume(p));
        }
        assert!(!t.try_consume(p));
    }

    #[test]
    fn refill_after_wait() {
        // Use real time: 4 tokens, 8/sec refill → 250ms wall sleep grants ~2 tokens.
        let mut t = ViewJumpThrottle::new(4, 8);
        let p = fresh_peer(2);
        for _ in 0..4 {
            assert!(t.try_consume(p));
        }
        assert!(!t.try_consume(p));
        std::thread::sleep(Duration::from_millis(300));
        assert!(t.try_consume(p));
        assert!(t.try_consume(p));
    }

    #[test]
    fn separate_peers_have_independent_buckets() {
        let mut t = ViewJumpThrottle::new(2, 2);
        let p1 = fresh_peer(3);
        let p2 = fresh_peer(4);
        assert!(t.try_consume(p1));
        assert!(t.try_consume(p1));
        assert!(!t.try_consume(p1));
        // p2 unaffected
        assert!(t.try_consume(p2));
        assert!(t.try_consume(p2));
        assert!(!t.try_consume(p2));
    }

    #[test]
    fn forget_removes_state() {
        let mut t = ViewJumpThrottle::new(1, 1);
        let p = fresh_peer(5);
        assert!(t.try_consume(p));
        assert!(!t.try_consume(p));
        t.forget(&p);
        // Fresh bucket → first call succeeds again.
        assert!(t.try_consume(p));
    }
}
