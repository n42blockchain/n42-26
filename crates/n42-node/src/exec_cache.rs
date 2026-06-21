//! Compact-block execution-output cache port — the trait boundary over reth's
//! `reth_evm::payload_cache` that the consensus orchestrator drives. The leader
//! serializes its EVM execution output so followers can skip re-execution; the
//! follower injects it into the payload cache before `new_payload`. All reth
//! coupling (`BlockExecutionOutput`, `reth_evm::payload_cache`, `Receipt`) lives
//! in the node-side adapter here, so the orchestrator and the future
//! `n42-consensus-service` crate exchange only bytes. Part of the Caplin EL-seam
//! refactor (stage 6a-2). The `ExecutionOutputCache` trait moves to the service
//! crate in stage 6c; the adapter + free fns stay node-side.

use crate::orchestrator::{CompactBlockExecution, compress_payload, decompress_payload};
use alloy_primitives::{Address, B256};
use reth_execution_types::{BlockExecutionOutput, BlockExecutionResult};
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use tracing::{info, warn};

/// Port the orchestrator calls instead of reth's `payload_cache` directly. One
/// in-process adapter today ([`RethExecutionOutputCache`]); both sides exchange
/// the compressed wire blob only.
pub trait ExecutionOutputCache: Send + Sync {
    /// Take + serialize + compress the cached compact-block execution output for
    /// `hash`. `None` when nothing is cached. The bytes are the wire blob the
    /// leader puts in `BlockDataBroadcast.execution_output`.
    fn take_serialized(&self, hash: B256) -> Option<Vec<u8>>;

    /// Inject a received compact-block wire blob into the payload cache so the
    /// next `new_payload` hits the cache and skips EVM re-execution. Returns
    /// whether the inject succeeded.
    fn inject(&self, hash: B256, compressed: &[u8], source: &'static str) -> bool;
}

/// In-process adapter over reth's global `reth_evm::payload_cache`.
pub struct RethExecutionOutputCache;

impl ExecutionOutputCache for RethExecutionOutputCache {
    fn take_serialized(&self, hash: B256) -> Option<Vec<u8>> {
        take_and_serialize_execution_output(&hash)
    }

    fn inject(&self, hash: B256, compressed: &[u8], source: &'static str) -> bool {
        inject_compact_block(&hash, compressed, source)
    }
}

type CachedPayloadData = (
    BlockExecutionOutput<reth_ethereum_primitives::Receipt>,
    Vec<Address>,
);

#[derive(Default)]
struct CompactInjectTracker {
    order: VecDeque<B256>,
    counts: HashMap<B256, u64>,
}

const COMPACT_INJECT_TRACKER_LIMIT: usize = 2048;

fn observe_compact_inject_attempt(hash: B256, source: &'static str) -> Option<u64> {
    static TRACKER: std::sync::OnceLock<Mutex<CompactInjectTracker>> = std::sync::OnceLock::new();
    let tracker = TRACKER.get_or_init(|| Mutex::new(CompactInjectTracker::default()));
    let mut tracker = tracker.lock().unwrap_or_else(|e| {
        tracing::warn!("compact_inject_tracker mutex poisoned, recovering");
        e.into_inner()
    });

    if let Some(seen) = tracker.counts.get_mut(&hash) {
        *seen += 1;
        let duplicate_attempt = *seen;
        metrics::counter!("n42_compact_inject_duplicate_total").increment(1);
        info!(
            target: "n42::cl::exec_bridge",
            %hash,
            source,
            duplicate_attempt,
            "N42_COMPACT_INJECT_DUP: repeated compact inject attempt"
        );
        return Some(duplicate_attempt);
    }

    tracker.counts.insert(hash, 1);
    tracker.order.push_back(hash);
    if tracker.order.len() > COMPACT_INJECT_TRACKER_LIMIT
        && let Some(evicted) = tracker.order.pop_front()
    {
        tracker.counts.remove(&evicted);
    }
    None
}

/// Take execution output from broadcast cache and serialize it for followers.
pub(crate) fn take_and_serialize_execution_output(hash: &B256) -> Option<Vec<u8>> {
    let (output, senders) =
        reth_evm::payload_cache::take_broadcast_execution::<CachedPayloadData>(hash)?;

    let ser_start = std::time::Instant::now();
    let compact = CompactBlockExecution {
        bundle_state: output.state,
        receipts: output.result.receipts,
        requests: output.result.requests,
        gas_used: output.result.gas_used,
        blob_gas_used: output.result.blob_gas_used,
        senders,
    };
    // Use serde_json (not bincode) because Receipt/BundleState serde impls use
    // custom formats (e.g., alloy_serde::quantity for hex u64) incompatible with bincode.
    match serde_json::to_vec(&compact) {
        Ok(serialized) => {
            let compressed = compress_payload(&serialized);
            let ser_ms = ser_start.elapsed().as_millis() as u64;
            info!(
                target: "n42::cl::exec_bridge",
                %hash,
                raw_kb = serialized.len() / 1024,
                compressed_kb = compressed.len() / 1024,
                ser_ms,
                "N42_COMPACT_BLOCK: execution output serialized for broadcast"
            );
            metrics::counter!("n42_compact_block_serialized").increment(1);
            metrics::histogram!("n42_compact_block_size_bytes").record(compressed.len() as f64);
            Some(compressed)
        }
        Err(e) => {
            warn!(target: "n42::cl::exec_bridge", %hash, error = %e, "compact block: failed to serialize execution output");
            None
        }
    }
}

/// Deserialize compact block execution output and load it into the payload cache.
pub(crate) fn inject_compact_block(hash: &B256, compressed: &[u8], source: &'static str) -> bool {
    let duplicate_attempt = observe_compact_inject_attempt(*hash, source);
    let inject_start = std::time::Instant::now();

    let decompress_start = std::time::Instant::now();
    let decompressed = match decompress_payload(compressed) {
        Ok(d) => d,
        Err(e) => {
            warn!(target: "n42::cl::exec_bridge", %hash, error = %e, "compact block: failed to decompress");
            return false;
        }
    };
    let decompress_ms = decompress_start.elapsed().as_millis() as u64;

    let deser_start = std::time::Instant::now();
    let compact: CompactBlockExecution = match serde_json::from_slice(&decompressed) {
        Ok(c) => c,
        Err(e) => {
            warn!(target: "n42::cl::exec_bridge", %hash, error = %e, "compact block: failed to deserialize");
            return false;
        }
    };
    let deser_ms = deser_start.elapsed().as_millis() as u64;

    let store_start = std::time::Instant::now();
    let output = BlockExecutionOutput {
        state: compact.bundle_state,
        result: BlockExecutionResult {
            receipts: compact.receipts,
            requests: compact.requests,
            gas_used: compact.gas_used,
            blob_gas_used: compact.blob_gas_used,
        },
    };
    reth_evm::payload_cache::store_payload_execution(*hash, (output, compact.senders));
    let store_ms = store_start.elapsed().as_millis() as u64;

    let total_ms = inject_start.elapsed().as_millis() as u64;
    info!(target: "n42::cl::exec_bridge", %hash,
        source,
        duplicate_attempt = duplicate_attempt.unwrap_or_default(),
        compressed_kb = compressed.len() / 1024,
        decompressed_kb = decompressed.len() / 1024,
        decompress_ms, deser_ms, store_ms, total_ms,
        "N42_COMPACT_INJECT: execution output injected into payload cache");
    metrics::counter!("n42_compact_block_cache_injected").increment(1);
    metrics::histogram!("n42_compact_inject_ms").record(total_ms as f64);
    true
}
