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

    /// Take the locally built execution output, derive the chain-native QMDB
    /// state and receipts commitments from the authenticated parent branch,
    /// and serialize the same output for compact broadcast. Gov5 header
    /// normalization changes the block hash after payload construction, so the
    /// orchestrator must bind the new header to what was actually executed.
    ///
    /// Adapters that do not support Gov5 normalization may keep the default
    /// `None`; an H2 participant then fails closed for non-empty blocks.
    fn take_gov5_normalization(
        &self,
        _hash: B256,
        _parent_hash: B256,
    ) -> Option<(B256, B256, Vec<u8>)> {
        None
    }

    /// Inject a received compact-block wire blob into the payload cache so the
    /// next `new_payload` hits the cache and skips EVM re-execution. Returns
    /// whether the inject succeeded.
    fn inject(&self, hash: B256, compressed: &[u8], source: &'static str) -> bool;

    /// Remove a previously injected execution output. Every caller that
    /// injects untrusted bytes must evict them when `new_payload` does not
    /// return `Valid`; otherwise a rejected bundle can poison later arrivals
    /// for the same declared block hash.
    fn evict(&self, hash: B256);
}
