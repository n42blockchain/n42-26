//! Serialize competing imports of one hash so the engine can reuse its result.
//!
//! This gate never caches a payload status or trusts a declared block hash:
//! every request still enters the engine with its complete payload. Different
//! hashes proceed independently. Cancellation releases the lock automatically.

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
}
