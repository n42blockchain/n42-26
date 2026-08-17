//! In-process [`ExecutionLayer`] adapter over reth's engine + payload-builder
//! handles — the single implementation today. Each method delegates 1:1 to the
//! call the orchestrator makes inline today, erasing the reth error type.

use super::{BuiltBlock, ElError, ExecutionLayer, ResolveKind};
use alloy_consensus::Typed2718;
use alloy_primitives::B256;
use alloy_rpc_types_engine::{
    ExecutionData, ForkchoiceState, ForkchoiceUpdated, PayloadAttributes, PayloadId, PayloadStatus,
    PayloadStatusEnum,
};
use reth_ethereum_engine_primitives::EthEngineTypes;
use reth_node_builder::ConsensusEngineHandle;
use reth_payload_builder::PayloadBuilderHandle;
use reth_payload_primitives::{PayloadKind, PayloadTypes};
use std::collections::VecDeque;
use std::sync::Mutex;

const VALIDATED_PAYLOAD_TIMESTAMP_CACHE_CAPACITY: usize = 256;

/// Wraps reth's `ConsensusEngineHandle` + (optional) `PayloadBuilderHandle`. Both
/// handles are cheaply cloneable (channel senders), so the adapter is cheap to
/// share via `Arc`. The payload builder is optional because non-producing roles
/// (the observer) only need `new_payload`/`fork_choice_updated`, never builds.
pub struct RethExecutionLayer {
    engine: ConsensusEngineHandle<EthEngineTypes>,
    payload_builder: Option<PayloadBuilderHandle<EthEngineTypes>>,
    validated_payload_timestamps: Mutex<VecDeque<(B256, u64)>>,
}

impl RethExecutionLayer {
    /// Full adapter (leader/follower): can build payloads.
    pub fn new(
        engine: ConsensusEngineHandle<EthEngineTypes>,
        payload_builder: PayloadBuilderHandle<EthEngineTypes>,
    ) -> Self {
        Self {
            engine,
            payload_builder: Some(payload_builder),
            validated_payload_timestamps: Mutex::new(VecDeque::new()),
        }
    }

    /// Import-only adapter (observer): `resolve_payload` is never called.
    pub fn engine_only(engine: ConsensusEngineHandle<EthEngineTypes>) -> Self {
        Self {
            engine,
            payload_builder: None,
            validated_payload_timestamps: Mutex::new(VecDeque::new()),
        }
    }

    fn record_validated_payload_timestamp(&self, hash: B256, timestamp: u64) {
        let mut timestamps = self
            .validated_payload_timestamps
            .lock()
            .expect("validated payload timestamp cache lock");
        if let Some(index) = timestamps
            .iter()
            .position(|(cached_hash, _)| *cached_hash == hash)
        {
            timestamps.remove(index);
        }
        timestamps.push_back((hash, timestamp));
        while timestamps.len() > VALIDATED_PAYLOAD_TIMESTAMP_CACHE_CAPACITY {
            timestamps.pop_front();
        }
    }
}

#[async_trait::async_trait]
impl ExecutionLayer for RethExecutionLayer {
    async fn new_payload(&self, payload: ExecutionData) -> Result<PayloadStatus, ElError> {
        // consensus_loop::background_import / execution_bridge eager import.
        let hash = payload.block_hash();
        let timestamp = payload.payload.timestamp();
        let result = self
            .engine
            .new_payload(payload)
            .await
            .map_err(|e| ElError(e.to_string()))?;
        if matches!(result.status, PayloadStatusEnum::Valid) {
            self.record_validated_payload_timestamp(hash, timestamp);
        }
        Ok(result)
    }

    async fn fork_choice_updated(
        &self,
        state: ForkchoiceState,
    ) -> Result<ForkchoiceUpdated, ElError> {
        // consensus_loop::finalize_committed_block (the finalize/import FCU).
        self.engine
            .fork_choice_updated(state, None)
            .await
            .map_err(|e| ElError(e.to_string()))
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
        let result = self
            .payload_builder
            .as_ref()?
            .resolve_kind(id, reth_kind)
            .await
            .map(|r| r.map_err(|e| ElError(e.to_string())).map(to_built_block));
        if let Some(Ok(ref built)) = result {
            self.record_validated_payload_timestamp(built.hash, built.timestamp);
        }
        result
    }

    fn validated_payload_timestamp(&self, hash: B256) -> Option<u64> {
        self.validated_payload_timestamps
            .lock()
            .expect("validated payload timestamp cache lock")
            .iter()
            .rev()
            .find_map(|(cached_hash, timestamp)| (*cached_hash == hash).then_some(*timestamp))
    }
}

/// Converts reth's `EthBuiltPayload` into the node-neutral [`BuiltBlock`] — all
/// reth block introspection (header/body/blob hashes + `block_to_payload`) lives
/// here so the orchestrator and the `ExecutionLayer` trait stay reth-free.
fn to_built_block(payload: reth_payload_builder::EthBuiltPayload) -> BuiltBlock {
    let block = payload.block();
    let blob_tx_hashes = block
        .body()
        .transactions()
        .filter(|tx| tx.is_eip4844())
        .map(|tx| *tx.tx_hash())
        .collect();
    BuiltBlock {
        hash: block.hash(),
        number: block.header().number,
        timestamp: block.header().timestamp,
        tx_count: block.body().transactions().count(),
        execution_data: <EthEngineTypes as PayloadTypes>::block_to_payload(block.clone(), None),
        blob_tx_hashes,
    }
}
