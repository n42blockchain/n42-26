//! What the engine holds unpersisted, a periodic memory line, and pacing of
//! builds behind persistence.
//!
//! Every canonical block the engine has not yet written to disk stays in
//! memory with its transactions, receipts, bundle state and hashed state. On
//! an execution owner under sustained load persistence keeps less than the
//! chain makes, so that backlog grows for as long as the load lasts. Reth's
//! own backpressure counts blocks, empty or full, and stalls the engine when
//! it trips; [`wait_for_persistence`] instead holds a build back while the
//! unpersisted blocks carry more than `N42_MAX_UNPERSISTED_TXS` transactions,
//! so the chain slows to what persistence keeps up with and memory stays
//! bounded.
//!
//! `N42_MEM_LOG_SECS=<s>` logs an `N42_MEM` line every `s` seconds (0: off):
//! resident memory, the allocator's counters when the binary registers them,
//! RocksDB's memtables, the unpersisted backlog and the frame queue.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Canonical blocks the engine holds in memory, not yet persisted.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Backlog {
    pub blocks: u64,
    pub transactions: u64,
}

type BacklogSource = Box<dyn Fn() -> Backlog + Send + Sync>;

/// Allocator counters, in bytes, by name.
type AllocatorSource = Box<dyn Fn() -> Vec<(&'static str, u64)> + Send + Sync>;

/// Memtable memory of reth's RocksDB tables, in bytes.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RocksdbMemtables {
    /// Every memtable still allocated, flushed ones RocksDB keeps as history
    /// included (`rocksdb.size-all-mem-tables`).
    pub all: u64,
    /// The unflushed memtables only (`rocksdb.cur-size-all-mem-tables`).
    pub unflushed: u64,
}

type RocksdbSource = Box<dyn Fn() -> RocksdbMemtables + Send + Sync>;

fn backlog_source() -> &'static OnceLock<BacklogSource> {
    static SOURCE: OnceLock<BacklogSource> = OnceLock::new();
    &SOURCE
}

fn allocator_source() -> &'static OnceLock<AllocatorSource> {
    static SOURCE: OnceLock<AllocatorSource> = OnceLock::new();
    &SOURCE
}

fn rocksdb_source() -> &'static OnceLock<RocksdbSource> {
    static SOURCE: OnceLock<RocksdbSource> = OnceLock::new();
    &SOURCE
}

/// Registers how to read the engine's unpersisted backlog; the first
/// registration wins.
pub fn set_backlog_source(source: impl Fn() -> Backlog + Send + Sync + 'static) {
    let _ = backlog_source().set(Box::new(source));
}

/// Registers how to read the allocator's counters; the first registration
/// wins.
pub fn set_allocator_source(source: impl Fn() -> Vec<(&'static str, u64)> + Send + Sync + 'static) {
    let _ = allocator_source().set(Box::new(source));
}

/// Registers how to read RocksDB's memtables; the first registration wins.
pub fn set_rocksdb_source(source: impl Fn() -> RocksdbMemtables + Send + Sync + 'static) {
    let _ = rocksdb_source().set(Box::new(source));
}

/// The unpersisted backlog, when a source is registered.
pub fn backlog() -> Option<Backlog> {
    backlog_source().get().map(|source| source())
}

/// `N42_MAX_UNPERSISTED_TXS=<n>`: builds wait while the unpersisted blocks
/// carry at least `n` transactions. Unset or 0: off.
fn max_unpersisted_transactions() -> Option<u64> {
    static MAX: OnceLock<u64> = OnceLock::new();
    let max = *MAX.get_or_init(|| {
        std::env::var("N42_MAX_UNPERSISTED_TXS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0)
    });
    (max > 0).then_some(max)
}

/// `N42_BACKLOG_WAIT_MS=<ms>`: the longest one build waits for persistence
/// (default 1,000, at most 3,000, below the 4 s base view timeout). A build
/// that waited that long goes ahead regardless.
fn backlog_wait_limit() -> Duration {
    static LIMIT: OnceLock<u64> = OnceLock::new();
    Duration::from_millis(*LIMIT.get_or_init(|| {
        std::env::var("N42_BACKLOG_WAIT_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(1_000)
            .min(3_000)
    }))
}

/// Holds a build back while [`backlog`] is at or above
/// `N42_MAX_UNPERSISTED_TXS`, up to `N42_BACKLOG_WAIT_MS`. Returns how long
/// it waited, if the limit is set and the backlog was over it.
pub fn wait_for_persistence() -> Option<Duration> {
    let max = max_unpersisted_transactions()?;
    wait_below(max, backlog_wait_limit(), || {
        backlog().map_or(0, |b| b.transactions)
    })
}

fn wait_below(max: u64, limit: Duration, transactions: impl Fn() -> u64) -> Option<Duration> {
    if transactions() < max {
        return None;
    }
    let started = Instant::now();
    while started.elapsed() < limit && transactions() >= max {
        std::thread::sleep(Duration::from_millis(5));
    }
    let waited = started.elapsed();
    metrics::histogram!("n42_persistence_backlog_wait_ms").record(waited.as_secs_f64() * 1_000.0);
    if transactions() >= max {
        metrics::counter!("n42_persistence_backlog_wait_expired_total").increment(1);
    }
    Some(waited)
}

/// Resident set size of this process, from `/proc/self/statm`.
fn resident_bytes() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(pages * 4096)
}

/// Starts the `N42_MEM` line when `N42_MEM_LOG_SECS` is set and above 0.
pub fn spawn_memory_log() {
    let seconds = std::env::var("N42_MEM_LOG_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    if seconds == 0 {
        return;
    }
    let every = Duration::from_secs(seconds);
    let spawned = std::thread::Builder::new()
        .name("n42-mem".into())
        .spawn(move || {
            loop {
                log_memory();
                std::thread::sleep(every);
            }
        });
    if let Err(error) = spawned {
        tracing::warn!(target: "n42::mem", %error, "memory log thread not started");
    }
}

fn mib(bytes: u64) -> u64 {
    bytes >> 20
}

fn log_memory() {
    let rss = resident_bytes().unwrap_or(0);
    metrics::gauge!("n42_mem_resident_bytes").set(rss as f64);
    let allocator = allocator_source()
        .get()
        .map(|source| source())
        .unwrap_or_default();
    for (name, bytes) in &allocator {
        metrics::gauge!("n42_mem_allocator_bytes", "counter" => *name).set(*bytes as f64);
    }
    let allocator_mib = allocator
        .iter()
        .map(|(name, bytes)| format!("{name}={}", mib(*bytes)))
        .collect::<Vec<_>>()
        .join(" ");
    let memtables = rocksdb_source()
        .get()
        .map(|source| source())
        .unwrap_or_default();
    metrics::gauge!("n42_mem_rocksdb_memtable_bytes").set(memtables.all as f64);
    metrics::gauge!("n42_mem_rocksdb_memtable_unflushed_bytes").set(memtables.unflushed as f64);
    let backlog = backlog().unwrap_or_default();
    metrics::gauge!("n42_unpersisted_blocks").set(backlog.blocks as f64);
    metrics::gauge!("n42_unpersisted_transactions").set(backlog.transactions as f64);
    let frame_queue_pending = if crate::frame_queue::frame_queue_enabled() {
        crate::frame_queue::FrameQueue::global().pending()
    } else {
        0
    };
    let frame_store_frames = if n42_tx_types::frame_ref_blocks_enabled() {
        n42_tx_types::FrameStore::global().len()
    } else {
        0
    };
    tracing::info!(
        target: "n42::mem",
        rss_mib = mib(rss),
        allocator_mib = %allocator_mib,
        rocksdb_memtable_mib = mib(memtables.all),
        rocksdb_memtable_unflushed_mib = mib(memtables.unflushed),
        unpersisted_blocks = backlog.blocks,
        unpersisted_txs = backlog.transactions,
        frame_queue_pending,
        frame_store_frames,
        "N42_MEM"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn a_build_waits_until_the_backlog_is_below_the_limit_or_the_wait_is_over() {
        assert_eq!(wait_below(10, Duration::from_secs(1), || 9), None);

        let backlog = AtomicU64::new(12);
        let waited = std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(40));
                backlog.store(3, Ordering::Relaxed);
            });
            wait_below(10, Duration::from_secs(5), || {
                backlog.load(Ordering::Relaxed)
            })
        })
        .expect("over the limit at first");
        assert!(waited >= Duration::from_millis(40), "{waited:?}");
        assert!(waited < Duration::from_secs(5), "{waited:?}");

        let waited = wait_below(10, Duration::from_millis(30), || 11).expect("over the limit");
        assert!(waited >= Duration::from_millis(30), "{waited:?}");
    }

    #[test]
    fn resident_memory_is_read() {
        assert!(resident_bytes().is_some_and(|bytes| bytes > 0));
    }
}
