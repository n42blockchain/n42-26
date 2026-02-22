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

impl ConsensusOrchestrator {
    /// Triggers payload building by calling fork_choice_updated with PayloadAttributes.
    ///
    /// When `slot_timestamp` is provided, it is used as the block's timestamp
    /// for wall-clock-aligned slot timing. Otherwise uses the current system time.
    ///
    /// A spawned task resolves the payload, inserts it via new_payload, and sends
    /// the block hash through `block_ready_rx` to feed the consensus engine.
    pub(super) async fn do_trigger_payload_build(&self, slot_timestamp: Option<u64>) {
        let beacon_engine = match &self.beacon_engine {
            Some(e) => e,
            None => {
                debug!("no beacon engine configured, skipping payload build");
                return;
            }
        };
        let payload_builder = match &self.payload_builder {
            Some(pb) => pb.clone(),
            None => {
                debug!("no payload builder configured, skipping payload build");
                return;
            }
        };

        let timestamp = slot_timestamp.unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
        });

        let attrs = PayloadAttributes {
            timestamp,
            prev_randao: B256::ZERO,
            suggested_fee_recipient: self.fee_recipient,
            withdrawals: Some(vec![]),
            parent_beacon_block_root: Some(B256::ZERO),
        };

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
            tokio::time::sleep(Duration::from_millis(100)).await;

            let resolve_result = tokio::time::timeout(
                Duration::from_secs(30),
                payload_builder.resolve_kind(payload_id, PayloadKind::WaitForPending),
            )
            .await;

            let payload_opt = match resolve_result {
                Ok(result) => result,
                Err(_) => {
                    error!("payload build timed out after 30s");
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

    /// Handles incoming block data from the leader via /n42/blocks/1 topic.
    ///
    /// Supports two arrival orders:
    ///   1. ExecuteBlock(hash) arrives first — data cached, imported on arrival.
    ///   2. BlockData arrives first — cached in pending_block_data, imported when ExecuteBlock arrives.
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

        // Bound cache to 16 entries; evict oldest on overflow.
        if self.pending_block_data.len() >= 16 {
            if let Some(old_key) = self.pending_block_data.keys().next().copied() {
                self.pending_block_data.remove(&old_key);
            }
        }
        self.pending_block_data.insert(hash, data);

        // Speculative pre-import: call new_payload immediately so the block is in reth's
        // database when the Proposal arrives. This eliminates new_payload latency from
        // the critical voting path.
        debug!(%hash, "speculative pre-import");
        self.import_and_notify(broadcast).await;
    }

    /// Handles incoming blob sidecar data from the leader via /n42/blobs/1 topic.
    ///
    /// Inserts each sidecar into the local DiskFileBlobStore for EIP-4844 compatibility.
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

    /// Imports a block into reth via new_payload, advances the chain head, and notifies
    /// the consensus engine. On Syncing status, queues the block for retry.
    pub(super) async fn import_and_notify(&mut self, broadcast: BlockDataBroadcast) {
        let engine_handle = match self.beacon_engine {
            Some(ref h) => h.clone(),
            None => return,
        };

        let execution_data = match serde_json::from_slice(&broadcast.payload_json) {
            Ok(data) => data,
            Err(e) => {
                warn!(hash = %broadcast.block_hash, "failed to deserialize execution payload: {e}");
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
        // Cross-check engine's validated hash to detect substitution attacks.
        if let Some(ref valid_hash) = status.latest_valid_hash {
            if *valid_hash != broadcast.block_hash {
                warn!(
                    expected = %broadcast.block_hash,
                    engine_hash = %valid_hash,
                    "block hash mismatch between broadcast and engine, skipping"
                );
                return;
            }
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

        if let Some(ref tx) = self.mobile_packet_tx {
            let _ = tx.try_send((broadcast.block_hash, deferred_view));
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
            if self.syncing_blocks.len() >= 8 {
                self.syncing_blocks.pop_front();
            }
            self.syncing_blocks.push_back(data);
        }
    }

    async fn retry_syncing_blocks(&mut self, engine_handle: &ConsensusEngineHandle<EthEngineTypes>) {
        let queued: Vec<Vec<u8>> = self.syncing_blocks.drain(..).collect();
        info!(count = queued.len(), "retrying previously-syncing blocks");

        for data in queued {
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
                    debug!(%retry_hash, "retry still Syncing, re-queuing");
                    self.syncing_blocks.push_back(data);
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

                // Send BlockReady after broadcasting block data — followers get a head start
                // on pre-import before the Proposal arrives via the consensus channel.
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
