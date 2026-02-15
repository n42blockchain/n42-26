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
