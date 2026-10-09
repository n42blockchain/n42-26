//! Process-local evidence about the registered execution read adapter.
//!
//! Counts are sharded by thread to avoid one contended atomic per state read.
//! A snapshot is monotonic per counter, not a transactionally consistent cut.

use alloy_primitives::B256;
use serde::{Deserialize, Serialize};
use std::sync::{
    OnceLock,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};

const SHARDS: usize = 64;
const EVENTS: usize = 9;
static NEXT_SLOT: AtomicUsize = AtomicUsize::new(0);
thread_local! {
    static SLOT: usize = NEXT_SLOT.fetch_add(1, Ordering::Relaxed) % SHARDS;
}

#[derive(Clone, Copy)]
pub(crate) enum ReadEvent {
    Account,
    Storage,
    AccountComparison,
    StorageComparison,
    Mismatch,
    ReadError,
    ProviderError,
    PinnedProvider,
    UnavailableProvider,
}

#[repr(align(128))]
struct CounterShard([AtomicU64; EVENTS]);

pub(crate) struct QmdbReadCounters([CounterShard; SHARDS]);

impl Default for QmdbReadCounters {
    fn default() -> Self {
        Self(std::array::from_fn(|_| {
            CounterShard(std::array::from_fn(|_| AtomicU64::new(0)))
        }))
    }
}

impl QmdbReadCounters {
    pub(crate) fn increment(&self, event: ReadEvent) {
        SLOT.with(|slot| {
            self.0[*slot].0[event as usize].fetch_add(1, Ordering::Relaxed);
        });
    }

    pub(crate) fn snapshot(&self) -> QmdbReadCounts {
        let totals: [u64; EVENTS] = std::array::from_fn(|event| {
            self.0
                .iter()
                .map(|shard| shard.0[event].load(Ordering::Relaxed))
                .sum()
        });
        QmdbReadCounts {
            account_reads: totals[0],
            storage_reads: totals[1],
            account_comparisons: totals[2],
            storage_comparisons: totals[3],
            mismatches: totals[4],
            read_errors: totals[5],
            provider_errors: totals[6],
            pinned_providers: totals[7],
            unavailable_providers: totals[8],
        }
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct QmdbReadCounts {
    pub account_reads: u64,
    pub storage_reads: u64,
    pub account_comparisons: u64,
    pub storage_comparisons: u64,
    pub mismatches: u64,
    pub read_errors: u64,
    pub provider_errors: u64,
    pub pinned_providers: u64,
    pub unavailable_providers: u64,
}

/// Self-reported process identity, not remote attestation or an auth token.
pub fn process_instance_id() -> B256 {
    static ID: OnceLock<B256> = OnceLock::new();
    *ID.get_or_init(|| B256::from(rand::random::<[u8; 32]>()))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QmdbReadStatus {
    pub schema: u32,
    pub instance_id: B256,
    pub process_id: u32,
    pub mode: super::qmdb_state_reader::QmdbReadsMode,
    pub backend: String,
    pub coverage: String,
    pub wal_enabled: bool,
    pub chain_id: Option<u64>,
    pub genesis_hash: Option<B256>,
    pub base_block_hash: Option<B256>,
    pub base_root: Option<B256>,
    pub requested_block_hash: Option<B256>,
    pub durable_root: Option<B256>,
    pub counters: QmdbReadCounts,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_survive_thread_churn_and_shard_collisions() {
        let counts = QmdbReadCounters::default();
        std::thread::scope(|scope| {
            for _ in 0..SHARDS * 2 {
                let counts = &counts;
                scope.spawn(move || {
                    for _ in 0..1000 {
                        counts.increment(ReadEvent::Account);
                    }
                    counts.increment(ReadEvent::ReadError);
                });
            }
        });
        let snapshot = counts.snapshot();
        assert_eq!(snapshot.account_reads, SHARDS as u64 * 2000);
        assert_eq!(snapshot.read_errors, SHARDS as u64 * 2);
        assert_eq!(snapshot.storage_reads, 0);
        assert_eq!(process_instance_id(), process_instance_id());
    }

    /// Incremental cost of RPC counters around real QMDB account lookups.
    /// Excludes the rest of the provider, EVM and consensus.
    #[test]
    #[ignore = "measurement, not a correctness gate"]
    fn bench_read_counter_overhead() {
        use alloy_primitives::{Address, U256};
        use n42_twig_core::{
            qmdb_compat::{QmdbOperation, encode_gov5_account_value, gov5_account_key},
            qmdb_leaf_tree::QmdbLeafTree,
        };
        let keys = 200_000u64;
        let address = |n: u64| {
            let mut bytes = [0; 20];
            bytes[12..].copy_from_slice(&n.to_be_bytes());
            Address::from(bytes)
        };
        let operations = (0..keys).map(|n| QmdbOperation {
            key: gov5_account_key(address(n).as_ref()),
            value: Some(encode_gov5_account_value(
                1,
                &U256::from(1_000_000_000).to_be_bytes(),
                &B256::ZERO.0,
            )),
        });
        let mut tree = QmdbLeafTree::new();
        let root = B256::from(tree.apply_sorted_ops(operations).unwrap());
        let view = crate::qmdb_read_view::QmdbReadView::from_tree(B256::ZERO, root, &tree);
        let counters = QmdbReadCounters::default();
        for threads in [1, 16] {
            for round in 0..6 {
                for enabled in if round % 2 == 0 {
                    [false, true]
                } else {
                    [true, false]
                } {
                    let before = counters.snapshot().account_reads;
                    let start = std::time::Instant::now();
                    let count = 1_000_000 / threads;
                    std::thread::scope(|scope| {
                        let mut workers = Vec::new();
                        for worker in 0..threads {
                            let (view, counters) = (&view, &counters);
                            workers.push(scope.spawn(move || {
                                let mut sum = 0;
                                for i in 0..count {
                                    let n = (i * 7919 + worker) as u64 % keys;
                                    sum += view.account(&address(n)).unwrap().unwrap().nonce;
                                    if enabled {
                                        counters.increment(ReadEvent::Account);
                                    }
                                }
                                assert_eq!(sum, count as u64);
                            }));
                        }
                        for worker in workers {
                            worker.join().unwrap();
                        }
                    });
                    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
                    assert_eq!(
                        counters.snapshot().account_reads - before,
                        if enabled { 1_000_000 } else { 0 }
                    );
                    println!(
                        "read_counter threads={threads} round={round} enabled={enabled} reads=1000000 elapsed_ms={elapsed_ms:.3}"
                    );
                }
            }
        }
    }
}
