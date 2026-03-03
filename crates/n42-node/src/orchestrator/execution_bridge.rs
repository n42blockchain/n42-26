use super::{BlobSidecarBroadcast, BlockDataBroadcast, ConsensusOrchestrator};
use alloy_consensus::Typed2718;
use alloy_eips::eip7594::BlobTransactionSidecarVariant;
use alloy_primitives::B256;
use alloy_rlp::Encodable;
use alloy_rpc_types_engine::{ForkchoiceState, PayloadAttributes, PayloadStatusEnum};
use n42_consensus::ConsensusEvent;
use n42_network::NetworkHandle;
use reth_ethereum_engine_primitives::EthEngineTypes;
use reth_node_builder::ConsensusEngineHandle;
use reth_payload_builder::{EthBuiltPayload, PayloadBuilderHandle, PayloadId};
use reth_payload_primitives::{EngineApiMessageVersion, PayloadKind, PayloadTypes};
use reth_transaction_pool::blobstore::{BlobStore, DiskFileBlobStore};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

/// Delay before resolving the built payload, allowing the builder to pack transactions.
const BUILDER_WARMUP_DELAY: Duration = Duration::from_millis(100);

/// Maximum time to wait for a payload build to complete.
const PAYLOAD_BUILD_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum number of cached pending block data entries.
const MAX_PENDING_BLOCK_DATA: usize = 16;

/// Maximum number of blocks in the syncing retry queue.
const MAX_SYNCING_QUEUE_SIZE: usize = 8;

impl ConsensusOrchestrator {
    /// Builds `PayloadAttributes` with timestamp correction and reward withdrawal injection.
    fn build_payload_attributes(&mut self, slot_timestamp: Option<u64>) -> PayloadAttributes {
        let mut timestamp = slot_timestamp.unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
        });

        // Engine API requires: payload_attributes.timestamp > head_block.timestamp.
        // Without this guard, fast block production (slot_time=0, or single-node f=0)
        // can produce two blocks within the same wall-clock second, causing
        // "Invalid payload attributes: invalid timestamp" errors.
        if timestamp <= self.last_committed_timestamp {
            let bumped = self.last_committed_timestamp + 1;
            warn!(
                proposed = timestamp,
                last_committed = self.last_committed_timestamp,
                bumped_to = bumped,
                "timestamp <= last committed block, bumping to avoid Engine API rejection"
            );
            timestamp = bumped;
        }

        let mut withdrawals = vec![];
        if let Some(ref reward_mgr) = self.mobile_reward_manager {
            let mut mgr = reward_mgr.lock().unwrap_or_else(|e| {
                error!("mobile_reward_manager mutex poisoned: {e}");
                e.into_inner()
            });
            let next_block_number = self.committed_block_count + 1;
            mgr.check_epoch_boundary(next_block_number);
            withdrawals = mgr.take_pending_rewards(next_block_number);
            if !withdrawals.is_empty() {
                info!(
                    target: "n42::reward",
                    count = withdrawals.len(),
                    "injecting mobile rewards as withdrawals"
                );
            }
        }

        PayloadAttributes {
            timestamp,
            prev_randao: B256::ZERO,
            suggested_fee_recipient: self.fee_recipient,
            withdrawals: Some(withdrawals),
            parent_beacon_block_root: Some(B256::ZERO),
        }
    }

    /// Triggers payload building via fork_choice_updated, then spawns a task to resolve it.
    pub(super) async fn do_trigger_payload_build(&mut self, slot_timestamp: Option<u64>) {
        if self.beacon_engine.is_none() {
            debug!("no beacon engine configured, skipping payload build");
            return;
        }
        let payload_builder = match &self.payload_builder {
            Some(pb) => pb.clone(),
            None => {
                debug!("no payload builder configured, skipping payload build");
                return;
            }
        };

        // Build attrs before borrowing beacon_engine to avoid borrow conflict.
        let attrs = self.build_payload_attributes(slot_timestamp);
        let beacon_engine = self.beacon_engine.as_ref().unwrap();
        let timestamp = attrs.timestamp;

        let fcu_state = ForkchoiceState {
            head_block_hash: self.head_block_hash,
            safe_block_hash: self.head_block_hash,
            finalized_block_hash: self.head_block_hash,
        };

        info!(head = %self.head_block_hash, timestamp, "triggering payload build via fork_choice_updated");

        match beacon_engine
            .fork_choice_updated(fcu_state, Some(attrs), EngineApiMessageVersion::default())
            .await
        {
            Ok(result) => {
                debug!(status = ?result.payload_status.status, "fork_choice_updated response");
                if let Some(payload_id) = result.payload_id {
                    // Record the timestamp we used so subsequent builds guarantee
                    // strictly increasing timestamps even in fast-commit scenarios.
                    self.last_committed_timestamp = self.last_committed_timestamp.max(timestamp);
                    info!(?payload_id, "payload building started, spawning resolve task");
                    self.spawn_payload_resolve_task(
                        beacon_engine.clone(),
                        payload_builder,
                        payload_id,
                    );
                } else {
                    warn!("fork_choice_updated did not return payload_id");
                }
            }
            Err(e) => {
                error!(error = %e, "fork_choice_updated failed");
            }
        }
    }

    fn spawn_payload_resolve_task(
        &self,
        engine_handle: ConsensusEngineHandle<EthEngineTypes>,
        payload_builder: PayloadBuilderHandle<EthEngineTypes>,
        payload_id: PayloadId,
    ) {
        let block_ready_tx = self.block_ready_tx.clone();
        let network = self.network.clone();
        let leader_payload_tx = self.leader_payload_tx.clone();
        let current_view = self.engine.current_view();
        let blob_store = self.blob_store.clone();

        tokio::spawn(async move {
            // Allow builder time to pack transactions from the pool.
            tokio::time::sleep(BUILDER_WARMUP_DELAY).await;

            let resolve_result = tokio::time::timeout(
                PAYLOAD_BUILD_TIMEOUT,
                payload_builder.resolve_kind(payload_id, PayloadKind::WaitForPending),
            )
            .await;

            let payload_opt = match resolve_result {
                Ok(result) => result,
                Err(_) => {
                    error!("payload build timed out after {}s", PAYLOAD_BUILD_TIMEOUT.as_secs());
                    return;
                }
            };

            match payload_opt {
                Some(Ok(payload)) => {
                    handle_built_payload(
                        payload,
                        engine_handle,
                        network,
                        block_ready_tx,
                        leader_payload_tx,
                        current_view,
                        blob_store,
                    )
                    .await;
                }
                Some(Err(e)) => error!(error = %e, "payload build failed"),
                None => warn!("payload not found (already resolved or expired)"),
            }
        });
    }

    /// Handles incoming block data from the leader, caching and pre-importing.
    pub(super) async fn handle_block_data(&mut self, data: Vec<u8>) {
        debug!(bytes = data.len(), "handle_block_data called");
        let broadcast: BlockDataBroadcast = match bincode::deserialize(&data) {
            Ok(b) => b,
            Err(e) => {
                warn!("invalid block data broadcast: {e}");
                return;
            }
        };

        let hash = broadcast.block_hash;
        self.pending_executions.remove(&hash);

        if self.pending_block_data.len() >= MAX_PENDING_BLOCK_DATA
            && let Some(old_key) = self.pending_block_data.keys().next().copied()
        {
            self.pending_block_data.remove(&old_key);
        }
        self.pending_block_data.insert(hash, data);

        // Pre-import so the block is in reth when the Proposal arrives.
        debug!(%hash, "speculative pre-import");
        self.import_and_notify(broadcast).await;
    }

    pub(super) fn handle_blob_sidecar(&self, data: Vec<u8>) {
        let blob_store = match &self.blob_store {
            Some(bs) => bs,
            None => return,
        };

        let broadcast: BlobSidecarBroadcast = match bincode::deserialize(&data) {
            Ok(b) => b,
            Err(e) => {
                warn!(error = %e, "invalid blob sidecar broadcast");
                return;
            }
        };

        let sidecar_count = broadcast.sidecars.len();
        for (tx_hash, sidecar_rlp) in broadcast.sidecars {
            match <BlobTransactionSidecarVariant as alloy_rlp::Decodable>::decode(
                &mut &sidecar_rlp[..],
            ) {
                Ok(sidecar) => {
                    if let Err(e) = blob_store.insert(tx_hash, sidecar) {
                        debug!(%tx_hash, error = %e, "failed to insert blob sidecar");
                    }
                }
                Err(e) => {
                    warn!(%tx_hash, error = %e, "failed to decode blob sidecar RLP");
                }
            }
        }

        debug!(
            block_hash = %broadcast.block_hash,
            sidecars = sidecar_count,
            "processed blob sidecar broadcast"
        );
    }

    /// Imports a block via new_payload; queues for retry on Syncing status.
    pub(super) async fn import_and_notify(&mut self, broadcast: BlockDataBroadcast) {
        let engine_handle = match self.beacon_engine {
            Some(ref h) => h.clone(),
            None => return,
        };

        // Parse once as Value, extract timestamp, then deserialize into the typed struct.
        let parsed_value: serde_json::Value = match serde_json::from_slice(&broadcast.payload_json) {
            Ok(v) => v,
            Err(e) => {
                warn!(hash = %broadcast.block_hash, "failed to deserialize execution payload: {e}");
                return;
            }
        };

        // Extract the block timestamp to keep last_committed_timestamp up to date
        // for followers as well (prevents stale timestamp on leader switch).
        if let Some(ts_hex) = parsed_value.get("timestamp").and_then(|t| t.as_str())
            && let Ok(ts) = u64::from_str_radix(ts_hex.trim_start_matches("0x"), 16)
        {
            self.last_committed_timestamp = self.last_committed_timestamp.max(ts);
        }

        let execution_data = match serde_json::from_value(parsed_value) {
            Ok(data) => data,
            Err(e) => {
                warn!(hash = %broadcast.block_hash, "failed to convert execution payload from Value: {e}");
                return;
            }
        };

        match engine_handle.new_payload(execution_data).await {
            Ok(status) => {
                if matches!(status.status, PayloadStatusEnum::Valid | PayloadStatusEnum::Accepted) {
                    self.handle_valid_import(&broadcast, &engine_handle, &status).await;
                } else if matches!(status.status, PayloadStatusEnum::Syncing) {
                    self.queue_syncing_block(&broadcast);
                } else {
                    warn!(
                        hash = %broadcast.block_hash,
                        status = ?status.status,
                        "new_payload rejected block"
                    );
                }
            }
            Err(e) => {
                error!(hash = %broadcast.block_hash, error = %e, "new_payload failed");
            }
        }
    }

    async fn handle_valid_import(
        &mut self,
        broadcast: &BlockDataBroadcast,
        engine_handle: &ConsensusEngineHandle<EthEngineTypes>,
        status: &alloy_rpc_types_engine::PayloadStatus,
    ) {
        if let Some(ref valid_hash) = status.latest_valid_hash
            && *valid_hash != broadcast.block_hash
        {
            warn!(
                expected = %broadcast.block_hash,
                engine_hash = %valid_hash,
                "block hash mismatch between broadcast and engine, skipping"
            );
            return;
        }

        info!(hash = %broadcast.block_hash, "block imported from leader");

        let fcu_state = ForkchoiceState {
            head_block_hash: broadcast.block_hash,
            safe_block_hash: broadcast.block_hash,
            finalized_block_hash: broadcast.block_hash,
        };
        if let Err(e) = engine_handle
            .fork_choice_updated(fcu_state, None, EngineApiMessageVersion::default())
            .await
        {
            error!(
                hash = %broadcast.block_hash,
                error = %e,
                "fork_choice_updated failed for imported block"
            );
        }

        self.head_block_hash = broadcast.block_hash;
        self.complete_deferred_finalization(broadcast).await;

        if let Err(e) = self
            .engine
            .process_event(ConsensusEvent::BlockImported(broadcast.block_hash))
        {
            error!(error = %e, "error processing BlockImported");
        }

        if !self.syncing_blocks.is_empty() {
            self.retry_syncing_blocks(engine_handle).await;
        }
    }

    pub(super) async fn complete_deferred_finalization(&mut self, broadcast: &BlockDataBroadcast) {
        let deferred_view = match &self.pending_finalization {
            Some(pf) if pf.block_hash == broadcast.block_hash => pf.view,
            _ => return,
        };

        info!(
            view = deferred_view,
            hash = %broadcast.block_hash,
            "completing deferred finalization"
        );
        self.pending_finalization = None;
        self.pending_block_data.clear();
        self.pending_executions.clear();

        if let Some(ref tx) = self.mobile_packet_tx
            && let Err(e) = tx.try_send((broadcast.block_hash, deferred_view))
        {
            warn!(view = deferred_view, hash = %broadcast.block_hash, error = %e, "mobile_packet_tx full or closed, deferred verification packet lost");
        }

        if self.engine.is_current_leader() {
            info!(
                next_view = self.engine.current_view(),
                "leader for next view, triggering payload build"
            );
            self.schedule_payload_build().await;
        }
    }

    fn queue_syncing_block(&mut self, broadcast: &BlockDataBroadcast) {
        info!(hash = %broadcast.block_hash, "new_payload returned Syncing, queuing for retry");
        if let Ok(data) = bincode::serialize(broadcast) {
            if self.syncing_blocks.len() >= MAX_SYNCING_QUEUE_SIZE {
                self.syncing_blocks.pop_front();
            }
            self.syncing_blocks.push_back((data, 0));
        }
    }

    async fn retry_syncing_blocks(&mut self, engine_handle: &ConsensusEngineHandle<EthEngineTypes>) {
        let queued: Vec<(Vec<u8>, u32)> = self.syncing_blocks.drain(..).collect();
        info!(count = queued.len(), "retrying previously-syncing blocks");

        const MAX_SYNCING_RETRIES: u32 = 3;

        for (data, retry_count) in queued {
            let retry_broadcast = match bincode::deserialize::<BlockDataBroadcast>(&data) {
                Ok(b) => b,
                Err(_) => continue,
            };
            let retry_hash = retry_broadcast.block_hash;
            let retry_exec = match serde_json::from_slice(&retry_broadcast.payload_json) {
                Ok(d) => d,
                Err(e) => {
                    warn!(%retry_hash, error = %e, "failed to deserialize retry payload");
                    continue;
                }
            };

            match engine_handle.new_payload(retry_exec).await {
                Ok(rs)
                    if matches!(
                        rs.status,
                        PayloadStatusEnum::Valid | PayloadStatusEnum::Accepted
                    ) =>
                {
                    info!(%retry_hash, "syncing block retry succeeded");
                    let fcu = ForkchoiceState {
                        head_block_hash: retry_hash,
                        safe_block_hash: retry_hash,
                        finalized_block_hash: retry_hash,
                    };
                    let _ = engine_handle
                        .fork_choice_updated(fcu, None, EngineApiMessageVersion::default())
                        .await;
                    self.head_block_hash = retry_hash;
                    if let Err(e) = self
                        .engine
                        .process_event(ConsensusEvent::BlockImported(retry_hash))
                    {
                        error!(error = %e, "error processing BlockImported for retry");
                    }
                }
                Ok(rs) if matches!(rs.status, PayloadStatusEnum::Syncing) => {
                    let next_retry = retry_count + 1;
                    if next_retry >= MAX_SYNCING_RETRIES {
                        warn!(%retry_hash, retries = next_retry, "syncing block exceeded max retries, dropping");
                    } else {
                        debug!(%retry_hash, retry = next_retry, "retry still Syncing, re-queuing");
                        self.syncing_blocks.push_back((data, next_retry));
                    }
                }
                Ok(rs) => {
                    warn!(%retry_hash, status = ?rs.status, "retry rejected");
                }
                Err(e) => {
                    warn!(%retry_hash, error = %e, "retry new_payload failed");
                }
            }
        }
    }
}

// ── Free functions for the spawned payload build task ──

async fn handle_built_payload(
    payload: EthBuiltPayload,
    engine_handle: ConsensusEngineHandle<EthEngineTypes>,
    network: NetworkHandle,
    block_ready_tx: mpsc::UnboundedSender<B256>,
    leader_payload_tx: mpsc::UnboundedSender<(B256, Vec<u8>)>,
    current_view: u64,
    blob_store: Option<DiskFileBlobStore>,
) {
    let hash = payload.block().hash();

    let execution_data =
        <EthEngineTypes as PayloadTypes>::block_to_payload(payload.block().clone());

    let payload_json = match serde_json::to_vec(&execution_data) {
        Ok(json) => json,
        Err(e) => {
            error!(%hash, error = %e, "CRITICAL: failed to serialize execution payload");
            return;
        }
    };

    match engine_handle.new_payload(execution_data).await {
        Ok(status) => match status.status {
            PayloadStatusEnum::Valid | PayloadStatusEnum::Accepted => {
                info!(%hash, status = ?status.status, "newPayload valid, broadcasting block data");

                broadcast_block_data(&network, &leader_payload_tx, hash, current_view, &payload_json);

                // BlockReady after broadcast: followers pre-import before Proposal arrives.
                let _ = block_ready_tx.send(hash);

                broadcast_blob_sidecars(&network, &payload, hash, current_view, blob_store);
            }
            _ => {
                error!(%hash, status = ?status.status, "newPayload returned non-valid status");
            }
        },
        Err(e) => {
            error!(%hash, error = %e, "newPayload failed");
        }
    }
}

fn broadcast_block_data(
    network: &NetworkHandle,
    leader_payload_tx: &mpsc::UnboundedSender<(B256, Vec<u8>)>,
    hash: B256,
    current_view: u64,
    payload_json: &[u8],
) {
    if payload_json.is_empty() {
        return;
    }
    let broadcast = BlockDataBroadcast {
        block_hash: hash,
        view: current_view,
        payload_json: payload_json.to_vec(),
    };
    match bincode::serialize(&broadcast) {
        Ok(encoded) => {
            info!(%hash, bytes = encoded.len(), "broadcasting block data to followers");
            if let Err(e) = network.announce_block(encoded.clone()) {
                warn!(error = %e, "failed to broadcast block data");
            }
            let _ = leader_payload_tx.send((hash, encoded));
        }
        Err(e) => {
            error!(error = %e, "failed to serialize block data broadcast");
        }
    }
}

fn broadcast_blob_sidecars(
    network: &NetworkHandle,
    payload: &EthBuiltPayload,
    hash: B256,
    current_view: u64,
    blob_store: Option<DiskFileBlobStore>,
) {
    let blob_store = match blob_store {
        Some(bs) => bs,
        None => return,
    };

    let blob_tx_hashes: Vec<B256> = payload
        .block()
        .body()
        .transactions()
        .filter(|tx| tx.is_eip4844())
        .map(|tx| *tx.tx_hash())
        .collect();

    if blob_tx_hashes.is_empty() {
        return;
    }

    match blob_store.get_all(blob_tx_hashes) {
        Ok(sidecars) if !sidecars.is_empty() => {
            let encoded_sidecars: Vec<(B256, Vec<u8>)> = sidecars
                .into_iter()
                .map(|(tx_hash, sidecar)| {
                    let mut buf = Vec::new();
                    sidecar.encode(&mut buf);
                    (tx_hash, buf)
                })
                .collect();

            let sidecar_count = encoded_sidecars.len();
            let broadcast = BlobSidecarBroadcast {
                block_hash: hash,
                view: current_view,
                sidecars: encoded_sidecars,
            };

            if let Ok(encoded) = bincode::serialize(&broadcast) {
                info!(%hash, blob_count = sidecar_count, bytes = encoded.len(), "broadcasting blob sidecars");
                if let Err(e) = network.broadcast_blob_sidecar(encoded) {
                    warn!(error = %e, "failed to broadcast blob sidecars");
                }
            }
        }
        Ok(_) => {}
        Err(e) => {
            warn!(%hash, error = %e, "failed to get blob sidecars from store");
        }
    }
}
