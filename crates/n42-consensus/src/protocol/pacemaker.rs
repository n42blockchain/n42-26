use n42_primitives::consensus::ViewNumber;
use std::time::Duration;
use tokio::time::{Instant, Sleep};

/// Active Pacemaker for the HotStuff-2 consensus protocol.
///
/// Manages view timeouts with exponential backoff (Jolteon-style):
/// `timeout(c) = min(base_eff * 2^floor(c / g), max_timeout)`.
///
/// `c` is the consecutive-timeout count held by `RoundState`: it grows on each
/// local view timeout and resets on a commit QC (Decide) and on a QC-based view
/// jump, never on a TC (formed locally or carried in a NewView).
///
/// `g` is the backoff group. Profile 1 (AM33, agreed by gov5, n42-rs and
/// n42-26 for v1.2) sets `g = min(f + 1, 3)` with `f = floor((n - 1) / 3)` and
/// `n` the size of the validator set the engine holds when it starts the view
/// timer ([`Self::set_committee_size`]), so a run of absent leaders costs the
/// base timeout each until `g` TCs in a row point at a real timing problem.
/// Without a committee size (`n = 0`, the other product lines) `g = 1`, the
/// rule before AM33.
///
/// Adaptive mode: when `observe_commit_latency` is called with real measurements,
/// the effective base timeout is adjusted to `max(base_timeout, p95_observed * 2)`.
/// This prevents premature timeouts on high-latency global networks while keeping
/// fast timeouts on local/regional deployments.
///
/// Ensures liveness: if the current leader is faulty or the network is slow,
/// validators will eventually time out and trigger a view change.
#[derive(Debug)]
pub struct Pacemaker {
    base_timeout: Duration,
    max_timeout: Duration,
    deadline: Instant,
    /// Adaptive timeout: ring buffer of recent commit latencies.
    /// Used to compute effective base timeout as max(configured, 2 × p95).
    recent_latencies: Vec<Duration>,
    /// Cursor for circular buffer insertion.
    latency_cursor: usize,
    /// Duration the current deadline was armed with by the last
    /// [`Self::reset_for_view`] (the base timeout before the first one).
    armed_timeout: Duration,
    /// Validator-set size the AM33 backoff group is derived from; 0 = none
    /// (group 1, the pre-AM33 rule).
    committee_size: usize,
}

impl Pacemaker {
    /// Number of recent latency samples to keep for adaptive timeout.
    const LATENCY_WINDOW: usize = 20;
    /// Maximum exponent for backoff calculation to avoid overflow.
    const MAX_BACKOFF_EXPONENT: u32 = 63;

    pub fn new(base_timeout_ms: u64, max_timeout_ms: u64) -> Self {
        let base_timeout = Duration::from_millis(base_timeout_ms);
        let max_timeout = Duration::from_millis(max_timeout_ms);
        Self {
            base_timeout,
            max_timeout,
            deadline: Instant::now() + base_timeout,
            recent_latencies: Vec::with_capacity(Self::LATENCY_WINDOW),
            latency_cursor: 0,
            armed_timeout: base_timeout,
            committee_size: 0,
        }
    }

    /// A pacemaker whose backoff group is derived from a validator set of
    /// `committee_size` (see [`Self::set_committee_size`]).
    pub fn with_committee_size(
        base_timeout_ms: u64,
        max_timeout_ms: u64,
        committee_size: usize,
    ) -> Self {
        let mut pacemaker = Self::new(base_timeout_ms, max_timeout_ms);
        pacemaker.committee_size = committee_size;
        pacemaker
    }

    /// AM33's backoff group for a validator set of `n`: `min(f + 1, 3)` with
    /// `f = floor((n - 1) / 3)`; 1 when `n` is 0 (no committee size).
    /// gov5's `hotstuff.BackoffGroupSize`.
    pub const fn backoff_group_size(n: usize) -> u32 {
        if n == 0 {
            return 1;
        }
        let f = (n - 1) / 3;
        if f + 1 < 3 { (f + 1) as u32 } else { 3 }
    }

    /// Sets the validator-set size the backoff group is derived from (gov5's
    /// `SetCommitteeSize`); 0 restores group 1. The engine sets it each time
    /// it starts a view timer: the set size on Profile 1, 0 elsewhere.
    pub fn set_committee_size(&mut self, n: usize) {
        self.committee_size = n;
    }

    /// The backoff group in force; see [`Self::backoff_group_size`].
    pub const fn backoff_group(&self) -> u32 {
        Self::backoff_group_size(self.committee_size)
    }

    /// Records an observed commit latency (view start → BlockCommitted).
    ///
    /// After enough samples, the effective base timeout adapts:
    /// `effective_base = max(configured_base, 2 × p95_latency)`
    ///
    /// This prevents premature timeouts on global networks where consensus
    /// naturally takes 600-800ms, while keeping fast timeouts locally.
    pub fn observe_commit_latency(&mut self, latency: Duration) {
        if self.recent_latencies.len() < Self::LATENCY_WINDOW {
            self.recent_latencies.push(latency);
        } else {
            self.recent_latencies[self.latency_cursor % Self::LATENCY_WINDOW] = latency;
        }
        self.latency_cursor += 1;
    }

    /// Returns the effective base timeout, considering adaptive measurements.
    fn effective_base_timeout(&self) -> Duration {
        if self.recent_latencies.len() < 5 {
            return self.base_timeout;
        }
        // Compute p95 of recent latencies.
        let mut sorted: Vec<u64> = self
            .recent_latencies
            .iter()
            .map(|d| d.as_millis() as u64)
            .collect();
        sorted.sort_unstable();
        let p95_idx = (sorted.len() * 95 / 100).min(sorted.len().saturating_sub(1));
        let p95_ms = sorted[p95_idx];
        // Effective base = max(configured, 2 × p95), capped at max_timeout.
        let adaptive_ms = p95_ms.saturating_mul(2);
        let effective = self.base_timeout.as_millis() as u64;
        Duration::from_millis(
            effective
                .max(adaptive_ms)
                .min(self.max_timeout.as_millis() as u64),
        )
    }

    /// Returns the timeout duration for the given number of consecutive timeouts.
    ///
    /// `effective_base * 2^floor(consecutive_timeouts / g)`, capped at
    /// `max_timeout`; `g` is [`Self::backoff_group`] (1 without a committee
    /// size). The group divides only the exponent: the adaptive base applies
    /// unchanged.
    pub fn timeout_duration(&self, consecutive_timeouts: u32) -> Duration {
        let base = self.effective_base_timeout();
        let exp = (consecutive_timeouts / self.backoff_group()).min(Self::MAX_BACKOFF_EXPONENT);
        let multiplier = 1u64.checked_shl(exp).unwrap_or(u64::MAX);
        let timeout_ms = (base.as_millis() as u64).saturating_mul(multiplier);
        Duration::from_millis(timeout_ms.min(self.max_timeout.as_millis() as u64))
    }

    /// Resets the timer for a new view with the appropriate backoff duration.
    pub fn reset_for_view(&mut self, view: ViewNumber, consecutive_timeouts: u32) {
        let duration = self.timeout_duration(consecutive_timeouts);
        self.deadline = Instant::now() + duration;
        self.armed_timeout = duration;
        tracing::debug!(
            view,
            timeout_ms = duration.as_millis() as u64,
            consecutive_timeouts,
            backoff_group = self.backoff_group(),
            "pacemaker timer reset"
        );
    }

    /// The duration the current view timer was armed with (deterministic,
    /// unlike [`Self::remaining`]).
    pub fn armed_timeout(&self) -> Duration {
        self.armed_timeout
    }

    /// Returns a `Sleep` future that completes when the current view times out.
    pub fn timeout_sleep(&self) -> Sleep {
        tokio::time::sleep_until(self.deadline)
    }

    /// Returns true if the current view has timed out.
    pub fn is_timed_out(&self) -> bool {
        Instant::now() >= self.deadline
    }

    /// Returns the remaining time until timeout.
    pub fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    /// Extends the current deadline by the given duration.
    ///
    /// Used at startup to allow extra time for GossipSub mesh formation
    /// before the first proposal.
    pub fn extend_deadline(&mut self, extra: Duration) {
        self.deadline += extra;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_timeout_duration_base() {
        let pm = Pacemaker::new(1000, 60000);
        assert_eq!(pm.timeout_duration(0), Duration::from_millis(1000));
    }

    #[test]
    fn test_timeout_duration_backoff() {
        let pm = Pacemaker::new(1000, 60000);
        assert_eq!(pm.timeout_duration(1), Duration::from_millis(2000));
        assert_eq!(pm.timeout_duration(2), Duration::from_millis(4000));
        assert_eq!(pm.timeout_duration(3), Duration::from_millis(8000));
    }

    #[test]
    fn test_timeout_duration_capped() {
        let pm = Pacemaker::new(1000, 30000);
        assert_eq!(pm.timeout_duration(10), Duration::from_millis(30000));
        assert_eq!(pm.timeout_duration(5), Duration::from_millis(30000));
        assert_eq!(pm.timeout_duration(4), Duration::from_millis(16000));
    }

    #[test]
    fn test_is_timed_out_initially() {
        let pm = Pacemaker::new(5000, 60000);
        assert!(!pm.is_timed_out());
    }

    #[test]
    fn test_remaining_positive_initially() {
        let pm = Pacemaker::new(5000, 60000);
        let remaining = pm.remaining();
        assert!(remaining > Duration::ZERO);
        assert!(remaining <= Duration::from_millis(5000));
    }

    #[test]
    fn test_reset_for_view() {
        let mut pm = Pacemaker::new(1000, 60000);

        pm.reset_for_view(2, 0);
        assert!(!pm.is_timed_out());
        assert!(pm.remaining() <= Duration::from_millis(1000));

        pm.reset_for_view(3, 2);
        assert!(!pm.is_timed_out());
        assert!(pm.remaining() <= Duration::from_millis(4000));
    }

    #[test]
    fn test_adaptive_timeout_no_samples() {
        // Without observations, uses configured base timeout.
        let pm = Pacemaker::new(4000, 60000);
        assert_eq!(pm.timeout_duration(0), Duration::from_millis(4000));
    }

    #[test]
    fn test_adaptive_timeout_increases_with_high_latency() {
        // Simulate global network: consensus takes ~700ms.
        let mut pm = Pacemaker::new(4000, 60000);
        for _ in 0..10 {
            pm.observe_commit_latency(Duration::from_millis(700));
        }
        // p95 = 700ms, adaptive = 2 × 700 = 1400ms.
        // But configured base = 4000ms > 1400ms, so stays at 4000ms.
        assert_eq!(pm.timeout_duration(0), Duration::from_millis(4000));
    }

    #[test]
    fn test_adaptive_timeout_adapts_to_slow_network() {
        // Simulate very slow network: consensus takes ~3000ms.
        // base_timeout = 4000ms, but 2 × p95 = 6000ms > 4000ms → adapts.
        let mut pm = Pacemaker::new(4000, 60000);
        for _ in 0..10 {
            pm.observe_commit_latency(Duration::from_millis(3000));
        }
        // p95 = 3000ms, adaptive = 2 × 3000 = 6000ms > 4000ms.
        assert_eq!(pm.timeout_duration(0), Duration::from_millis(6000));
    }

    #[test]
    fn test_adaptive_timeout_capped_by_max() {
        // Even with extreme latency, timeout is capped at max_timeout.
        let mut pm = Pacemaker::new(4000, 10000);
        for _ in 0..10 {
            pm.observe_commit_latency(Duration::from_millis(8000));
        }
        // 2 × 8000 = 16000, but max = 10000.
        assert_eq!(pm.timeout_duration(0), Duration::from_millis(10000));
    }

    fn by_c(pm: &Pacemaker, upto: u32) -> Vec<u64> {
        (0..=upto)
            .map(|c| pm.timeout_duration(c).as_millis() as u64)
            .collect()
    }

    #[test]
    fn am33_backoff_group_size() {
        // f = floor((n - 1) / 3), g = min(f + 1, 3); no committee size: g = 1.
        for (n, g) in [
            (0, 1),
            (1, 1),
            (2, 1),
            (3, 1),
            (4, 2),
            (5, 2),
            (6, 2),
            (7, 3),
            (10, 3),
            (100, 3),
            (512, 3),
        ] {
            assert_eq!(Pacemaker::backoff_group_size(n), g, "n = {n}");
        }
        assert_eq!(Pacemaker::new(1000, 2000).backoff_group(), 1);
    }

    #[test]
    fn am33_n4_doubles_every_second_timeout() {
        // n = 4: f = 1, g = 2 (gate 5d timeouts, 24 s / 96 s).
        let pm = Pacemaker::with_committee_size(24_000, 96_000, 4);
        assert_eq!(pm.backoff_group(), 2);
        assert_eq!(
            by_c(&pm, 6),
            [24_000, 24_000, 48_000, 48_000, 96_000, 96_000, 96_000]
        );
    }

    #[test]
    fn am33_n7_and_n100_double_every_third_timeout() {
        for n in [7, 100] {
            let pm = Pacemaker::with_committee_size(6_000, 30_000, n);
            assert_eq!(pm.backoff_group(), 3, "n = {n}");
            assert_eq!(
                by_c(&pm, 9),
                [
                    6_000, 6_000, 6_000, 12_000, 12_000, 12_000, 24_000, 24_000, 24_000, 30_000
                ],
                "n = {n}"
            );
        }
    }

    #[test]
    fn am33_capped_at_max_and_saturates() {
        let pm = Pacemaker::with_committee_size(6_000, 30_000, 100);
        for c in [9, 10, 63, 189, 190, u32::MAX] {
            assert_eq!(
                pm.timeout_duration(c),
                Duration::from_millis(30_000),
                "c = {c}"
            );
        }
    }

    #[test]
    fn am33_set_committee_size_switches_the_group() {
        let mut pm = Pacemaker::new(1000, 60_000);
        assert_eq!(pm.timeout_duration(2), Duration::from_millis(4000));
        pm.set_committee_size(7);
        assert_eq!(pm.timeout_duration(2), Duration::from_millis(1000));
        assert_eq!(pm.timeout_duration(3), Duration::from_millis(2000));
        pm.set_committee_size(0);
        assert_eq!(pm.timeout_duration(2), Duration::from_millis(4000));
    }

    #[test]
    fn am33_group_composes_with_the_adaptive_base() {
        // The group divides only the exponent; base_eff = max(base, 2 * p95).
        let mut pm = Pacemaker::with_committee_size(4000, 60_000, 4);
        for _ in 0..10 {
            pm.observe_commit_latency(Duration::from_millis(3000));
        }
        assert_eq!(by_c(&pm, 5), [6000, 6000, 12_000, 12_000, 24_000, 24_000]);
    }
}
