use alloy_primitives::B256;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// Connection quality tier for a mobile phone.
///
/// Tiers are computed automatically based on RTT and timeout history.
/// Currently used for observability (metrics). Future phases may use
/// tiers to prioritize message delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PhoneTier {
    /// Low latency (< 2s avg RTT), no consecutive timeouts.
    Fast = 0,
    /// Intermediate quality.
    Normal = 1,
    /// High latency (> 5s avg RTT) or 3+ consecutive timeouts.
    Slow = 2,
}

impl PhoneTier {
    /// Returns a string label suitable for metrics tags.
    pub fn as_str(&self) -> &'static str {
        match self {
            PhoneTier::Fast => "fast",
            PhoneTier::Normal => "normal",
            PhoneTier::Slow => "slow",
        }
    }

    /// Converts from u64 (stored in AtomicU64).
    fn from_u64(v: u64) -> Self {
        match v {
            0 => PhoneTier::Fast,
            1 => PhoneTier::Normal,
            _ => PhoneTier::Slow,
        }
    }
}

/// Represents an active QUIC session with a mobile verifier.
///
/// Each connected phone maintains a session on the IDC node, tracking
/// the phone's identity, connection state, and cached bytecode inventory.
///
/// P2 optimization: `packets_sent` and `receipts_received` use `AtomicU64`
/// so they can be updated with only a read lock on the sessions HashMap,
/// reducing write lock contention under high concurrency (2500+ phones).
pub struct MobileSession {
    /// Unique session identifier (derived from QUIC connection ID).
    pub session_id: u64,
    /// BLS12-381 public key of the mobile verifier.
    pub verifier_pubkey: [u8; 48],
    /// When the phone connected.
    pub connected_at: Instant,
    /// Last activity timestamp as millis since `connected_at`.
    /// Uses AtomicU64 to allow updates with only a read lock.
    last_active_offset_ms: AtomicU64,
    /// Set of code hashes the phone has cached.
    /// Used to determine which bytecodes to include in verification packets.
    pub cached_code_hashes: HashSet<B256>,
    /// Number of verification packets sent to this phone.
    pub packets_sent: AtomicU64,
    /// Number of verification receipts received from this phone.
    pub receipts_received: AtomicU64,

    // --- Connection tiering fields (Phase 2 Feature B) ---
    /// Current connection quality tier (PhoneTier as u64).
    tier: AtomicU64,
    /// Total number of send timeouts.
    timeout_count: AtomicU64,
    /// Consecutive send timeouts (reset on success).
    consecutive_timeouts: AtomicU64,
    /// Cumulative RTT in milliseconds (for computing average).
    total_rtt_ms: AtomicU64,
    /// Number of RTT samples collected.
    rtt_sample_count: AtomicU64,
}

impl std::fmt::Debug for MobileSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MobileSession")
            .field("session_id", &self.session_id)
            .field("connected_at", &self.connected_at)
            .field("cached_code_hashes", &self.cached_code_hashes.len())
            .field("packets_sent", &self.packets_sent.load(Ordering::Relaxed))
            .field("receipts_received", &self.receipts_received.load(Ordering::Relaxed))
            .field("tier", &self.tier())
            .finish()
    }
}

impl MobileSession {
    /// Creates a new mobile session.
    pub fn new(session_id: u64, verifier_pubkey: [u8; 48]) -> Self {
        Self {
            session_id,
            verifier_pubkey,
            connected_at: Instant::now(),
            last_active_offset_ms: AtomicU64::new(0),
            cached_code_hashes: HashSet::new(),
            packets_sent: AtomicU64::new(0),
            receipts_received: AtomicU64::new(0),
            tier: AtomicU64::new(PhoneTier::Fast as u64),
            timeout_count: AtomicU64::new(0),
            consecutive_timeouts: AtomicU64::new(0),
            total_rtt_ms: AtomicU64::new(0),
            rtt_sample_count: AtomicU64::new(0),
        }
    }

    /// Returns the current connection quality tier.
    pub fn tier(&self) -> PhoneTier {
        PhoneTier::from_u64(self.tier.load(Ordering::Relaxed))
    }

    /// Records a successful send: resets consecutive timeouts, increments packet counter.
    pub fn record_send_success(&self) {
        self.consecutive_timeouts.store(0, Ordering::Relaxed);
        self.record_packet_sent();
        self.recompute_tier();
    }

    /// Records a send timeout: increments timeout counters.
    pub fn record_send_timeout(&self) {
        self.timeout_count.fetch_add(1, Ordering::Relaxed);
        self.consecutive_timeouts.fetch_add(1, Ordering::Relaxed);
        self.recompute_tier();
    }

    /// Records an RTT sample (in milliseconds) and recomputes tier.
    pub fn record_rtt(&self, rtt_ms: u64) {
        self.total_rtt_ms.fetch_add(rtt_ms, Ordering::Relaxed);
        self.rtt_sample_count.fetch_add(1, Ordering::Relaxed);
        self.recompute_tier();
    }

    /// Returns the average RTT in milliseconds, or None if no samples.
    pub fn avg_rtt_ms(&self) -> Option<u64> {
        let count = self.rtt_sample_count.load(Ordering::Relaxed);
        if count == 0 {
            return None;
        }
        Some(self.total_rtt_ms.load(Ordering::Relaxed) / count)
    }

    /// Recomputes the tier based on consecutive timeouts and average RTT.
    ///
    /// - **Fast**: consecutive_timeouts == 0 && avg_rtt < 2000ms (or no RTT data)
    /// - **Slow**: consecutive_timeouts >= 3 || avg_rtt > 5000ms
    /// - **Normal**: everything else
    fn recompute_tier(&self) {
        let consec = self.consecutive_timeouts.load(Ordering::Relaxed);
        let avg = self.avg_rtt_ms();

        let new_tier = if consec >= 3 || avg.map_or(false, |r| r > 5000) {
            PhoneTier::Slow
        } else if consec == 0 && avg.map_or(true, |r| r < 2000) {
            PhoneTier::Fast
        } else {
            PhoneTier::Normal
        };

        self.tier.store(new_tier as u64, Ordering::Relaxed);
    }

    /// Marks the session as recently active (lock-free).
    pub fn touch(&self) {
        let offset = self.connected_at.elapsed().as_millis() as u64;
        self.last_active_offset_ms.store(offset, Ordering::Relaxed);
    }

    /// Increments the receipt counter and touches the session (lock-free).
    pub fn record_receipt(&self) {
        self.receipts_received.fetch_add(1, Ordering::Relaxed);
        self.touch();
    }

    /// Increments the packet counter and touches the session (lock-free).
    pub fn record_packet_sent(&self) {
        self.packets_sent.fetch_add(1, Ordering::Relaxed);
        self.touch();
    }

    /// Returns the session duration.
    pub fn duration(&self) -> std::time::Duration {
        self.connected_at.elapsed()
    }

    /// Returns time since last activity.
    pub fn idle_duration(&self) -> std::time::Duration {
        let offset_ms = self.last_active_offset_ms.load(Ordering::Relaxed);
        let total_ms = self.connected_at.elapsed().as_millis() as u64;
        std::time::Duration::from_millis(total_ms.saturating_sub(offset_ms))
    }

    /// Updates the phone's cached code hash set.
    pub fn update_cache_inventory(&mut self, code_hashes: HashSet<B256>) {
        self.cached_code_hashes = code_hashes;
    }

    /// Checks if the phone has a specific bytecode cached.
    pub fn has_cached(&self, code_hash: &B256) -> bool {
        self.cached_code_hashes.contains(code_hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_creation() {
        let pubkey = [0xAA; 48];
        let session = MobileSession::new(42, pubkey);
        assert_eq!(session.session_id, 42);
        assert_eq!(session.verifier_pubkey, pubkey);
        assert_eq!(session.packets_sent.load(Ordering::Relaxed), 0);
        assert_eq!(session.receipts_received.load(Ordering::Relaxed), 0);
        assert!(session.cached_code_hashes.is_empty());
    }

    #[test]
    fn test_record_receipt_and_packet() {
        let session = MobileSession::new(1, [0u8; 48]);

        session.record_receipt();
        session.record_receipt();
        assert_eq!(session.receipts_received.load(Ordering::Relaxed), 2);

        session.record_packet_sent();
        assert_eq!(session.packets_sent.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_idle_duration() {
        let session = MobileSession::new(1, [0u8; 48]);
        // Initially idle_duration ≈ duration since connected
        let idle = session.idle_duration();
        assert!(idle.as_millis() < 100, "fresh session should have near-zero idle");

        // Touch should reduce idle time
        std::thread::sleep(std::time::Duration::from_millis(10));
        session.touch();
        let idle_after_touch = session.idle_duration();
        assert!(idle_after_touch.as_millis() < 50);
    }

    #[test]
    fn test_cache_inventory() {
        let mut session = MobileSession::new(1, [0u8; 48]);
        let hash1 = B256::repeat_byte(0x01);
        let hash2 = B256::repeat_byte(0x02);
        let hash3 = B256::repeat_byte(0x03);

        assert!(!session.has_cached(&hash1));

        let mut cache = HashSet::new();
        cache.insert(hash1);
        cache.insert(hash2);
        session.update_cache_inventory(cache);

        assert!(session.has_cached(&hash1));
        assert!(session.has_cached(&hash2));
        assert!(!session.has_cached(&hash3));
    }

    #[test]
    fn test_concurrent_atomic_counters() {
        use std::sync::Arc;

        let session = Arc::new(MobileSession::new(1, [0u8; 48]));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let s = session.clone();
                std::thread::spawn(move || {
                    for _ in 0..1000 {
                        s.record_receipt();
                        s.record_packet_sent();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(session.receipts_received.load(Ordering::Relaxed), 8000);
        assert_eq!(session.packets_sent.load(Ordering::Relaxed), 8000);
    }

    #[test]
    fn test_duration_monotonic() {
        let session = MobileSession::new(1, [0u8; 48]);
        let d1 = session.duration();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let d2 = session.duration();
        assert!(d2 >= d1, "duration should be monotonically increasing");
    }

    #[test]
    fn test_debug_format() {
        let session = MobileSession::new(99, [0u8; 48]);
        session.record_receipt();
        session.record_packet_sent();
        session.record_packet_sent();
        let debug_str = format!("{:?}", session);
        assert!(debug_str.contains("session_id: 99"));
        assert!(debug_str.contains("packets_sent: 2"));
        assert!(debug_str.contains("receipts_received: 1"));
    }

    // --- Feature B: Connection Tiering tests ---

    #[test]
    fn test_phone_tier_default() {
        let session = MobileSession::new(1, [0xAA; 48]);
        assert_eq!(session.tier(), PhoneTier::Fast);
    }

    #[test]
    fn test_tier_degrades_on_timeouts() {
        let session = MobileSession::new(1, [0u8; 48]);
        assert_eq!(session.tier(), PhoneTier::Fast);

        session.record_send_timeout();
        // 1 consecutive timeout: Normal (not 0, not >= 3)
        assert_eq!(session.tier(), PhoneTier::Normal);

        session.record_send_timeout();
        assert_eq!(session.tier(), PhoneTier::Normal);

        session.record_send_timeout();
        // 3 consecutive timeouts: Slow
        assert_eq!(session.tier(), PhoneTier::Slow);
    }

    #[test]
    fn test_tier_recovers_on_success() {
        let session = MobileSession::new(1, [0u8; 48]);

        // Drive to Slow
        for _ in 0..3 {
            session.record_send_timeout();
        }
        assert_eq!(session.tier(), PhoneTier::Slow);

        // Success resets consecutive_timeouts → Fast (no RTT data yet)
        session.record_send_success();
        assert_eq!(session.tier(), PhoneTier::Fast);
    }

    #[test]
    fn test_record_rtt_average() {
        let session = MobileSession::new(1, [0u8; 48]);
        assert_eq!(session.avg_rtt_ms(), None);

        session.record_rtt(1000);
        assert_eq!(session.avg_rtt_ms(), Some(1000));

        session.record_rtt(3000);
        // avg = (1000 + 3000) / 2 = 2000
        assert_eq!(session.avg_rtt_ms(), Some(2000));
    }

    #[test]
    fn test_tier_slow_on_high_rtt() {
        let session = MobileSession::new(1, [0u8; 48]);

        // Record high RTT samples: avg > 5000ms → Slow
        session.record_rtt(6000);
        session.record_rtt(7000);
        assert_eq!(session.tier(), PhoneTier::Slow);
    }

    #[test]
    fn test_tier_normal_on_moderate_rtt() {
        let session = MobileSession::new(1, [0u8; 48]);

        // avg_rtt = 3000ms → between 2000 and 5000 → Normal (if consecutive_timeouts == 0)
        session.record_rtt(3000);
        assert_eq!(session.tier(), PhoneTier::Normal);
    }

    #[test]
    fn test_tier_from_u64_roundtrip() {
        assert_eq!(PhoneTier::from_u64(PhoneTier::Fast as u64), PhoneTier::Fast);
        assert_eq!(PhoneTier::from_u64(PhoneTier::Normal as u64), PhoneTier::Normal);
        assert_eq!(PhoneTier::from_u64(PhoneTier::Slow as u64), PhoneTier::Slow);
        // Unknown values map to Slow (safe default)
        assert_eq!(PhoneTier::from_u64(99), PhoneTier::Slow);
    }

    #[test]
    fn test_phone_tier_as_str() {
        assert_eq!(PhoneTier::Fast.as_str(), "fast");
        assert_eq!(PhoneTier::Normal.as_str(), "normal");
        assert_eq!(PhoneTier::Slow.as_str(), "slow");
    }
}
