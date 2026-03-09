use super::{BlobSidecarBroadcast, BlockDataBroadcast, CompactBlockExecution, ConsensusOrchestrator};
use alloy_consensus::Typed2718;
use alloy_eips::eip7594::BlobTransactionSidecarVariant;
use alloy_primitives::{Address, B256};
use alloy_rlp::Encodable;
use alloy_rpc_types_engine::{ForkchoiceState, PayloadAttributes, PayloadStatusEnum};
use n42_consensus::ConsensusEvent;
use n42_network::NetworkHandle;
use reth_ethereum_engine_primitives::EthEngineTypes;
use reth_execution_types::{BlockExecutionOutput, BlockExecutionResult};
use reth_node_builder::ConsensusEngineHandle;
use reth_payload_builder::{EthBuiltPayload, PayloadBuilderHandle, PayloadId};
use reth_payload_primitives::{EngineApiMessageVersion, PayloadKind, PayloadTypes};
use reth_transaction_pool::blobstore::{BlobStore, DiskFileBlobStore};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

/// Whether Compact Block (follower EVM skip) is enabled.
/// Controlled by `N42_COMPACT_BLOCK` env var: "0" to disable, anything else or absent = enabled.
pub(super) fn compact_block_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("N42_COMPACT_BLOCK").map(|v| v != "0").unwrap_or(true)
    })
}

type CachedPayloadData = (BlockExecutionOutput<reth_ethereum_primitives::Receipt>, Vec<Address>);

/// Take execution output from broadcast cache and serialize it for followers.
fn take_and_serialize_execution_output(hash: &B256) -> Option<Vec<u8>> {
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
            let compressed = super::compress_payload(&serialized);
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

/// Deserialize compact block execution output and inject into payload cache.
pub(super) fn inject_compact_block(hash: &B256, compressed: &[u8]) -> bool {
    let decompressed = match super::decompress_payload(compressed) {
        Ok(d) => d,
        Err(e) => {
            warn!(target: "n42::cl::exec_bridge", %hash, error = %e, "compact block: failed to decompress");
            return false;
        }
    };
    let compact: CompactBlockExecution = match serde_json::from_slice(&decompressed) {
        Ok(c) => c,
        Err(e) => {
            warn!(target: "n42::cl::exec_bridge", %hash, error = %e, "compact block: failed to deserialize");
            return false;
        }
    };
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
    info!(target: "n42::cl::exec_bridge", %hash, "N42_COMPACT_BLOCK: injected execution output into payload cache");
    metrics::counter!("n42_compact_block_cache_injected").increment(1);
    true
}

/// Delay before resolving the built payload, allowing the builder to pack transactions.
/// Configurable via `N42_BUILDER_WARMUP_MS` (default: 10).
/// Set to 0 in high-throughput scenarios where the tx pool is always filled.
fn builder_warmup_delay() -> Duration {
    static DELAY: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *DELAY.get_or_init(|| {
        let ms: u64 = std::env::var("N42_BUILDER_WARMUP_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(10);
        Duration::from_millis(ms)
    })
}

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

        // Guard: prevent duplicate builds on the same parent hash.
        // Without this, eager import + speculative build can race with the finalize path,
        // spawning multiple resolve tasks that produce different blocks at the same height
        // (same parent, different timestamps). This floods reth with conflicting new_payload
        // calls, triggering pipeline sync and permanent chain stalls.
        let parent = self.head_block_hash;
        if let Some(building_parent) = self.building_on_parent
            && building_parent == parent
        {
            debug!(target: "n42::cl::exec_bridge", %parent, "build already in progress on this parent, skipping");
            return;
        }
        self.building_on_parent = Some(parent);

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
                        warn!(target: "n42::cl::exec_bridge", "fork_choice_updated did not return payload_id, scheduling retry");
                        // FCU returned SYNCING — reth hasn't caught up yet.
                        // Clear both guards so the retry can re-attempt.
                        self.building_on_parent = None;
                        self.speculative_build_hash = None;
                        // Schedule a retry so the leader doesn't permanently stall.
                        self.schedule_build_retry();
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
            // Clear both guards so retry can re-attempt.
            self.building_on_parent = None;
            self.speculative_build_hash = None;
            // Also schedule retry on FCU error — the execution layer may recover.
            self.schedule_build_retry();
        }
    }

    /// Schedules a delayed build retry when FCU returns SYNCING or no payload_id.
    ///
    /// Uses the existing `next_build_at` / build_timer mechanism in the main select! loop.
    /// The leader will re-attempt `do_trigger_payload_build` after the delay.
    /// Each call resets the timer, providing natural exponential spacing if called repeatedly.
    pub(super) fn schedule_build_retry(&mut self) {
        if !self.engine.is_current_leader() {
            return;
        }
        // Retry after 2 seconds — enough time for reth to complete pipeline sync.
        let retry_at = tokio::time::Instant::now() + Duration::from_secs(2);
        info!(target: "n42::cl::exec_bridge", "build retry scheduled in 2s (reth may be syncing)");
        self.next_build_at = Some(retry_at);
        // Clear slot_timestamp to indicate this is a retry, not a scheduled slot.
        self.next_slot_timestamp = None;
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
        let build_complete_tx = self.build_complete_tx.clone();
        let block_guard = self.eager_import_block_guard.clone();

        let handle = tokio::spawn(async move {
            // Allow builder time to pack transactions from the pool.
            let warmup = builder_warmup_delay();
            if !warmup.is_zero() {
                tokio::time::sleep(warmup).await;
            }

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
                        block_guard,
                    )
                    .await;
                }
                Some(Err(e)) => error!(target: "n42::cl::exec_bridge", error = %e, "payload build failed"),
                None => warn!(target: "n42::cl::exec_bridge", "payload not found (already resolved or expired)"),
            }
        });

        // Monitor the JoinHandle so that panics/cancellations in the payload
        // resolve task are logged rather than silently swallowed.
        // Also sends the build-complete signal so `building_on_parent` is cleared
        // even on failure/timeout/panic — preventing permanent build stalls.
        tokio::spawn(async move {
            if let Err(e) = handle.await {
                error!(
                    target: "n42::cl::exec_bridge",
                    error = %e,
                    "payload resolve task terminated unexpectedly (panic or cancellation)"
                );
            }
            // Signal completion regardless of success/failure/panic.
            let _ = build_complete_tx.send(());
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

        // Dedup: skip if we already have this block (direct push + GossipSub overlap).
        if self.pending_block_data.contains_key(&hash) {
            debug!(target: "n42::cl::exec_bridge", %hash, "duplicate block data, skipping");
            return;
        }

        self.pending_executions.remove(&hash);

        // Pipeline: follower received block data — create timing entry.
        // `build_complete` is set immediately (the leader already built it).
        let mut timing = super::PipelineTiming::new_follower();
        timing.build_complete = Some(tokio::time::Instant::now());
        self.record_pipeline_timing(hash, timing);

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
            let payload_compressed = broadcast.payload_json;
            let execution_output_compressed = broadcast.execution_output;
            let view = broadcast.view;
            let block_ts = broadcast.timestamp;
            let eager_done_tx = self.eager_import_done_tx.clone();
            let block_guard = self.eager_import_block_guard.clone();
            tokio::spawn(async move {
                let decompress_start = std::time::Instant::now();
                let payload_json = match super::decompress_payload(&payload_compressed) {
                    Ok(d) => d,
                    Err(_) => return,
                };
                let decompress_ms = decompress_start.elapsed().as_millis() as u64;
                // Deserialize first, then use typed accessor for block number.
                let deser_start = std::time::Instant::now();
                let execution_data: alloy_rpc_types_engine::ExecutionData = match serde_json::from_slice(&payload_json) {
                    Ok(data) => data,
                    Err(_) => return,
                };
                let deser_ms = deser_start.elapsed().as_millis() as u64;
                info!(
                    target: "n42::cl::exec_bridge",
                    %hash,
                    compressed_kb = payload_compressed.len() / 1024,
                    decompressed_kb = payload_json.len() / 1024,
                    decompress_ms,
                    deser_ms,
                    has_compact_block = execution_output_compressed.is_some(),
                    "N42_DECOMPRESS: follower payload decoded"
                );
                // Guard against duplicate imports for the same block number with different hashes.
                // Sending new_payload for the same block number but different hash triggers
                // reth pipeline sync and causes chain stalls.
                let block_number = execution_data.block_number();
                let prev = block_guard.fetch_max(block_number, std::sync::atomic::Ordering::SeqCst);
                if prev >= block_number {
                    info!(target: "n42::cl::exec_bridge", %hash, view, block_number, prev, "follower eager import: skipping duplicate block number");
                    return;
                }

                // Compact Block: inject execution output into payload cache before new_payload.
                // This lets reth skip EVM re-execution (cache hit path), reducing import from
                // ~209ms to ~22ms. Safety: state root is still verified by reth's new_payload.
                let compact_injected = execution_output_compressed.as_ref()
                    .is_some_and(|exec| compact_block_enabled() && inject_compact_block(&hash, exec));

                // Follower eager import: only run new_payload (no FCU).
                // new_payload inserts the block into reth's engine tree so that
                // finalize_committed_block's FCU can accept it instantly (Case A).
                // We intentionally skip fork_choice_updated here to avoid changing
                // the canonical chain — speculative blocks may not match what consensus
                // ultimately commits, and premature FCU causes reorgs that stall the chain.
                let import_start = std::time::Instant::now();
                match eh.new_payload(execution_data).await {
                    Ok(status) if matches!(status.status, PayloadStatusEnum::Valid | PayloadStatusEnum::Accepted) => {
                        let np_elapsed = import_start.elapsed().as_millis() as u64;
                        info!(
                            target: "n42::cl::exec_bridge",
                            %hash, view, np_elapsed, compact_injected,
                            "follower eager import: new_payload accepted (no FCU)"
                        );
                        if compact_injected {
                            metrics::counter!("n42_compact_block_cache_hits").increment(1);
                        }
                        metrics::counter!("n42_follower_eager_import_hits_total").increment(1);
                        let _ = eager_done_tx.send((hash, block_ts));
                    }
                    Ok(status) => {
                        debug!(target: "n42::cl::exec_bridge", %hash, view, status = ?status.status, compact_injected, "follower eager import: not accepted");
                        if compact_injected {
                            metrics::counter!("n42_compact_block_cache_misses").increment(1);
                        }
                    }
                    Err(e) => {
                        debug!(target: "n42::cl::exec_bridge", %hash, view, error = %e, compact_injected, "follower eager import: failed");
                        if compact_injected {
                            metrics::counter!("n42_compact_block_cache_misses").increment(1);
                        }
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

        let payload_json = match super::decompress_payload(&broadcast.payload_json) {
            Ok(d) => d,
            Err(e) => {
                warn!(target: "n42::cl::exec_bridge", hash = %broadcast.block_hash, "failed to decompress payload: {e}");
                return;
            }
        };
        let execution_data = match serde_json::from_slice(&payload_json) {
            Ok(data) => data,
            Err(e) => {
                warn!(target: "n42::cl::exec_bridge", hash = %broadcast.block_hash, "failed to deserialize execution payload: {e}");
                return;
            }
        };

        // Compact Block: inject execution output into payload cache before new_payload.
        if let Some(ref exec_compressed) = broadcast.execution_output
            && compact_block_enabled()
        {
            inject_compact_block(&broadcast.block_hash, exec_compressed);
        }

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
            let retry_payload = match super::decompress_payload(&retry_broadcast.payload_json) {
                Ok(d) => d,
                Err(_) => continue,
            };
            let retry_exec = match serde_json::from_slice(&retry_payload) {
                Ok(d) => d,
                Err(e) => {
                    warn!(target: "n42::cl::exec_bridge", %retry_hash, error = %e, "failed to deserialize retry payload");
                    continue;
                }
            };

            // Compact Block: inject on retry path too.
            if let Some(ref exec_compressed) = retry_broadcast.execution_output
                && compact_block_enabled()
            {
                inject_compact_block(&retry_hash, exec_compressed);
            }

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
#[allow(clippy::too_many_arguments)]
async fn handle_built_payload(
    payload: EthBuiltPayload,
    engine_handle: ConsensusEngineHandle<EthEngineTypes>,
    network: NetworkHandle,
    block_ready_tx: mpsc::UnboundedSender<B256>,
    leader_payload_tx: mpsc::UnboundedSender<(B256, Vec<u8>)>,
    current_view: u64,
    blob_store: Option<DiskFileBlobStore>,
    eager_import_done_tx: mpsc::UnboundedSender<(B256, u64)>,
    block_guard: Arc<std::sync::atomic::AtomicU64>,
) {
    let hash = payload.block().hash();

    let execution_data =
        <EthEngineTypes as PayloadTypes>::block_to_payload(payload.block().clone());

    let ser_start = std::time::Instant::now();
    let payload_json = match serde_json::to_vec(&execution_data) {
        Ok(json) => json,
        Err(e) => {
            error!(target: "n42::cl::exec_bridge", %hash, error = %e, "CRITICAL: failed to serialize execution payload");
            return;
        }
    };
    let ser_ms = ser_start.elapsed().as_millis() as u64;

    let block_timestamp = payload.block().header().timestamp;
    let tx_count = payload.block().body().transactions().count();
    info!(
        target: "n42::cl::exec_bridge",
        %hash,
        tx_count,
        payload_kb = payload_json.len() / 1024,
        ser_ms,
        block_timestamp,
        "N42_LEADER_SERIALIZE: payload serialized"
    );

    // Compact Block: serialize execution output for followers to skip EVM re-execution.
    let execution_output_bytes = if compact_block_enabled() {
        take_and_serialize_execution_output(&hash)
    } else {
        None
    };

    // 1. Broadcast block data + blob sidecars to followers
    broadcast_block_data(&network, &leader_payload_tx, hash, current_view, &payload_json, block_timestamp, execution_output_bytes);
    broadcast_blob_sidecars(&network, &payload, hash, current_view, blob_store);

    // 2. Trigger consensus voting immediately (non-blocking channel send)
    let _ = block_ready_tx.send(hash);

    // 3. Eager import: run new_payload + fcu while consensus votes in parallel.
    //    This is the key pipelining optimization — by the time finalize_committed_block
    //    runs after consensus commit, the block is likely already in reth (Case A).
    //
    //    Guard: prevent importing the same block number with different hashes,
    //    which triggers reth pipeline sync and chain stalls.
    let block_number = payload.block().header().number;
    let prev = block_guard.fetch_max(block_number, std::sync::atomic::Ordering::SeqCst);
    if prev >= block_number {
        debug!(target: "n42::cl::exec_bridge", %hash, block_number, prev, "leader eager import: skipping duplicate block number");
        return;
    }
    // Leader eager import: only run new_payload (no FCU).
    // Inserts block into reth's engine tree so finalize_committed_block's FCU
    // can accept it instantly. We skip FCU to avoid changing canonical chain —
    // only finalize_committed_block (after consensus commit) should do FCU.
    let import_start = std::time::Instant::now();
    match engine_handle.new_payload(execution_data).await {
        Ok(status) if matches!(status.status, PayloadStatusEnum::Valid | PayloadStatusEnum::Accepted) => {
            let np_elapsed = import_start.elapsed().as_millis() as u64;
            info!(target: "n42::cl::exec_bridge", %hash, np_elapsed, "eager import: new_payload accepted (no FCU)");
            metrics::counter!("n42_eager_import_hits_total").increment(1);
            let _ = eager_import_done_tx.send((hash, block_timestamp));
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
    execution_output: Option<Vec<u8>>,
) {
    if payload_json.is_empty() {
        return;
    }
    let compress_start = std::time::Instant::now();
    let compressed = super::compress_payload(payload_json);
    let compress_ms = compress_start.elapsed().as_millis() as u64;
    let raw_len = payload_json.len();
    let compressed_len = compressed.len();
    let exec_kb = execution_output.as_ref().map_or(0, |v| v.len() / 1024);
    info!(
        target: "n42::cl::exec_bridge",
        %hash,
        raw_kb = raw_len / 1024,
        compressed_kb = compressed_len / 1024,
        exec_kb,
        ratio = format_args!("{:.1}%", compressed_len as f64 / raw_len.max(1) as f64 * 100.0),
        compress_ms,
        "N42_COMPRESS: payload compressed"
    );
    let broadcast = BlockDataBroadcast {
        block_hash: hash,
        view: current_view,
        payload_json: compressed,
        timestamp,
        execution_output,
    };
    let encoded = match bincode::serialize(&broadcast) {
        Ok(enc) => enc,
        Err(e) => {
            error!(target: "n42::cl::exec_bridge", error = %e, "failed to serialize block data broadcast");
            return;
        }
    };

    // Leader direct push: send to all known validator peers via QUIC unicast.
    // This bypasses GossipSub mesh flooding for large payloads.
    let validator_peers = network.all_validator_peers();
    let direct_count = validator_peers.len();
    let send_start = std::time::Instant::now();
    for (_idx, peer_id) in &validator_peers {
        let _ = network.send_block_direct(*peer_id, encoded.clone());
    }
    let send_ms = send_start.elapsed().as_millis() as u64;
    if direct_count > 0 {
        info!(
            target: "n42::cl::exec_bridge",
            %hash,
            encoded_kb = encoded.len() / 1024,
            direct_count,
            send_ms,
            "N42_DIRECT_PUSH: sent to all validator peers"
        );
    }

    // GossipSub fallback: only used when direct push hasn't covered all peers
    // (e.g. during startup, before Identify exchange completes).
    // When direct push is active, skip GossipSub to avoid redundant large-message
    // flooding that causes 10-30s propagation delays through the mesh.
    if direct_count == 0 {
        if let Err(e) = network.announce_block(encoded.clone()) {
            warn!(target: "n42::cl::exec_bridge", error = %e, "failed to broadcast block data via gossipsub fallback");
        }
        info!(
            target: "n42::cl::exec_bridge",
            %hash,
            bytes = encoded.len(),
            "block data broadcast: gossipsub only (no validator peers known)"
        );
    } else {
        info!(
            target: "n42::cl::exec_bridge",
            %hash,
            bytes = encoded.len(),
            direct_peers = direct_count,
            "block data broadcast: direct push only (skipping gossipsub)"
        );
    }

    let _ = leader_payload_tx.send((hash, encoded));
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
