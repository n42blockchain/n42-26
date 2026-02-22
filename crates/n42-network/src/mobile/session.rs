use alloy_primitives::B256;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// Connection quality tier for a mobile phone.
///
/// Computed automatically from RTT history and timeout count.
/// Used for observability and to skip slow phones for optional messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PhoneTier {
    /// Low latency (avg RTT < 2s, no consecutive timeouts).
    Fast = 0,
    /// Intermediate quality.
    Normal = 1,
    /// High latency (avg RTT > 5s) or 3+ consecutive timeouts.
    Slow = 2,
}

impl PhoneTier {
    /// Returns a string label for metrics tags.
    pub fn as_str(&self) -> &'static str {
        match self {
            PhoneTier::Fast => "fast",
            PhoneTier::Normal => "normal",
            PhoneTier::Slow => "slow",
        }
    }

    fn from_u64(v: u64) -> Self {
        match v {
            0 => PhoneTier::Fast,
            1 => PhoneTier::Normal,
            _ => PhoneTier::Slow,
        }
    }
}

/// EWMA alpha: weight given to new sample (alpha = 3/10 = 0.3).
const EWMA_ALPHA_NUM: u64 = 3;
const EWMA_ALPHA_DEN: u64 = 10;

/// Represents an active QUIC session with a mobile verifier.
///
/// Uses `AtomicU64` for hot counters so they can be updated with only a
/// read lock on the sessions map, reducing write contention under high
/// concurrency (2500+ phones per shard).
pub struct MobileSession {
    pub session_id: u64,
    pub verifier_pubkey: [u8; 48],
    pub connected_at: Instant,
    /// Milliseconds elapsed since `connected_at` at last activity (lock-free).
    last_active_offset_ms: AtomicU64,
    pub cached_code_hashes: HashSet<B256>,
    pub packets_sent: AtomicU64,
    pub receipts_received: AtomicU64,
    tier: AtomicU64,
    timeout_count: AtomicU64,
    consecutive_timeouts: AtomicU64,
    /// EWMA RTT in milliseconds (0 = no samples yet).
    ewma_rtt_ms: AtomicU64,
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
            ewma_rtt_ms: AtomicU64::new(0),
        }
    }

    /// Returns the current connection quality tier.
    pub fn tier(&self) -> PhoneTier {
        PhoneTier::from_u64(self.tier.load(Ordering::Relaxed))
    }

    /// Records a successful send: resets consecutive timeouts and increments packet counter.
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

    /// Records an RTT sample (ms) using EWMA (alpha = 0.3) and recomputes tier.
    ///
    /// Uses a CAS loop for lock-free atomic updates.
    pub fn record_rtt(&self, rtt_ms: u64) {
        loop {
            let current = self.ewma_rtt_ms.load(Ordering::Relaxed);
            let new_val = if current == 0 {
                rtt_ms
            } else {
                (EWMA_ALPHA_NUM * rtt_ms + (EWMA_ALPHA_DEN - EWMA_ALPHA_NUM) * current)
                    / EWMA_ALPHA_DEN
            };
            if self
                .ewma_rtt_ms
                .compare_exchange_weak(current, new_val, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                break;
            }
        }
        self.recompute_tier();
    }

    /// Returns the EWMA RTT in milliseconds, or `None` if no samples yet.
    pub fn avg_rtt_ms(&self) -> Option<u64> {
        let v = self.ewma_rtt_ms.load(Ordering::Relaxed);
        if v == 0 { None } else { Some(v) }
    }

    /// Recomputes tier from consecutive timeouts and average RTT.
    ///
    /// - Fast: consecutive_timeouts == 0 && avg_rtt < 2000ms (or no data)
    /// - Slow: consecutive_timeouts >= 3 || avg_rtt > 5000ms
    /// - Normal: everything else
    fn recompute_tier(&self) {
        let consec = self.consecutive_timeouts.load(Ordering::Relaxed);
        let avg = self.avg_rtt_ms();

        let new_tier = if consec >= 3 || avg.is_some_and(|r| r > 5000) {
            PhoneTier::Slow
        } else if consec == 0 && avg.is_none_or(|r| r < 2000) {
            PhoneTier::Fast
        } else {
            PhoneTier::Normal
        };

        self.tier.store(new_tier as u64, Ordering::Relaxed);
    }

    /// Records activity by updating the last-active timestamp (lock-free).
    pub fn touch(&self) {
        let offset = self.connected_at.elapsed().as_millis() as u64;
        self.last_active_offset_ms.store(offset, Ordering::Relaxed);
    }

    /// Increments the receipt counter and touches the session.
    pub fn record_receipt(&self) {
        self.receipts_received.fetch_add(1, Ordering::Relaxed);
        self.touch();
    }

    /// Increments the packet counter and touches the session.
    pub fn record_packet_sent(&self) {
        self.packets_sent.fetch_add(1, Ordering::Relaxed);
        self.touch();
    }

    /// Returns the total session duration.
    pub fn duration(&self) -> std::time::Duration {
        self.connected_at.elapsed()
    }

    /// Returns time since last activity.
    pub fn idle_duration(&self) -> std::time::Duration {
        let offset_ms = self.last_active_offset_ms.load(Ordering::Relaxed);
        let total_ms = self.connected_at.elapsed().as_millis() as u64;
        std::time::Duration::from_millis(total_ms.saturating_sub(offset_ms))
    }

    pub fn update_cache_inventory(&mut self, code_hashes: HashSet<B256>) {
        self.cached_code_hashes = code_hashes;
    }

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
        assert!(session.idle_duration().as_millis() < 100);
        std::thread::sleep(std::time::Duration::from_millis(10));
        session.touch();
        assert!(session.idle_duration().as_millis() < 50);
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
        assert!(d2 >= d1);
    }

    #[test]
    fn test_debug_format() {
        let session = MobileSession::new(99, [0u8; 48]);
        session.record_receipt();
        session.record_packet_sent();
        session.record_packet_sent();
        let s = format!("{session:?}");
        assert!(s.contains("session_id: 99"));
        assert!(s.contains("packets_sent: 2"));
        assert!(s.contains("receipts_received: 1"));
    }

    #[test]
    fn test_phone_tier_default() {
        assert_eq!(MobileSession::new(1, [0xAA; 48]).tier(), PhoneTier::Fast);
    }

    #[test]
    fn test_tier_degrades_on_timeouts() {
        let session = MobileSession::new(1, [0u8; 48]);
        assert_eq!(session.tier(), PhoneTier::Fast);
        session.record_send_timeout();
        assert_eq!(session.tier(), PhoneTier::Normal);
        session.record_send_timeout();
        assert_eq!(session.tier(), PhoneTier::Normal);
        session.record_send_timeout();
        assert_eq!(session.tier(), PhoneTier::Slow);
    }

    #[test]
    fn test_tier_recovers_on_success() {
        let session = MobileSession::new(1, [0u8; 48]);
        for _ in 0..3 {
            session.record_send_timeout();
        }
        assert_eq!(session.tier(), PhoneTier::Slow);
        session.record_send_success();
        assert_eq!(session.tier(), PhoneTier::Fast);
    }

    #[test]
    fn test_ewma_rtt_first_sample() {
        let session = MobileSession::new(1, [0u8; 48]);
        assert_eq!(session.avg_rtt_ms(), None);
        session.record_rtt(1000);
        assert_eq!(session.avg_rtt_ms(), Some(1000));
    }

    #[test]
    fn test_ewma_rtt_converges() {
        let session = MobileSession::new(1, [0u8; 48]);
        session.record_rtt(100);
        for _ in 0..9 {
            session.record_rtt(100);
        }
        assert!(session.avg_rtt_ms().unwrap() <= 100);
        session.record_rtt(5000);
        assert!(session.avg_rtt_ms().unwrap() > 1000, "EWMA should react to spike");
    }

    #[test]
    fn test_ewma_rtt_decay() {
        let session = MobileSession::new(1, [0u8; 48]);
        session.record_rtt(5000);
        for _ in 0..15 {
            session.record_rtt(100);
        }
        assert!(session.avg_rtt_ms().unwrap() < 2000, "EWMA should decay below 2000ms");
    }

    #[test]
    fn test_ewma_rtt_second_sample() {
        let session = MobileSession::new(1, [0u8; 48]);
        session.record_rtt(1000);
        session.record_rtt(3000);
        // EWMA: 0.3 * 3000 + 0.7 * 1000 = 1600
        assert_eq!(session.avg_rtt_ms(), Some(1600));
    }

    #[test]
    fn test_tier_slow_on_high_rtt() {
        let session = MobileSession::new(1, [0u8; 48]);
        session.record_rtt(6000);
        session.record_rtt(7000);
        assert_eq!(session.tier(), PhoneTier::Slow);
    }

    #[test]
    fn test_tier_normal_on_moderate_rtt() {
        let session = MobileSession::new(1, [0u8; 48]);
        session.record_rtt(3000);
        assert_eq!(session.tier(), PhoneTier::Normal);
    }

    #[test]
    fn test_tier_from_u64_roundtrip() {
        assert_eq!(PhoneTier::from_u64(PhoneTier::Fast as u64), PhoneTier::Fast);
        assert_eq!(PhoneTier::from_u64(PhoneTier::Normal as u64), PhoneTier::Normal);
        assert_eq!(PhoneTier::from_u64(PhoneTier::Slow as u64), PhoneTier::Slow);
        assert_eq!(PhoneTier::from_u64(99), PhoneTier::Slow);
    }

    #[test]
    fn test_phone_tier_as_str() {
        assert_eq!(PhoneTier::Fast.as_str(), "fast");
        assert_eq!(PhoneTier::Normal.as_str(), "normal");
        assert_eq!(PhoneTier::Slow.as_str(), "slow");
    }

    #[test]
    fn test_tier_rapid_transitions() {
        let session = MobileSession::new(1, [0u8; 48]);
        session.record_send_timeout();
        assert_eq!(session.tier(), PhoneTier::Normal);
        session.record_send_timeout();
        session.record_send_timeout();
        assert_eq!(session.tier(), PhoneTier::Slow);
        session.record_rtt(3000);
        session.record_send_success();
        assert_eq!(session.tier(), PhoneTier::Normal);
        for _ in 0..8 {
            session.record_rtt(500);
        }
        let rtt = session.avg_rtt_ms().unwrap();
        assert!(rtt < 2000, "EWMA should be below 2000ms, got {rtt}");
        assert_eq!(session.tier(), PhoneTier::Fast);
    }

    #[test]
    fn test_tier_fast_to_slow_via_high_rtt() {
        let session = MobileSession::new(1, [0u8; 48]);
        session.record_rtt(10000);
        assert_eq!(session.tier(), PhoneTier::Slow);
    }
}
