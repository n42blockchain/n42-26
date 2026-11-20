//! Serialize competing imports of one hash so the engine can reuse its result.
//!
//! This gate never caches a payload status or trusts a declared block hash:
//! every request still enters the engine with its complete payload. Different
//! hashes proceed independently, except that an import can wait for its
//! parent's ([`ImportGate::settled`]). Cancellation releases the lock
//! automatically.

use alloy_primitives::B256;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

#[derive(Default)]
pub(super) struct ImportGate {
    entries: Mutex<HashMap<B256, Weak<AsyncMutex<()>>>>,
}

impl ImportGate {
    /// Opt in until fleet measurements establish its effect on import latency.
    pub(super) fn configured() -> Option<Self> {
        static ENABLED: OnceLock<bool> = OnceLock::new();
        ENABLED
            .get_or_init(|| {
                std::env::var("N42_IMPORT_SINGLE_FLIGHT").is_ok_and(|value| value == "1")
            })
            .then(Self::default)
    }

    fn entry(&self, hash: B256) -> Arc<AsyncMutex<()>> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(entry) = entries.get(&hash).and_then(Weak::upgrade) {
            return entry;
        }
        // Weak entries retain neither execution outputs nor transaction bodies.
        // Reclaim idle hashes periodically; active owners and waiters keep their
        // entries alive, so cleanup cannot create a second lock for their hash.
        if entries.len() >= 64 {
            entries.retain(|_, entry| entry.strong_count() != 0);
        }
        let entry = Arc::new(AsyncMutex::new(()));
        entries.insert(hash, Arc::downgrade(&entry));
        entry
    }

    pub(super) async fn acquire(&self, hash: B256) -> OwnedMutexGuard<()> {
        let entry = self.entry(hash);
        let started = std::time::Instant::now();
        let guard = match entry.clone().try_lock_owned() {
            Ok(guard) => guard,
            Err(_) => {
                metrics::counter!("n42_engine_import_gate_contention_total").increment(1);
                entry.lock_owned().await
            }
        };
        metrics::histogram!("n42_engine_import_gate_wait_ms")
            .record(started.elapsed().as_secs_f64() * 1_000.0);
        guard
    }

    /// The lock of an import of `hash` that is under way or waited for, if
    /// there is one. Unlike [`Self::entry`] it makes none.
    fn under_way(&self, hash: &B256) -> Option<Arc<AsyncMutex<()>>> {
        let entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        entries.get(hash).and_then(Weak::upgrade)
    }

    /// Returns once no import of `hash` is under way here: at once when
    /// there is none, which is also the case for a block that has not
    /// arrived yet.
    pub(super) async fn settled(&self, hash: B256) {
        if let Some(entry) = self.under_way(&hash) {
            drop(entry.lock().await);
        }
    }

    /// [`Self::settled`] for a thread that may block.
    pub(super) fn settled_blocking(&self, hash: B256) {
        if let Some(entry) = self.under_way(&hash) {
            drop(entry.blocking_lock());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    /// An import waits for its parent's while that one is under way, on an
    /// async task and on a blocking thread, and does not wait for a block
    /// nobody is importing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_import_stands_behind_its_parents_only_while_that_is_under_way() {
        let gate = Arc::new(ImportGate::default());
        let parent = B256::repeat_byte(1);
        tokio::time::timeout(Duration::from_secs(5), gate.settled(parent))
            .await
            .expect("no import of the parent is under way");
        assert!(gate.under_way(&parent).is_none(), "asking makes no entry");

        let held = gate.acquire(parent).await;
        let done = Arc::new(AtomicBool::new(false));
        let waiter = {
            let (gate, done) = (gate.clone(), done.clone());
            tokio::spawn(async move {
                gate.settled(parent).await;
                done.load(Ordering::SeqCst)
            })
        };
        let blocking = {
            let (gate, done) = (gate.clone(), done.clone());
            tokio::task::spawn_blocking(move || {
                gate.settled_blocking(parent);
                done.load(Ordering::SeqCst)
            })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiter.is_finished() && !blocking.is_finished());
        done.store(true, Ordering::SeqCst);
        drop(held);
        assert!(waiter.await.unwrap(), "released only when the parent's import ended");
        assert!(blocking.await.unwrap());

        gate.settled(parent).await;
    }
}
