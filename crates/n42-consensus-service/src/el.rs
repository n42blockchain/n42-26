//! Execution-layer port — a trait boundary over the reth Engine API that the
//! consensus service drives, decoupling consensus from the concrete reth handles
//! (a Caplin-style `ExecutionEngine` seam). The one in-process adapter
//! (`RethExecutionLayer`) lives node-side in `n42-node`; this crate holds only
//! the trait + node-neutral types. Design: `docs/task-caplin-cl-module.md`.

use alloy_primitives::B256;
use alloy_rpc_types_engine::{
    ExecutionData, ForkchoiceState, ForkchoiceUpdated, PayloadAttributes, PayloadId, PayloadStatus,
};
pub use n42_execution::ExecutionPath;

/// Error at the EL boundary. Erases reth's concrete engine error enums
/// (`BeaconOnNewPayloadError` / `BeaconForkChoiceUpdateError` / `PayloadBuilderError`)
/// — every current call site only logs the message and branches on `Ok`/`Err`.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ElError(pub String);

/// Node-neutral result of a completed payload build. Carries only alloy/std
/// types so the [`ExecutionLayer`] trait stays free of reth concretes (the
/// in-process adapter converts reth's `EthBuiltPayload` into this). The
/// orchestrator broadcasts + eager-imports from these fields.
#[derive(Debug, Clone)]
pub struct BuiltBlock {
    /// Block hash of the built block.
    pub hash: B256,
    /// Block number.
    pub number: u64,
    /// Block timestamp (seconds).
    pub timestamp: u64,
    /// Number of transactions in the block.
    pub tx_count: usize,
    /// Engine-API execution payload (alloy wire type) for re-import via
    /// `new_payload` and serialization to followers.
    pub execution_data: ExecutionData,
    /// Transaction hashes of the EIP-4844 (blob) transactions in this block,
    /// used to gather + broadcast their sidecars.
    pub blob_tx_hashes: Vec<B256>,
}

/// How to resolve a started build — node-neutral replacement for reth's
/// `PayloadKind` (the adapter maps it). Only the wait-for-pending mode is used
/// today (block until the builder finishes packing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveKind {
    /// Wait for the pending build to finish before resolving.
    WaitForPending,
}

/// The execution-layer seam the consensus orchestrator calls instead of holding
/// reth's `ConsensusEngineHandle` / `PayloadBuilderHandle` directly.
///
/// Methods mirror the exact reth calls made today (file:line in `RethExecutionLayer`),
/// returning the alloy *wire* types (`ForkchoiceUpdated` / `PayloadStatus`) so call
/// sites keep reading `.payload_status.status` / `.status` unchanged.
#[async_trait::async_trait]
pub trait ExecutionLayer: Send + Sync + 'static {
    /// Engine-API `newPayload` — insert/validate a block in the EL.
    async fn new_payload(&self, payload: ExecutionData) -> Result<PayloadStatus, ElError>;

    /// Classified Engine-API `newPayload` call.
    ///
    /// The raw method remains the implementation seam for adapters and test
    /// doubles. Production callers should use this method so historical
    /// sequential replay is never aggregated with live EVM latency. PEVM
    /// historical replay is intentionally rejected: it is an independent,
    /// read-only replay workload and must not mutate the canonical engine tree.
    async fn new_payload_for(
        &self,
        path: ExecutionPath,
        payload: ExecutionData,
    ) -> Result<PayloadStatus, ElError> {
        if !path.uses_current_engine_api() {
            return Err(ElError(format!(
                "execution path {} is not implemented by the canonical Engine API adapter",
                path.label()
            )));
        }

        let started = std::time::Instant::now();
        let result = self.new_payload(payload).await;
        let outcome = if result.is_ok() { "ok" } else { "error" };
        metrics::histogram!(
            "n42_evm_path_duration_ms",
            "path" => path.label(),
            "phase" => "new_payload",
        )
        .record(started.elapsed().as_secs_f64() * 1_000.0);
        metrics::counter!(
            "n42_evm_path_calls_total",
            "path" => path.label(),
            "phase" => "new_payload",
            "outcome" => outcome,
        )
        .increment(1);
        result
    }

    /// Engine-API `forkchoiceUpdated` WITHOUT attributes (finalize / import path).
    async fn fork_choice_updated(
        &self,
        state: ForkchoiceState,
    ) -> Result<ForkchoiceUpdated, ElError>;

    /// Classified canonical-head update paired with [`Self::new_payload_for`].
    async fn fork_choice_updated_for(
        &self,
        path: ExecutionPath,
        state: ForkchoiceState,
    ) -> Result<ForkchoiceUpdated, ElError> {
        if !path.uses_current_engine_api() || !path.may_write_canonical_state() {
            return Err(ElError(format!(
                "execution path {} may not update canonical fork choice",
                path.label()
            )));
        }

        let started = std::time::Instant::now();
        let result = self.fork_choice_updated(state).await;
        let outcome = if result.is_ok() { "ok" } else { "error" };
        metrics::histogram!(
            "n42_evm_path_duration_ms",
            "path" => path.label(),
            "phase" => "forkchoice_updated",
        )
        .record(started.elapsed().as_secs_f64() * 1_000.0);
        metrics::counter!(
            "n42_evm_path_calls_total",
            "path" => path.label(),
            "phase" => "forkchoice_updated",
            "outcome" => outcome,
        )
        .increment(1);
        result
    }

    /// `forkchoiceUpdated` WITH attributes — starts a payload build; the caller
    /// reads `.payload_id`. Kept separate from the attribute-less FCU so the
    /// finalize path can later move off the consensus hot path.
    async fn fork_choice_updated_with_attrs(
        &self,
        state: ForkchoiceState,
        attrs: PayloadAttributes,
    ) -> Result<ForkchoiceUpdated, ElError>;

    /// Classified FCU with payload attributes, used to start a live payload build.
    async fn fork_choice_updated_with_attrs_for(
        &self,
        path: ExecutionPath,
        state: ForkchoiceState,
        attrs: PayloadAttributes,
    ) -> Result<ForkchoiceUpdated, ElError> {
        if !path.may_start_payload_build() {
            return Err(ElError(format!(
                "execution path {} may not start a canonical payload build",
                path.label()
            )));
        }

        let started = std::time::Instant::now();
        let result = self.fork_choice_updated_with_attrs(state, attrs).await;
        let outcome = if result.is_ok() { "ok" } else { "error" };
        metrics::histogram!(
            "n42_evm_path_duration_ms",
            "path" => path.label(),
            "phase" => "forkchoice_updated_with_attrs",
        )
        .record(started.elapsed().as_secs_f64() * 1_000.0);
        metrics::counter!(
            "n42_evm_path_calls_total",
            "path" => path.label(),
            "phase" => "forkchoice_updated_with_attrs",
            "outcome" => outcome,
        )
        .increment(1);
        result
    }

    /// Resolve a started build to its payload (blocks until the pending build
    /// completes). `None` ⇒ no such job. Returns a node-neutral [`BuiltBlock`]
    /// so this trait carries no reth types — the in-process adapter converts
    /// reth's `EthBuiltPayload`.
    async fn resolve_payload(
        &self,
        id: PayloadId,
        kind: ResolveKind,
    ) -> Option<Result<BuiltBlock, ElError>>;
}
