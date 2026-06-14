//! Execution-layer port — a trait boundary over the reth Engine API that the
//! consensus orchestrator drives, decoupling consensus from the concrete reth
//! handles (a Caplin-style `ExecutionEngine` seam; see `cl/phase1/execution_client`
//! in Erigon). One in-process adapter today ([`RethExecutionLayer`]); the seam
//! later enables reusing it from the observer, moving forkchoiceUpdated off the
//! consensus hot path, and (future) a standalone consensus binary against any
//! Engine-API EL. Design: `docs/task-caplin-cl-module.md`.

mod reth_engine;
pub use reth_engine::RethExecutionLayer;

use alloy_rpc_types_engine::{
    ExecutionData, ForkchoiceState, ForkchoiceUpdated, PayloadAttributes, PayloadId, PayloadStatus,
};
use reth_payload_builder::EthBuiltPayload;
use reth_payload_primitives::PayloadKind;

/// Error at the EL boundary. Erases reth's concrete engine error enums
/// (`BeaconOnNewPayloadError` / `BeaconForkChoiceUpdateError` / `PayloadBuilderError`)
/// — every current call site only logs the message and branches on `Ok`/`Err`.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ElError(pub String);

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

    /// Engine-API `forkchoiceUpdated` WITHOUT attributes (finalize / import path).
    async fn fork_choice_updated(
        &self,
        state: ForkchoiceState,
    ) -> Result<ForkchoiceUpdated, ElError>;

    /// `forkchoiceUpdated` WITH attributes — starts a payload build; the caller
    /// reads `.payload_id`. Kept separate from the attribute-less FCU so the
    /// finalize path can later move off the consensus hot path.
    async fn fork_choice_updated_with_attrs(
        &self,
        state: ForkchoiceState,
        attrs: PayloadAttributes,
    ) -> Result<ForkchoiceUpdated, ElError>;

    /// Resolve a started build to its payload (blocks until the pending build
    /// completes). `None` ⇒ no such job.
    ///
    /// NOTE: still returns reth's `EthBuiltPayload`, keeping the existing
    /// `handle_built_payload` conversion unchanged. A node-neutral `BuiltBlock`
    /// result is a follow-up for when this trait is promoted into its own crate.
    async fn resolve_payload(
        &self,
        id: PayloadId,
        kind: PayloadKind,
    ) -> Option<Result<EthBuiltPayload, ElError>>;
}
