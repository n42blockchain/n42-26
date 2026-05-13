//! Global single-entry cache for leader payload execution results.
//!
//! When the payload builder (leader) finishes building a block, it caches
//! the execution output here. When `validate_block_with_state` (new_payload)
//! processes the same block, it can skip EVM re-execution by retrieving the
//! cached result.
//!
//! Two cache slots exist:
//! - `CACHE`: consumed by the leader's own `new_payload` (take semantics)
//! - `BROADCAST_CACHE`: consumed by `handle_built_payload` for serialization
//!   to followers (Compact Block). Separated so the leader's `take` doesn't
//!   consume the data needed for broadcast.

use alloy_primitives::B256;
use std::any::Any;
use std::sync::{Mutex, OnceLock};

struct CachedPayload {
    block_hash: B256,
    data: Box<dyn Any + Send + Sync>,
}

type CacheSlot = OnceLock<Mutex<Option<CachedPayload>>>;

static CACHE: CacheSlot = OnceLock::new();

/// Secondary cache for broadcast to followers (Compact Block).
static BROADCAST_CACHE: CacheSlot = OnceLock::new();

fn store(slot: &CacheSlot, block_hash: B256, data: impl Any + Send + Sync + 'static) {
    let cache = slot.get_or_init(|| Mutex::new(None));
    let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some(CachedPayload { block_hash, data: Box::new(data) });
}

fn take<T: 'static>(slot: &CacheSlot, block_hash: &B256) -> Option<T> {
    let cache = slot.get_or_init(|| Mutex::new(None));
    let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(entry) = guard.as_ref() {
        if &entry.block_hash == block_hash {
            if !entry.data.is::<T>() {
                tracing::warn!(
                    target: "evm::payload_cache",
                    %block_hash,
                    "payload cache type mismatch on downcast"
                );
                return None;
            }
            let entry = guard.take().unwrap();
            return entry.data.downcast::<T>().ok().map(|b| *b);
        }
    }
    None
}

/// Store payload execution data keyed by block hash (for leader's own new_payload).
pub fn store_payload_execution(block_hash: B256, data: impl Any + Send + Sync + 'static) {
    store(&CACHE, block_hash, data);
}

/// Take (remove) the cached payload execution data if the block hash matches.
pub fn take_payload_execution<T: 'static>(block_hash: &B256) -> Option<T> {
    take::<T>(&CACHE, block_hash)
}

/// Store a clone of execution data for broadcast to followers (Compact Block).
pub fn store_broadcast_execution(block_hash: B256, data: impl Any + Send + Sync + 'static) {
    store(&BROADCAST_CACHE, block_hash, data);
}

/// Take broadcast execution data if block hash matches.
pub fn take_broadcast_execution<T: 'static>(block_hash: &B256) -> Option<T> {
    take::<T>(&BROADCAST_CACHE, block_hash)
}
