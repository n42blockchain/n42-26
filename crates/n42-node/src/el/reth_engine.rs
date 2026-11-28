//! In-process [`ExecutionLayer`] adapter over reth's engine + payload-builder
//! handles — the single implementation today. Each method delegates 1:1 to the
//! call the orchestrator makes inline today, erasing the reth error type.

use super::{BuiltBlock, ElError, ExecutionLayer, ResolveKind};
use crate::engine_types::{N42BuiltPayload, N42EngineTypes};
use alloy_consensus::Typed2718;
use alloy_consensus::transaction::TxHashRef as _;
use alloy_rpc_types_engine::{
    ExecutionData, ForkchoiceState, ForkchoiceUpdated, PayloadAttributes, PayloadId, PayloadStatus,
};
use reth_node_builder::ConsensusEngineHandle;
use reth_payload_builder::PayloadBuilderHandle;
use reth_payload_primitives::{PayloadKind, PayloadTypes};

/// Wraps reth's `ConsensusEngineHandle` + (optional) `PayloadBuilderHandle`. Both
/// handles are cheaply cloneable (channel senders), so the adapter is cheap to
/// share via `Arc`. The payload builder is optional because non-producing roles
/// (the observer) only need `new_payload`/`fork_choice_updated`, never builds.
pub struct RethExecutionLayer {
    engine: ConsensusEngineHandle<N42EngineTypes>,
    payload_builder: Option<PayloadBuilderHandle<N42EngineTypes>>,
    import_gate: Option<std::sync::Arc<super::import_gate::ImportGate>>,
    follower_import: Option<super::FollowerImport>,
    /// The block this node resolved from its builder last: parent, number
    /// and transaction count, which survive the header normalization.
    last_built: std::sync::Mutex<Option<(alloy_primitives::B256, u64, usize)>>,
    /// Profile 1: the deferred proposal check (design 3.4).
    deferred: Option<super::deferred::DeferredChecker>,
    /// Range catch-up: sender verification ahead of the engine.
    sender_warmup: Option<std::sync::Arc<n42_execution::SenderWarmup>>,
}

impl RethExecutionLayer {
    /// Full adapter (leader/follower): can build payloads.
    pub fn new(
        engine: ConsensusEngineHandle<N42EngineTypes>,
        payload_builder: PayloadBuilderHandle<N42EngineTypes>,
    ) -> Self {
        Self {
            engine,
            payload_builder: Some(payload_builder),
            import_gate: super::import_gate::ImportGate::configured().map(std::sync::Arc::new),
            follower_import: None,
            last_built: std::sync::Mutex::new(None),
            deferred: None,
            sender_warmup: None,
        }
    }

    /// Import-only adapter (observer): `resolve_payload` is never called.
    pub fn engine_only(engine: ConsensusEngineHandle<N42EngineTypes>) -> Self {
        Self {
            engine,
            payload_builder: None,
            import_gate: super::import_gate::ImportGate::configured().map(std::sync::Arc::new),
            follower_import: None,
            last_built: std::sync::Mutex::new(None),
            deferred: None,
            sender_warmup: None,
        }
    }
}

impl RethExecutionLayer {
    /// Executes other nodes' blocks with the parallel transfer batches
    /// before the engine validates them (`N42_PARALLEL_IMPORT=1`).
    ///
    /// Imports are then kept in order here: one at a time per block, and a
    /// block's behind its parent's. The engine validates one block at a
    /// time whatever is queued for it, so nothing is delayed by the order;
    /// without it a block's execution would start before the engine has its
    /// parent, and the forkchoice update of a block would queue behind the
    /// payloads of its descendants.
    pub fn with_follower_import(mut self, follower_import: Option<super::FollowerImport>) -> Self {
        if follower_import.is_some() && self.import_gate.is_none() {
            self.import_gate = Some(Default::default());
        }
        self.follower_import = follower_import;
        self
    }

    /// Profile 1: checks proposals before their import (design 3.4).
    pub fn with_deferred_check(
        mut self,
        checker: Option<super::deferred::DeferredChecker>,
    ) -> Self {
        self.deferred = checker;
        self
    }

    /// Range catch-up: verifies senders ahead of the engine into its caches.
    pub fn with_sender_warmup(
        mut self,
        warmup: Option<std::sync::Arc<n42_execution::SenderWarmup>>,
    ) -> Self {
        self.sender_warmup = warmup;
        self
    }

    /// Returns once no import of `parent` is under way, when imports are
    /// kept in order.
    async fn parent_imported(&self, parent: alloy_primitives::B256) {
        if let Some(gate) = self
            .import_gate
            .as_ref()
            .filter(|_| self.follower_import.is_some())
        {
            let started = std::time::Instant::now();
            gate.settled(parent).await;
            record_parent_import_wait(started);
        }
    }
}

/// How long an import stood behind its parent's.
fn record_parent_import_wait(started: std::time::Instant) {
    metrics::histogram!("n42_engine_import_parent_wait_ms")
        .record(started.elapsed().as_secs_f64() * 1_000.0);
}

#[async_trait::async_trait]
impl ExecutionLayer for RethExecutionLayer {
    async fn new_payload(&self, payload: ExecutionData) -> Result<PayloadStatus, ElError> {
        let _import_guard = match &self.import_gate {
            Some(gate) => Some(gate.acquire(payload.block_hash()).await),
            None => None,
        };
        let parent = payload.parent_hash();
        // A block this node just built goes into the engine tree as executed.
        if super::own_block::enabled() && super::own_block::try_insert(&payload).await {
            return Ok(PayloadStatus::new(
                alloy_rpc_types_engine::PayloadStatusEnum::Valid,
                Some(payload.block_hash()),
            ));
        }
        // Another node's block: its transfers run in batches here, and the
        // engine checks the output instead of executing the block.
        let own = self.last_built.lock().is_ok_and(|built| {
            *built
                == Some((
                    payload.parent_hash(),
                    payload.block_number(),
                    payload.payload.transactions().len(),
                ))
        });
        if let Some(follower_import) = self.follower_import.as_ref().filter(|_| !own) {
            let prepared = {
                let (follower_import, payload) = (follower_import.clone(), payload.clone());
                let gate = self.import_gate.clone();
                tokio::task::spawn_blocking(move || {
                    follower_import.prepare_after(&payload, || {
                        if let Some(gate) = gate {
                            let started = std::time::Instant::now();
                            gate.settled_blocking(parent);
                            record_parent_import_wait(started);
                        }
                    })
                })
                .await
                .unwrap_or(false)
            };
            if prepared {
                return super::follower_import::submit_prepared(payload.block_hash(), || async {
                    self.engine
                        .new_payload(payload.clone())
                        .await
                        .map_err(|e| ElError(e.to_string()))
                })
                .await;
            }
        }
        // consensus_loop::background_import / execution_bridge eager import.
        self.parent_imported(parent).await;
        self.engine
            .new_payload(payload)
            .await
            .map_err(|e| ElError(e.to_string()))
    }

    async fn fork_choice_updated(
        &self,
        state: ForkchoiceState,
    ) -> Result<ForkchoiceUpdated, ElError> {
        // consensus_loop::finalize_committed_block (the finalize/import FCU).
        let started = std::time::Instant::now();
        let result = self
            .engine
            .fork_choice_updated(state, None)
            .await
            .map_err(|e| ElError(e.to_string()));
        metrics::histogram!("n42_engine_boundary_duration_ms", "phase" => "canonical_update")
            .record(started.elapsed().as_secs_f64() * 1_000.0);
        result
    }

    async fn fork_choice_updated_with_attrs(
        &self,
        state: ForkchoiceState,
        attrs: PayloadAttributes,
    ) -> Result<ForkchoiceUpdated, ElError> {
        // execution_bridge::do_trigger_payload_build (FCU-with-attributes start).
        self.engine
            .fork_choice_updated(state, Some(attrs))
            .await
            .map_err(|e| ElError(e.to_string()))
    }

    async fn resolve_payload(
        &self,
        id: PayloadId,
        kind: ResolveKind,
    ) -> Option<Result<BuiltBlock, ElError>> {
        // execution_bridge::spawn_payload_resolve_task (resolve_kind WaitForPending).
        let reth_kind = match kind {
            ResolveKind::WaitForPending => PayloadKind::WaitForPending,
        };
        let builder = self.payload_builder.as_ref()?;
        let started = std::time::Instant::now();
        let result = builder.resolve_kind(id, reth_kind).await.map(|r| {
            r.map_err(|e| ElError(e.to_string())).map(|payload| {
                let block = payload.block();
                if let Ok(mut built) = self.last_built.lock() {
                    *built = Some((
                        block.header().parent_hash,
                        block.header().number,
                        block.body().transactions.len(),
                    ));
                }
                super::own_block::remember_built(payload.block_arc());
                let convert_started = std::time::Instant::now();
                let built = to_built_block(payload);
                metrics::histogram!("n42_engine_boundary_duration_ms", "phase" => "payload_convert")
                        .record(convert_started.elapsed().as_secs_f64() * 1_000.0);
                built
            })
        });
        // Includes waiting for the pending build and converting its completed
        // output. This is a builder boundary, not a consensus seal timestamp.
        metrics::histogram!("n42_engine_boundary_duration_ms", "phase" => "payload_resolve")
            .record(started.elapsed().as_secs_f64() * 1_000.0);
        result
    }

    async fn deferred_check(
        &self,
        payload: &ExecutionData,
    ) -> n42_consensus_service::deferred_check::DeferredCheck {
        let Some(checker) = self.deferred.clone() else {
            return n42_consensus_service::deferred_check::DeferredCheck::Unsupported;
        };
        let payload = payload.clone();
        let queued = std::time::Instant::now();
        tokio::task::spawn_blocking(move || {
            // Time spent waiting for a blocking thread.
            metrics::histogram!("n42_deferred_check_stage_ms", "stage" => "queue")
                .record(queued.elapsed().as_secs_f64() * 1000.0);
            checker.check(&payload)
        })
        .await
        .unwrap_or_else(|error| {
            n42_consensus_service::deferred_check::DeferredCheck::Retry(error.to_string())
        })
    }

    fn sender_warmup(&self) -> Option<std::sync::Arc<n42_execution::SenderWarmup>> {
        self.sender_warmup.clone()
    }

    async fn build_on_sealed(
        &self,
        parent: alloy_primitives::B256,
        attributes: PayloadAttributes,
    ) -> Option<Result<BuiltBlock, ElError>> {
        // The block's output is published by its own insert.
        if !crate::build_on_output::enabled()
            || !super::own_block::enabled()
            || self.payload_builder.is_none()
        {
            return None;
        }
        let started = std::time::Instant::now();
        let built = tokio::task::spawn_blocking(move || {
            crate::build_on_output::build(parent, attributes, crate::build_on_output::SEALED_WAIT)
        })
        .await
        .unwrap_or_else(|error| Err(format!("build on a sealed block panicked: {error}")));
        let outcome = if built.is_ok() { "built" } else { "failed" };
        metrics::counter!("n42_build_on_output_total", "outcome" => outcome).increment(1);
        tracing::info!(
            target: "n42::el",
            %parent,
            outcome,
            reason = built.as_ref().err().map(String::as_str).unwrap_or(""),
            txs = built.as_ref().map_or(0, |payload| payload.block().body().transactions.len()),
            elapsed_ms = started.elapsed().as_secs_f64() * 1_000.0,
            "N42_BUILD_ON_OUTPUT: child of the sealed own block"
        );
        metrics::histogram!("n42_engine_boundary_duration_ms", "phase" => "build_on_sealed")
            .record(started.elapsed().as_secs_f64() * 1_000.0);
        Some(built.map_err(ElError).map(|payload| {
            let block = payload.block();
            if let Ok(mut last) = self.last_built.lock() {
                *last = Some((
                    block.header().parent_hash,
                    block.header().number,
                    block.body().transactions.len(),
                ));
            }
            super::own_block::remember_built(payload.block_arc());
            to_built_block(payload)
        }))
    }
}

/// Converts `N42BuiltPayload` into the node-neutral [`BuiltBlock`] — all
/// reth block introspection (header/body/blob hashes + `block_to_payload`) lives
/// here so the orchestrator and the `ExecutionLayer` trait stay reth-free.
fn to_built_block(payload: N42BuiltPayload) -> BuiltBlock {
    let block = payload.block();
    let blob_tx_hashes: Vec<_> = block
        .body()
        .transactions()
        .filter(|tx| tx.is_eip4844())
        .map(|tx| *tx.tx_hash())
        .collect();
    let execution_data = if blob_tx_hashes.is_empty() {
        crate::engine_types::blobless_block_to_payload(block)
    } else {
        <N42EngineTypes as PayloadTypes>::block_to_payload(block.clone(), None)
    };
    BuiltBlock {
        hash: block.hash(),
        number: block.header().number,
        timestamp: block.header().timestamp,
        tx_count: block.body().transactions().count(),
        execution_data,
        blob_tx_hashes,
    }
}
