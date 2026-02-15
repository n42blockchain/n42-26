use n42_primitives::consensus::ViewNumber;
use std::time::Duration;
use tokio::time::{Instant, Sleep};

/// Active Pacemaker for the HotStuff-2 consensus protocol.
///
/// Manages view timeouts with exponential backoff (Jolteon-style):
/// `timeout = min(base_timeout * 2^consecutive_timeouts, max_timeout)`
///
/// The pacemaker ensures liveness: if the current leader is faulty or
/// the network is slow, validators will eventually time out and trigger
/// a view change to a new leader.
#[derive(Debug)]
pub struct Pacemaker {
    /// Base timeout duration.
    base_timeout: Duration,
    /// Maximum timeout duration (cap).
    max_timeout: Duration,
    /// Deadline for the current view's timer.
    deadline: Instant,
}

impl Pacemaker {
    /// Creates a new pacemaker with the given timeout parameters.
    pub fn new(base_timeout_ms: u64, max_timeout_ms: u64) -> Self {
        let base_timeout = Duration::from_millis(base_timeout_ms);
        let max_timeout = Duration::from_millis(max_timeout_ms);

        Self {
            base_timeout,
            max_timeout,
            deadline: Instant::now() + base_timeout,
        }
    }

    /// Computes the timeout duration for the given number of consecutive timeouts.
    ///
    /// Uses exponential backoff: `base * 2^consecutive_timeouts`, capped at `max_timeout`.
    pub fn timeout_duration(&self, consecutive_timeouts: u32) -> Duration {
        let multiplier = 1u64.checked_shl(consecutive_timeouts).unwrap_or(u64::MAX);
        let timeout_ms = self
            .base_timeout
            .as_millis()
            .saturating_mul(multiplier as u128);
        let capped = Duration::from_millis(
            timeout_ms.min(self.max_timeout.as_millis()) as u64,
        );
        capped
    }

    /// Resets the timer for a new view.
    /// Called when advancing to a new view (either after commit or view change).
    pub fn reset_for_view(&mut self, _view: ViewNumber, consecutive_timeouts: u32) {
        let duration = self.timeout_duration(consecutive_timeouts);
        self.deadline = Instant::now() + duration;
        tracing::debug!(
            timeout_ms = duration.as_millis() as u64,
            consecutive_timeouts,
            "pacemaker timer reset"
        );
    }

    /// Returns a `Sleep` future that completes when the current view times out.
    ///
    /// Usage:
    /// ```ignore
    /// tokio::select! {
    ///     _ = pacemaker.timeout_sleep() => {
    ///         // View timed out
    ///     }
    ///     msg = rx.recv() => {
    ///         // Process consensus message
    ///     }
    /// }
    /// ```
    pub fn timeout_sleep(&self) -> Sleep {
        tokio::time::sleep_until(self.deadline)
    }

    /// Checks if the current view has timed out (non-blocking).
    pub fn is_timed_out(&self) -> bool {
        Instant::now() >= self.deadline
    }

    /// Returns the remaining time until timeout.
    pub fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_timeout_duration_base() {
        let pm = Pacemaker::new(1000, 60000);

        let duration = pm.timeout_duration(0);
        assert_eq!(
            duration,
            Duration::from_millis(1000),
            "consecutive_timeouts=0 should give base_timeout (1000ms)"
        );
    }

    #[test]
    fn test_timeout_duration_backoff() {
        let pm = Pacemaker::new(1000, 60000);

        // consecutive_timeouts=1 -> 1000 * 2^1 = 2000ms
        let d1 = pm.timeout_duration(1);
        assert_eq!(
            d1,
            Duration::from_millis(2000),
            "consecutive_timeouts=1 should give 2 * base_timeout"
        );

        // consecutive_timeouts=2 -> 1000 * 2^2 = 4000ms
        let d2 = pm.timeout_duration(2);
        assert_eq!(
            d2,
            Duration::from_millis(4000),
            "consecutive_timeouts=2 should give 4 * base_timeout"
        );

        // consecutive_timeouts=3 -> 1000 * 2^3 = 8000ms
        let d3 = pm.timeout_duration(3);
        assert_eq!(
            d3,
            Duration::from_millis(8000),
            "consecutive_timeouts=3 should give 8 * base_timeout"
        );
    }

    #[test]
    fn test_timeout_duration_capped() {
        let pm = Pacemaker::new(1000, 30000);

        // consecutive_timeouts=10 -> 1000 * 2^10 = 1_024_000ms, but capped at 30_000ms
        let duration = pm.timeout_duration(10);
        assert_eq!(
            duration,
            Duration::from_millis(30000),
            "high consecutive_timeouts should be capped at max_timeout"
        );

        // consecutive_timeouts=5 -> 1000 * 32 = 32000, capped at 30000
        let d5 = pm.timeout_duration(5);
        assert_eq!(
            d5,
            Duration::from_millis(30000),
            "32000 > 30000 max, should be capped"
        );

        // consecutive_timeouts=4 -> 1000 * 16 = 16000, NOT capped (16000 < 30000)
        let d4 = pm.timeout_duration(4);
        assert_eq!(
            d4,
            Duration::from_millis(16000),
            "16000 < 30000 max, should not be capped"
        );
    }

    #[test]
    fn test_is_timed_out_initially() {
        // base_timeout of 5 seconds - should NOT be timed out right after creation
        let pm = Pacemaker::new(5000, 60000);
        assert!(
            !pm.is_timed_out(),
            "pacemaker should not be timed out immediately after creation"
        );
    }

    #[test]
    fn test_remaining_positive_initially() {
        let pm = Pacemaker::new(5000, 60000);
        let remaining = pm.remaining();
        assert!(
            remaining > Duration::ZERO,
            "remaining time should be positive immediately after creation"
        );
        assert!(
            remaining <= Duration::from_millis(5000),
            "remaining time should not exceed base_timeout"
        );
    }

    #[test]
    fn test_reset_for_view() {
        let mut pm = Pacemaker::new(1000, 60000);

        // Reset with 0 consecutive timeouts -> 1000ms timeout
        pm.reset_for_view(2, 0);
        assert!(!pm.is_timed_out(), "should not be timed out after reset");

        let remaining = pm.remaining();
        assert!(
            remaining <= Duration::from_millis(1000),
            "remaining should be <= 1000ms after reset with 0 timeouts"
        );

        // Reset with 2 consecutive timeouts -> 4000ms timeout
        pm.reset_for_view(3, 2);
        assert!(!pm.is_timed_out(), "should not be timed out after reset");

        let remaining = pm.remaining();
        assert!(
            remaining <= Duration::from_millis(4000),
            "remaining should be <= 4000ms after reset with 2 timeouts"
        );
    }
}
