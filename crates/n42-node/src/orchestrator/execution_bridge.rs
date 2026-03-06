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
const BUILDER_WARMUP_DELAY: Duration = Duration::from_millis(10);

/// Maximum time to wait for a payload build to complete.
const PAYLOAD_BUILD_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum number of cached pending block data entries.
pub(super) const MAX_PENDING_BLOCK_DATA: usize = 16;

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
                target: "n42::cl::exec_bridge",
                proposed = timestamp,
                last_committed = self.last_committed_timestamp,
                bumped_to = bumped,
                "timestamp <= last committed block, bumping to avoid Engine API rejection"
            );
            timestamp = bumped;
        }

        // 1. Pre-fetch staked BLS pubkeys (lock StakingManager, then release).
        //    This avoids holding both locks simultaneously and prevents deadlocks.
        let staked_pubkeys = if let Some(ref staking_mgr) = self.staking_manager {
            let mgr = staking_mgr.lock().unwrap_or_else(|e| e.into_inner());
            mgr.staked_bls_pubkeys()
        } else {
            std::collections::HashSet::new()
        };

        // 2. Compute mobile rewards (lock MobileRewardManager).
        let mut withdrawals = vec![];
        if let Some(ref reward_mgr) = self.mobile_reward_manager {
            let mut mgr = reward_mgr.lock().unwrap_or_else(|e| {
                error!(target: "n42::cl::exec_bridge", "mobile_reward_manager mutex poisoned: {e}");
                e.into_inner()
            });
            let next_block_number = self.committed_block_count + 1;
            mgr.check_epoch_boundary(next_block_number, &staked_pubkeys);
            withdrawals = mgr.take_pending_rewards(next_block_number);
            if !withdrawals.is_empty() {
                info!(
                    target: "n42::cl::exec_bridge",
                    count = withdrawals.len(),
                    "injecting mobile rewards as withdrawals"
                );
            }
        }

        // 3. Staking integration: resolve reward addresses and inject cooldown returns.
        //    Re-acquire StakingManager lock for address resolution and cooldown checks.
        if let Some(ref staking_mgr) = self.staking_manager {
            let mut staking = staking_mgr.lock().unwrap_or_else(|e| e.into_inner());

            // Reward address resolution: BLS-derived keccak → staker's actual EVM address.
            for w in &mut withdrawals {
                if let Some(addr) = staking.staker_address_by_reward(w.address) {
                    w.address = addr;
                }
            }

            // Cooldown expiration checks + return withdrawals.
            let next_block_number = self.committed_block_count + 1;
            staking.check_cooldowns(next_block_number);
            let returns = staking.take_pending_returns(next_block_number, 8);
            if !returns.is_empty() {
                info!(
                    target: "n42::cl::exec_bridge",
                    count = returns.len(),
                    "injecting staking returns as withdrawals"
                );
                withdrawals.extend(returns);
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
            debug!(target: "n42::cl::exec_bridge", "no beacon engine configured, skipping payload build");
            return;
        }
        let payload_builder = match &self.payload_builder {
            Some(pb) => pb.clone(),
            None => {
                debug!(target: "n42::cl::exec_bridge", "no payload builder configured, skipping payload build");
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

        debug!(target: "n42::cl::exec_bridge", head = %self.head_block_hash, timestamp, "triggering payload build via fork_choice_updated");

        // Try FCU; on "invalid payload attributes" (timestamp race), retry once with
        // a conservatively bumped timestamp.  This handles the edge case where
        // last_committed_timestamp doesn't perfectly track reth's internal head.
        let mut last_err = None;
        for attempt in 0..2u8 {
            let try_attrs = if attempt == 0 {
                attrs.clone()
            } else {
                // Retry: bump timestamp aggressively to guarantee > head.timestamp.
                // Use +2 because consecutive fast blocks bump by +1 each, and our
                // last_committed_timestamp tracking can be 1 behind the actual head.
                let bumped_ts = self.last_committed_timestamp.max(attrs.timestamp) + 2;
                warn!(target: "n42::cl::exec_bridge", bumped_ts, "retrying FCU with bumped timestamp");
                let mut retry_attrs = attrs.clone();
                retry_attrs.timestamp = bumped_ts;
                retry_attrs
            };
            let used_ts = try_attrs.timestamp;

            match beacon_engine
                .fork_choice_updated(fcu_state, Some(try_attrs), EngineApiMessageVersion::default())
                .await
            {
                Ok(result) => {
                    debug!(target: "n42::cl::exec_bridge", status = ?result.payload_status.status, "fork_choice_updated response");
                    if let Some(payload_id) = result.payload_id {
                        // Record the timestamp we used so subsequent builds guarantee
                        // strictly increasing timestamps even in fast-commit scenarios.
                        self.last_committed_timestamp = self.last_committed_timestamp.max(used_ts);
                        debug!(target: "n42::cl::exec_bridge", ?payload_id, "payload building started, spawning resolve task");
                        self.spawn_payload_resolve_task(
                            beacon_engine.clone(),
                            payload_builder,
                            payload_id,
                        );
                    } else {
                        warn!(target: "n42::cl::exec_bridge", "fork_choice_updated did not return payload_id");
                    }
                    last_err = None;
                    break;
                }
                Err(e) => {
                    if attempt == 0 {
                        warn!(target: "n42::cl::exec_bridge", error = %e, "fork_choice_updated failed, will retry with bumped timestamp");
                    }
                    last_err = Some(e);
                }
            }
        }
        if let Some(e) = last_err {
            error!(target: "n42::cl::exec_bridge", error = %e, "fork_choice_updated failed after retry");
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
        let eager_import_done_tx = self.eager_import_done_tx.clone();

        let handle = tokio::spawn(async move {
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
                    error!(target: "n42::cl::exec_bridge", "payload build timed out after {}s", PAYLOAD_BUILD_TIMEOUT.as_secs());
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
                        eager_import_done_tx,
                    )
                    .await;
                }
                Some(Err(e)) => error!(target: "n42::cl::exec_bridge", error = %e, "payload build failed"),
                None => warn!(target: "n42::cl::exec_bridge", "payload not found (already resolved or expired)"),
            }
        });

        // Monitor the JoinHandle so that panics/cancellations in the payload
        // resolve task are logged rather than silently swallowed.
        tokio::spawn(async move {
            if let Err(e) = handle.await {
                error!(
                    target: "n42::cl::exec_bridge",
                    error = %e,
                    "payload resolve task terminated unexpectedly (panic or cancellation)"
                );
            }
        });
    }

    /// Handles incoming block data from the leader.
    ///
    /// **Async execution optimization**: Instead of executing the block (new_payload)
    /// before voting, we immediately notify the consensus engine that block data is
    /// available.  This allows followers to vote without waiting for EVM execution.
    /// Actual execution is deferred to `finalize_committed_block()` (Case B).
    ///
    /// Safety: HotStuff-2 safety depends on the QC chain, not execution results.
    /// The leader already executed the block when building it.  If execution fails
    /// at finalization time, the node will detect the inconsistency and trigger sync.
    pub(super) async fn handle_block_data(&mut self, data: Vec<u8>) {
        debug!(target: "n42::cl::exec_bridge", bytes = data.len(), "handle_block_data called");
        let broadcast: BlockDataBroadcast = match bincode::deserialize(&data) {
            Ok(b) => b,
            Err(e) => {
                warn!(target: "n42::cl::exec_bridge", "invalid block data broadcast: {e}");
                return;
            }
        };

        let hash = broadcast.block_hash;
        self.pending_executions.remove(&hash);

        // Update timestamp tracking from the broadcast's direct field.
        if broadcast.timestamp > 0 {
            self.last_committed_timestamp = self.last_committed_timestamp.max(broadcast.timestamp);
        }

        if self.pending_block_data.len() >= MAX_PENDING_BLOCK_DATA
            && let Some(old_key) = self.pending_block_data.keys().next().copied()
        {
            self.pending_block_data.remove(&old_key);
        }
        self.pending_block_data.insert(hash, data);

        // Notify consensus immediately — enables voting without EVM execution.
        debug!(target: "n42::cl::exec_bridge", %hash, "block data cached, notifying consensus (deferred execution)");
        if let Err(e) = self
            .engine
            .process_event(ConsensusEvent::BlockImported(hash))
        {
            error!(target: "n42::cl::exec_bridge", error = %e, "error processing BlockImported for deferred execution");
        }

        // Follower eager import: start new_payload + fcu in parallel with consensus voting.
        // By the time finalize_committed_block runs after consensus commit, the block is
        // likely already in reth (Case A), eliminating the ~200ms background import stall.
        if let Some(ref engine_handle) = self.beacon_engine {
            let eh = engine_handle.clone();
            let payload_json = broadcast.payload_json;
            let view = broadcast.view;
            let block_ts = broadcast.timestamp;
            let eager_done_tx = self.eager_import_done_tx.clone();
            tokio::spawn(async move {
                let execution_data = match serde_json::from_slice(&payload_json) {
                    Ok(data) => data,
                    Err(_) => return,
                };
                let import_start = std::time::Instant::now();
                match eh.new_payload(execution_data).await {
                    Ok(status) if matches!(status.status, PayloadStatusEnum::Valid | PayloadStatusEnum::Accepted) => {
                        let np_elapsed = import_start.elapsed().as_millis() as u64;
                        let fcu = ForkchoiceState {
                            head_block_hash: hash,
                            safe_block_hash: hash,
                            finalized_block_hash: hash,
                        };
                        if let Err(e) = eh
                            .fork_choice_updated(fcu, None, EngineApiMessageVersion::default())
                            .await
                        {
                            info!(target: "n42::cl::exec_bridge", %hash, view, np_elapsed, error = %e, "follower eager import: fcu failed");
                        } else {
                            info!(target: "n42::cl::exec_bridge", %hash, view, elapsed_ms = import_start.elapsed().as_millis() as u64, np_elapsed, "follower eager import: block imported");
                            metrics::counter!("n42_follower_eager_import_hits_total").increment(1);
                            let _ = eager_done_tx.send((hash, block_ts));
                        }
                    }
                    Ok(status) => {
                        debug!(target: "n42::cl::exec_bridge", %hash, view, status = ?status.status, "follower eager import: not accepted");
                    }
                    Err(e) => {
                        debug!(target: "n42::cl::exec_bridge", %hash, view, error = %e, "follower eager import: failed");
                    }
                }
            });
        }
    }

    pub(super) fn handle_blob_sidecar(&self, data: Vec<u8>) {
        let blob_store = match &self.blob_store {
            Some(bs) => bs,
            None => return,
        };

        let broadcast: BlobSidecarBroadcast = match bincode::deserialize(&data) {
            Ok(b) => b,
            Err(e) => {
                warn!(target: "n42::cl::exec_bridge", error = %e, "invalid blob sidecar broadcast");
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
                        debug!(target: "n42::cl::exec_bridge", %tx_hash, error = %e, "failed to insert blob sidecar");
                    }
                }
                Err(e) => {
                    warn!(target: "n42::cl::exec_bridge", %tx_hash, error = %e, "failed to decode blob sidecar RLP");
                }
            }
        }

        debug!(
            target: "n42::cl::exec_bridge",
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

        // Update timestamp from the direct field.
        if broadcast.timestamp > 0 {
            self.last_committed_timestamp = self.last_committed_timestamp.max(broadcast.timestamp);
        }

        let execution_data = match serde_json::from_slice(&broadcast.payload_json) {
            Ok(data) => data,
            Err(e) => {
                warn!(target: "n42::cl::exec_bridge", hash = %broadcast.block_hash, "failed to deserialize execution payload: {e}");
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
                        target: "n42::cl::exec_bridge",
                        hash = %broadcast.block_hash,
                        status = ?status.status,
                        "new_payload rejected block"
                    );
                }
            }
            Err(e) => {
                error!(target: "n42::cl::exec_bridge", hash = %broadcast.block_hash, error = %e, "new_payload failed");
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
                target: "n42::cl::exec_bridge",
                expected = %broadcast.block_hash,
                engine_hash = %valid_hash,
                "block hash mismatch between broadcast and engine, skipping"
            );
            return;
        }

        debug!(target: "n42::cl::exec_bridge", hash = %broadcast.block_hash, "block imported from leader");

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
                target: "n42::cl::exec_bridge",
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
            error!(target: "n42::cl::exec_bridge", error = %e, "error processing BlockImported");
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
            target: "n42::cl::exec_bridge",
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
            warn!(target: "n42::cl::exec_bridge", view = deferred_view, hash = %broadcast.block_hash, error = %e, "mobile_packet_tx full or closed, deferred verification packet lost");
        }

        if self.engine.is_current_leader() {
            if self.speculative_build_hash == Some(broadcast.block_hash) {
                debug!(
                    target: "n42::cl::exec_bridge",
                    next_view = self.engine.current_view(),
                    "leader: speculative build already in progress (deferred finalization)"
                );
            } else {
                debug!(
                    target: "n42::cl::exec_bridge",
                    next_view = self.engine.current_view(),
                    "leader for next view, triggering immediate payload build"
                );
                self.do_trigger_payload_build(None).await;
            }
        }
    }

    fn queue_syncing_block(&mut self, broadcast: &BlockDataBroadcast) {
        info!(target: "n42::cl::exec_bridge", hash = %broadcast.block_hash, "new_payload returned Syncing, queuing for retry");
        if let Ok(data) = bincode::serialize(broadcast) {
            if self.syncing_blocks.len() >= MAX_SYNCING_QUEUE_SIZE {
                self.syncing_blocks.pop_front();
            }
            self.syncing_blocks.push_back((data, 0));
        }
    }

    async fn retry_syncing_blocks(&mut self, engine_handle: &ConsensusEngineHandle<EthEngineTypes>) {
        let queued: Vec<(Vec<u8>, u32)> = self.syncing_blocks.drain(..).collect();
        info!(target: "n42::cl::exec_bridge", count = queued.len(), "retrying previously-syncing blocks");

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
                    warn!(target: "n42::cl::exec_bridge", %retry_hash, error = %e, "failed to deserialize retry payload");
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
                    info!(target: "n42::cl::exec_bridge", %retry_hash, "syncing block retry succeeded");
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
                        error!(target: "n42::cl::exec_bridge", error = %e, "error processing BlockImported for retry");
                    }
                }
                Ok(rs) if matches!(rs.status, PayloadStatusEnum::Syncing) => {
                    let next_retry = retry_count + 1;
                    if next_retry >= MAX_SYNCING_RETRIES {
                        warn!(target: "n42::cl::exec_bridge", %retry_hash, retries = next_retry, "syncing block exceeded max retries, dropping");
                    } else {
                        debug!(target: "n42::cl::exec_bridge", %retry_hash, retry = next_retry, "retry still Syncing, re-queuing");
                        self.syncing_blocks.push_back((data, next_retry));
                    }
                }
                Ok(rs) => {
                    warn!(target: "n42::cl::exec_bridge", %retry_hash, status = ?rs.status, "retry rejected");
                }
                Err(e) => {
                    warn!(target: "n42::cl::exec_bridge", %retry_hash, error = %e, "retry new_payload failed");
                }
            }
        }
    }
}

// ── Free functions for the spawned payload build task ──

/// Leader pipelined import: broadcast block data, trigger consensus, then import eagerly.
///
/// The leader already executed all transactions during payload building.  Instead of calling
/// `new_payload` synchronously (which would double EVM time on the critical path), we:
///   1. Broadcast block data + blob sidecars to followers
///   2. Send BlockReady to trigger consensus voting immediately
///   3. Call `new_payload` + `fcu` eagerly while consensus is running in parallel
///
/// If the eager import completes before `finalize_committed_block()` runs, that function
/// will find the block already in reth (Case A) and trigger the next build immediately —
/// eliminating the ~200ms pipeline stall from the background import path (Case B).
/// If consensus is faster than import, Case B still works as a fallback.
async fn handle_built_payload(
    payload: EthBuiltPayload,
    engine_handle: ConsensusEngineHandle<EthEngineTypes>,
    network: NetworkHandle,
    block_ready_tx: mpsc::UnboundedSender<B256>,
    leader_payload_tx: mpsc::UnboundedSender<(B256, Vec<u8>)>,
    current_view: u64,
    blob_store: Option<DiskFileBlobStore>,
    eager_import_done_tx: mpsc::UnboundedSender<(B256, u64)>,
) {
    let hash = payload.block().hash();

    let execution_data =
        <EthEngineTypes as PayloadTypes>::block_to_payload(payload.block().clone());

    let payload_json = match serde_json::to_vec(&execution_data) {
        Ok(json) => json,
        Err(e) => {
            error!(target: "n42::cl::exec_bridge", %hash, error = %e, "CRITICAL: failed to serialize execution payload");
            return;
        }
    };

    let block_timestamp = payload.block().header().timestamp;
    debug!(target: "n42::cl::exec_bridge", %hash, block_timestamp, "leader pipelined import: broadcasting block data");

    // 1. Broadcast block data + blob sidecars to followers
    broadcast_block_data(&network, &leader_payload_tx, hash, current_view, &payload_json, block_timestamp);
    broadcast_blob_sidecars(&network, &payload, hash, current_view, blob_store);

    // 2. Trigger consensus voting immediately (non-blocking channel send)
    let _ = block_ready_tx.send(hash);

    // 3. Eager import: run new_payload + fcu while consensus votes in parallel.
    //    This is the key pipelining optimization — by the time finalize_committed_block
    //    runs after consensus commit, the block is likely already in reth (Case A).
    let import_start = std::time::Instant::now();
    match engine_handle.new_payload(execution_data).await {
        Ok(status) if matches!(status.status, PayloadStatusEnum::Valid | PayloadStatusEnum::Accepted) => {
            let np_elapsed = import_start.elapsed().as_millis() as u64;
            let fcu = ForkchoiceState {
                head_block_hash: hash,
                safe_block_hash: hash,
                finalized_block_hash: hash,
            };
            if let Err(e) = engine_handle
                .fork_choice_updated(fcu, None, EngineApiMessageVersion::default())
                .await
            {
                info!(target: "n42::cl::exec_bridge", %hash, np_elapsed, error = %e, "eager import: fcu failed");
            } else {
                info!(target: "n42::cl::exec_bridge", %hash, elapsed_ms = import_start.elapsed().as_millis() as u64, np_elapsed, "eager import: block imported successfully");
                metrics::counter!("n42_eager_import_hits_total").increment(1);
                let _ = eager_import_done_tx.send((hash, block_timestamp));
            }
        }
        Ok(status) => {
            info!(target: "n42::cl::exec_bridge", %hash, status = ?status.status, elapsed_ms = import_start.elapsed().as_millis() as u64, "eager import: new_payload not accepted");
        }
        Err(e) => {
            info!(target: "n42::cl::exec_bridge", %hash, error = %e, "eager import: new_payload failed");
        }
    }
}

fn broadcast_block_data(
    network: &NetworkHandle,
    leader_payload_tx: &mpsc::UnboundedSender<(B256, Vec<u8>)>,
    hash: B256,
    current_view: u64,
    payload_json: &[u8],
    timestamp: u64,
) {
    if payload_json.is_empty() {
        return;
    }
    let broadcast = BlockDataBroadcast {
        block_hash: hash,
        view: current_view,
        payload_json: payload_json.to_vec(),
        timestamp,
    };
    match bincode::serialize(&broadcast) {
        Ok(encoded) => {
            debug!(target: "n42::cl::exec_bridge", %hash, bytes = encoded.len(), "broadcasting block data to followers");
            if let Err(e) = network.announce_block(encoded.clone()) {
                warn!(target: "n42::cl::exec_bridge", error = %e, "failed to broadcast block data");
            }
            let _ = leader_payload_tx.send((hash, encoded));
        }
        Err(e) => {
            error!(target: "n42::cl::exec_bridge", error = %e, "failed to serialize block data broadcast");
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
                debug!(target: "n42::cl::exec_bridge", %hash, blob_count = sidecar_count, bytes = encoded.len(), "broadcasting blob sidecars");
                if let Err(e) = network.broadcast_blob_sidecar(encoded) {
                    warn!(target: "n42::cl::exec_bridge", error = %e, "failed to broadcast blob sidecars");
                }
            }
        }
        Ok(_) => {}
        Err(e) => {
            warn!(target: "n42::cl::exec_bridge", %hash, error = %e, "failed to get blob sidecars from store");
        }
    }
}
