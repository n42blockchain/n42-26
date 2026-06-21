//! Compact-block execution-output cache port — the trait boundary over reth's
//! `reth_evm::payload_cache`. The leader serializes its EVM execution output so
//! followers can skip re-execution; the follower injects it before `new_payload`.
//! All reth coupling lives in the node-side adapter (`RethExecutionOutputCache`
//! in `n42-node`); this crate holds only the byte-oriented trait. Caplin EL-seam
//! refactor (stage 6a-2 / 6c).

use alloy_primitives::B256;

/// Port the orchestrator calls instead of reth's `payload_cache` directly. One
/// in-process adapter today (`RethExecutionOutputCache`, node-side); both sides
/// exchange the compressed wire blob only.
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
