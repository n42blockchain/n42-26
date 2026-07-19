use alloy_primitives::B256;
use alloy_rpc_types_engine::PayloadStatusEnum;
use metrics::{counter, gauge};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use tracing::{error, warn};

const DEFAULT_CAPACITY: usize = 512;
const MAX_REASON_LEN: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
enum BadBlockReason {
    InvalidPayload(String),
}

impl std::fmt::Display for BadBlockReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidPayload(reason) => write!(f, "invalid payload: {reason}"),
        }
    }
}

#[derive(Debug)]
struct Inner {
    capacity: usize,
    reasons: HashMap<B256, BadBlockReason>,
    lru: VecDeque<B256>,
}

/// Process-local, bounded cache of payloads that reth deterministically rejected.
///
/// This is deliberately not persisted: an Engine implementation upgrade may make
/// yesterday's verdict obsolete. Only an explicit `new_payload(INVALID)` may be
/// inserted; transport errors and retryable statuses must never poison the cache.
#[derive(Clone, Debug)]
pub(crate) struct BadBlockCache {
    inner: Arc<Mutex<Inner>>,
}

impl Default for BadBlockCache {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }
}

impl BadBlockCache {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                capacity: capacity.max(1),
                reasons: HashMap::new(),
                lru: VecDeque::new(),
            })),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|poisoned| {
            error!(target: "n42::bad_block_cache", "bad-block cache mutex poisoned; recovering contents");
            poisoned.into_inner()
        })
    }

    /// Returns true for a known deterministic reject and refreshes its LRU age.
    pub(crate) fn should_skip(&self, hash: B256, source: &'static str) -> bool {
        let reason = {
            let mut inner = self.lock();
            let Some(reason) = inner.reasons.get(&hash).cloned() else {
                return false;
            };
            touch(&mut inner.lru, hash);
            reason
        };

        counter!("n42_bad_block_cache_hits_total", "source" => source).increment(1);
        warn!(
            target: "n42::bad_block_cache",
            %hash,
            source,
            reason = %reason,
            "skipping previously rejected payload"
        );
        true
    }

    /// Records an explicit Engine API `new_payload(INVALID)` verdict.
    pub(crate) fn insert_if_invalid(
        &self,
        hash: B256,
        status: &PayloadStatusEnum,
        source: &'static str,
    ) -> bool {
        let PayloadStatusEnum::Invalid { validation_error } = status else {
            return false;
        };
        self.insert_invalid_payload(hash, validation_error, source);
        true
    }

    /// Records an explicit Engine API `new_payload(INVALID)` verdict.
    pub(crate) fn insert_invalid_payload(
        &self,
        hash: B256,
        validation_error: &str,
        source: &'static str,
    ) {
        let reason = BadBlockReason::InvalidPayload(truncate_reason(validation_error));
        let (inserted, evicted, len) = {
            let mut inner = self.lock();
            let inserted = !inner.reasons.contains_key(&hash);
            inner.reasons.insert(hash, reason.clone());
            touch(&mut inner.lru, hash);

            let mut evicted = None;
            if inner.reasons.len() > inner.capacity
                && let Some(oldest) = inner.lru.pop_front()
            {
                inner.reasons.remove(&oldest);
                evicted = Some(oldest);
            }
            (inserted, evicted, inner.reasons.len())
        };

        if inserted {
            counter!("n42_bad_block_cache_inserts_total", "source" => source).increment(1);
        }
        if let Some(evicted_hash) = evicted {
            counter!("n42_bad_block_cache_evictions_total").increment(1);
            warn!(
                target: "n42::bad_block_cache",
                %evicted_hash,
                "evicted least-recently-used bad block"
            );
        }
        gauge!("n42_bad_block_cache_entries").set(len as f64);
        warn!(
            target: "n42::bad_block_cache",
            %hash,
            source,
            reason = %reason,
            inserted,
            "cached deterministic payload rejection"
        );
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.lock().reasons.len()
    }
}

fn touch(lru: &mut VecDeque<B256>, hash: B256) {
    if let Some(position) = lru.iter().position(|candidate| *candidate == hash) {
        lru.remove(position);
    }
    lru.push_back(hash);
}

fn truncate_reason(reason: &str) -> String {
    let mut end = reason.len().min(MAX_REASON_LEN);
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    reason[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_least_recently_used_entry_at_capacity() {
        let cache = BadBlockCache::with_capacity(2);
        let first = B256::repeat_byte(1);
        let second = B256::repeat_byte(2);
        let third = B256::repeat_byte(3);
        cache.insert_invalid_payload(first, "one", "test");
        cache.insert_invalid_payload(second, "two", "test");

        assert!(cache.should_skip(first, "test"), "lookup refreshes LRU age");
        cache.insert_invalid_payload(third, "three", "test");

        assert!(cache.should_skip(first, "test"));
        assert!(!cache.should_skip(second, "test"));
        assert!(cache.should_skip(third, "test"));
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn replacement_is_bounded_and_does_not_grow_cache() {
        let cache = BadBlockCache::with_capacity(2);
        let hash = B256::repeat_byte(4);
        let long_reason = "无".repeat(MAX_REASON_LEN);
        cache.insert_invalid_payload(hash, &long_reason, "test");
        {
            let inner = cache.lock();
            let Some(BadBlockReason::InvalidPayload(reason)) = inner.reasons.get(&hash) else {
                panic!("invalid payload reason should be stored");
            };
            assert!(reason.len() <= MAX_REASON_LEN);
            assert!(reason.is_char_boundary(reason.len()));
        }
        cache.insert_invalid_payload(hash, "replacement", "test");

        let inner = cache.lock();
        assert_eq!(inner.reasons.len(), 1);
        assert_eq!(
            inner.reasons.get(&hash),
            Some(&BadBlockReason::InvalidPayload("replacement".to_owned()))
        );
    }
}
