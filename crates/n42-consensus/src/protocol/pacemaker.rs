use n42_primitives::consensus::ViewNumber;
use std::time::Duration;
use tokio::time::{Instant, Sleep};

/// Active Pacemaker for the HotStuff-2 consensus protocol.
///
/// Manages view timeouts with exponential backoff (Jolteon-style):
/// `timeout = min(base_timeout * 2^consecutive_timeouts, max_timeout)`
///
/// Ensures liveness: if the current leader is faulty or the network is slow,
/// validators will eventually time out and trigger a view change.
#[derive(Debug)]
pub struct Pacemaker {
    base_timeout: Duration,
    max_timeout: Duration,
    deadline: Instant,
}

impl Pacemaker {
    pub fn new(base_timeout_ms: u64, max_timeout_ms: u64) -> Self {
        let base_timeout = Duration::from_millis(base_timeout_ms);
        let max_timeout = Duration::from_millis(max_timeout_ms);
        Self {
            base_timeout,
            max_timeout,
            deadline: Instant::now() + base_timeout,
        }
    }

    /// Returns the timeout duration for the given number of consecutive timeouts.
    ///
    /// Uses exponential backoff: `base * 2^consecutive_timeouts`, capped at `max_timeout`.
    pub fn timeout_duration(&self, consecutive_timeouts: u32) -> Duration {
        let multiplier = 1u64.checked_shl(consecutive_timeouts).unwrap_or(u64::MAX);
        let timeout_ms = self
            .base_timeout
            .as_millis()
            .saturating_mul(multiplier as u128);
        Duration::from_millis(timeout_ms.min(self.max_timeout.as_millis()) as u64)
    }

    /// Resets the timer for a new view with the appropriate backoff duration.
    pub fn reset_for_view(&mut self, view: ViewNumber, consecutive_timeouts: u32) {
        let duration = self.timeout_duration(consecutive_timeouts);
        self.deadline = Instant::now() + duration;
        tracing::debug!(
            view,
            timeout_ms = duration.as_millis() as u64,
            consecutive_timeouts,
            "pacemaker timer reset"
        );
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
}
