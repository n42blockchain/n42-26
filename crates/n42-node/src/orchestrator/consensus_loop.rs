use super::ConsensusOrchestrator;
use super::state_mgmt::max_consecutive_empty_skips;
use alloy_primitives::B256;
use alloy_rpc_types_engine::{ForkchoiceState, PayloadStatusEnum};
use n42_consensus::EngineOutput;
use n42_primitives::QuorumCertificate;
use reth_ethereum_engine_primitives::EthEngineTypes;
use reth_node_builder::ConsensusEngineHandle;
use reth_payload_primitives::EngineApiMessageVersion;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;
use metrics::{counter, gauge, histogram};
use tracing::{debug, error, info, warn};

/// Number of recent blocks to check for emptiness before skipping proposal.
const EMPTY_BLOCK_CHECK_WINDOW: usize = 3;

/// Payload size below which a block is considered "empty" (no meaningful transactions).
const EMPTY_BLOCK_SIZE_THRESHOLD: usize = 512;

impl ConsensusOrchestrator {
    pub(super) fn recent_blocks_empty(&self) -> bool {
        let check_count = EMPTY_BLOCK_CHECK_WINDOW.min(self.committed_blocks.len());
        if check_count == 0 {
            return false;
        }
        self.committed_blocks
            .iter()
            .rev()
            .take(check_count)
            .all(|b| b.payload.is_empty() || b.payload.len() < EMPTY_BLOCK_SIZE_THRESHOLD)
    }

    /// Schedules the next payload build, skipping if recent blocks are empty.
    pub(super) async fn schedule_payload_build(&mut self) {
        if self.recent_blocks_empty()
            && self.consecutive_empty_skips < max_consecutive_empty_skips()
        {
            self.consecutive_empty_skips += 1;
            counter!("n42_empty_block_skips_total").increment(1);
            debug!(
                target: "n42::cl::consensus_loop",
                skip_count = self.consecutive_empty_skips,
                max = max_consecutive_empty_skips(),
                "skipping empty block proposal (low activity)"
            );
            if !self.slot_time.is_zero() {
                if self.fast_propose {
                    // In fast propose mode, retry empty checks after slot_time delay
                    // without grid alignment.
                    self.next_build_at = Some(Instant::now() + self.slot_time);
                    self.next_slot_timestamp = None;
                } else {
                    let (slot_ts, delay) = self.next_slot_boundary();
                    self.next_build_at = Some(Instant::now() + delay);
                    self.next_slot_timestamp = Some(slot_ts);
                }
            }
            return;
        }
        self.consecutive_empty_skips = 0;

        if self.slot_time.is_zero() || self.fast_propose {
            // Fast propose: skip slot boundary alignment. Use a small configurable
            // delay to let transactions accumulate in the pool.
            let min_delay_ms: u64 = std::env::var("N42_MIN_PROPOSE_DELAY_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            if min_delay_ms > 0 {
                let delay = Duration::from_millis(min_delay_ms);
                self.next_build_at = Some(Instant::now() + delay);
                self.next_slot_timestamp = None;
                info!(
                    target: "n42::cl::consensus_loop",
                    min_delay_ms,
                    "fast propose: scheduled build after minimum delay"
                );
            } else {
                if self.fast_propose {
                    info!(target: "n42::cl::consensus_loop", "fast propose: triggering immediate payload build");
                }
                self.do_trigger_payload_build(None).await;
            }
        } else {
            let (slot_ts, delay) = self.next_slot_boundary();
            let deadline = Instant::now() + delay;
            self.next_build_at = Some(deadline);
            self.next_slot_timestamp = Some(slot_ts);
            info!(
                target: "n42::cl::consensus_loop",
                slot_timestamp = slot_ts,
                delay_ms = delay.as_millis() as u64,
                "scheduled payload build at next slot boundary"
            );
        }
    }

    /// Returns `(slot_timestamp_secs, delay_until_boundary)`.
    pub(super) fn next_slot_boundary(&self) -> (u64, Duration) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let now_ms = now.as_millis() as u64;
        let slot_ms = self.slot_time.as_millis() as u64;

        if slot_ms == 0 {
            tracing::error!(target: "n42::cl::consensus_loop", "slot_time is zero, defaulting to 1-second delay");
            return (now_ms / 1000 + 1, Duration::from_secs(1));
        }

        let current_slot = now_ms / slot_ms;
        let next_boundary_ms = (current_slot + 1) * slot_ms;
        let delay_ms = next_boundary_ms - now_ms;
        let slot_timestamp = next_boundary_ms / 1000;

        (slot_timestamp, Duration::from_millis(delay_ms))
    }

    pub(super) async fn handle_engine_output(&mut self, output: EngineOutput) {
        match output {
            EngineOutput::BroadcastMessage(msg) => {
                if let Err(e) = self.network.broadcast_consensus(msg) {
                    error!(target: "n42::cl::consensus_loop", error = %e, "failed to broadcast consensus message");
                }
            }
            EngineOutput::SendToValidator(target, msg) => {
                // Direct send + broadcast for reliability. Direct send via request-response
                // can silently fail (QUIC transport issues), so always broadcast as backup.
                // The consensus engine deduplicates votes by (view, voter).
                if let Some(peer_id) = self.network.validator_peer(target) {
                    let _ = self.network.send_direct(peer_id, msg.clone());
                }
                if let Err(e) = self.network.broadcast_consensus(msg) {
                    error!(target: "n42::cl::consensus_loop", target_validator = target, error = %e, "broadcast failed");
                }
            }
            EngineOutput::ExecuteBlock(block_hash) => {
                self.handle_execute_block(block_hash).await;
            }
            EngineOutput::BlockCommitted { view, block_hash, commit_qc } => {
                self.handle_block_committed(view, block_hash, commit_qc).await;
            }
            EngineOutput::ViewChanged { new_view } => {
                self.handle_view_changed(new_view).await;
            }
            EngineOutput::SyncRequired { local_view, target_view } => {
                counter!("n42_sync_required_total").increment(1);
                self.initiate_sync(local_view, target_view);
            }
            EngineOutput::EquivocationDetected { view, validator, hash1, hash2 } => {
                counter!("n42_equivocations_detected_total").increment(1);
                warn!(
                    target: "n42::cl::consensus_loop",
                    view,
                    validator,
                    %hash1,
                    %hash2,
                    "EQUIVOCATION: validator voted for two different blocks in same view"
                );
                if let Some(ref state) = self.consensus_state {
                    state.record_equivocation(view, validator, hash1, hash2);
                }
            }
            EngineOutput::EpochTransition { new_epoch, validator_count } => {
                info!(target: "n42::cl::consensus_loop", new_epoch, validator_count, "epoch transition: validator set updated");
                counter!("n42_epoch_transitions_total").increment(1);
                gauge!("n42_epoch_validator_count").set(validator_count as f64);

                let updated_vs = self.engine.epoch_manager().current_validator_set().clone();
                self.validator_set_for_sync = Some(updated_vs);

                // Pre-stage the validator set for the epoch after next, if scheduled.
                if let Some(schedule) = &self.epoch_schedule
                    && let Some((validators, threshold)) = schedule.get_for_epoch(new_epoch + 1)
                {
                    self.engine.epoch_manager_mut().stage_next_epoch(validators, threshold);
                    info!(
                        target: "n42::cl::consensus_loop",
                        next_epoch = new_epoch + 1,
                        validator_count = validators.len(),
                        threshold,
                        "staged next epoch validator set from schedule"
                    );
                }
            }
        }
    }

    async fn handle_execute_block(&mut self, block_hash: B256) {
        let has_data = self.pending_block_data.contains_key(&block_hash);
        info!(
            target: "n42::cl::consensus_loop",
            %block_hash,
            pending_data = has_data,
            "ExecuteBlock requested"
        );

        if has_data {
            // Block data already cached — notify consensus immediately for fast voting.
            // Actual EVM execution deferred to finalize_committed_block() (Case B).
            info!(target: "n42::cl::consensus_loop", %block_hash, "block data available, sending BlockImported to consensus");
            if let Err(e) = self
                .engine
                .process_event(n42_consensus::ConsensusEvent::BlockImported(block_hash))
            {
                error!(target: "n42::cl::consensus_loop", %block_hash, error = %e, "error processing BlockImported");
            }
        } else {
            self.pending_executions.insert(block_hash);
        }
    }

    async fn handle_block_committed(
        &mut self,
        view: u64,
        block_hash: B256,
        commit_qc: QuorumCertificate,
    ) {
        self.committed_block_count += 1;
        counter!("n42_blocks_committed_total").increment(1);
        gauge!("n42_consensus_view").set(view as f64);
        let elapsed = self.view_started_at.take().map(|t| t.elapsed());
        let elapsed_ms = elapsed.map(|d| d.as_millis() as u64);
        if let Some(ms) = elapsed_ms {
            histogram!("n42_consensus_commit_latency_ms").record(ms as f64);
        }
        // Adaptive timeout: feed observed consensus latency to pacemaker.
        if let Some(d) = elapsed {
            self.engine.pacemaker_mut().observe_commit_latency(d);
        }

        // Pipeline: record commit time and emit summary.
        let pipeline_summary = if let Some(mut timing) = self.pipeline_timings.remove(&block_hash) {
            timing.committed = Some(Instant::now());
            timing.emit_metrics();
            Some(timing.summary())
        } else {
            None
        };

        let consensus_timing = self.engine.last_committed_view_timing()
            .map(|t| t.summary())
            .unwrap_or_else(|| "-".to_string());
        let epoch = self.engine.epoch_manager().current_epoch();
        let peers = self.connected_peers.len();
        let hash_str = format!("{block_hash}");
        let short_hash = hash_str.get(..10).unwrap_or(&hash_str);
        info!(
            target: "n42::cl::slot_notifier",
            view,
            block = short_hash,
            epoch,
            peers,
            elapsed_ms = elapsed_ms.unwrap_or(0),
            pipeline = pipeline_summary.as_deref().unwrap_or("-"),
            consensus_timing = %consensus_timing,
            "view committed"
        );

        if let Some(ref state) = self.consensus_state {
            state.update_committed_qc(commit_qc.clone());
            state.notify_block_committed(block_hash, self.committed_block_count);
        }

        self.store_committed_block(view, block_hash, commit_qc.clone());
        self.head_block_hash = block_hash;

        // JMT background update: extract BundleState from pending block data.
        if let Some(ref jmt) = self.jmt {
            let diff = self.pending_block_data.get(&block_hash)
                .and_then(|data| Self::extract_state_diff_for_jmt(data));
            if let Some(diff) = diff {
                let jmt = Arc::clone(jmt);
                let state = self.consensus_state.clone();
                let block_count = self.committed_block_count;
                tokio::task::spawn_blocking(move || {
                    let start = std::time::Instant::now();
                    let mut tree = jmt.lock().unwrap_or_else(|e| e.into_inner());
                    match tree.apply_diff(&diff) {
                        Ok((version, root)) => {
                            let elapsed_ms = start.elapsed().as_millis();
                            gauge!("n42_jmt_latest_root").set(version as f64);
                            info!(
                                target: "n42::jmt",
                                version,
                                %root,
                                accounts = diff.len(),
                                storage_changes = diff.total_storage_changes(),
                                elapsed_ms,
                                "JMT updated"
                            );
                            if let Some(ref state) = state {
                                state.update_jmt_root(version, root);
                            }
                            // Prune old versions every 100 blocks to reclaim memory.
                            if block_count.is_multiple_of(100) {
                                tree.prune(200);
                            }
                        }
                        Err(e) => {
                            warn!(target: "n42::jmt", error = %e, "JMT apply_diff failed");
                        }
                    }
                });
            } else {
                debug!(target: "n42::jmt", %block_hash, "no block data available for JMT update (will catch up)");
            }
        }

        // ZK proof sidecar: schedule proof generation (async, non-blocking).
        // Uses committed_block_count (monotonically increasing) for interval check,
        // not view (which can skip on timeouts).
        if let Some(ref scheduler) = self.zk_scheduler {
            if let Some(data) = self.pending_block_data.get(&block_hash) {
                let block_count = self.committed_block_count;
                // Extract decompressed CompactBlockExecution JSON from the raw
                // BlockDataBroadcast. This is the same decompress path used by
                // JMT and staking scan.
                let bundle_json = match Self::extract_bundle_state_json(data) {
                    Some(json) => json,
                    None => {
                        warn!(target: "n42::zk", block_count, "ZK input: failed to extract bundle_state_json, using empty (proof will be incomplete)");
                        Vec::new()
                    }
                };
                let input = n42_zkproof::BlockExecutionInput {
                    block_hash,
                    block_number: block_count,
                    parent_hash: self.head_block_hash,
                    header_rlp: Vec::new(),
                    transactions_rlp: Vec::new(),
                    bundle_state_json: bundle_json,
                    parent_state_root: B256::ZERO,
                };
                scheduler.on_block_committed(block_count, input);
            }
        }

        // Scan committed block for staking/unstaking transactions.
        if let Some(ref staking_mgr) = self.staking_manager
            && let Some(data) = self.committed_blocks.back().map(|b| b.payload.as_slice())
            && !data.is_empty()
        {
            match super::decompress_payload(data) {
                Ok(decompressed) => {
                    let mut mgr = staking_mgr.lock().unwrap_or_else(|e| e.into_inner());
                    mgr.scan_committed_block(self.committed_block_count, &decompressed);
                }
                Err(e) => {
                    warn!(target: "n42::cl::consensus_loop", error = %e, "failed to decompress payload for staking scan");
                }
            }
        }

        self.finalize_committed_block(view, block_hash, commit_qc).await;
        self.save_consensus_state();

        // Note: if finalize_committed_block spawned a background import (Case B),
        // we do NOT schedule the next payload build here. It will be triggered by
        // handle_import_done when the background import completes.
        // Case A (block already in reth) schedules the build immediately.
    }

    async fn finalize_committed_block(
        &mut self,
        view: u64,
        block_hash: B256,
        commit_qc: QuorumCertificate,
    ) {
        let engine_handle = match &self.beacon_engine {
            Some(h) => h.clone(),
            None => {
                self.pending_block_data.clear();
                self.pending_executions.clear();
                return;
            }
        };

        let fcu_state = ForkchoiceState {
            head_block_hash: block_hash,
            safe_block_hash: block_hash,
            finalized_block_hash: block_hash,
        };

        let fcu_start = std::time::Instant::now();
        let finalized = match engine_handle
            .fork_choice_updated(fcu_state, None, EngineApiMessageVersion::default())
            .await
        {
            Ok(result) => {
                let elapsed_ms = fcu_start.elapsed().as_millis() as u64;
                info!(target: "n42::cl::consensus_loop", view, %block_hash, status = ?result.payload_status.status, elapsed_ms, "N42_FCU: finalize fcu");
                matches!(
                    result.payload_status.status,
                    PayloadStatusEnum::Valid | PayloadStatusEnum::Accepted
                )
            }
            Err(e) => {
                warn!(target: "n42::cl::consensus_loop", view, %block_hash, error = %e, "fork_choice_updated failed");
                false
            }
        };

        // If FCU failed (block not yet in reth engine tree), check if an eager import
        // (new_payload only, no FCU) completed during the FCU round-trip. If so, retry
        // FCU — the block should now be in the engine tree and FCU will succeed.
        let finalized = if !finalized {
            let mut found_match = false;
            while let Ok((hash, ts)) = self.eager_import_done_rx.try_recv() {
                if ts > 0 {
                    self.last_committed_timestamp = self.last_committed_timestamp.max(ts);
                }
                if let Some(timing) = self.pipeline_timings.get_mut(&hash) {
                    timing.import_complete = Some(Instant::now());
                }
                if hash == block_hash {
                    found_match = true;
                }
            }
            if found_match {
                info!(target: "n42::cl::consensus_loop", view, %block_hash, "eager import completed during FCU, retrying");
                counter!("n42_eager_import_rescued_total").increment(1);
                let retry_fcu = ForkchoiceState {
                    head_block_hash: block_hash,
                    safe_block_hash: block_hash,
                    finalized_block_hash: block_hash,
                };
                let retry_start = std::time::Instant::now();
                match engine_handle
                    .fork_choice_updated(retry_fcu, None, EngineApiMessageVersion::default())
                    .await
                {
                    Ok(result) => {
                        let retry_ms = retry_start.elapsed().as_millis() as u64;
                        info!(target: "n42::cl::consensus_loop", view, %block_hash, status = ?result.payload_status.status, retry_ms, "N42_FCU_RETRY: retry fcu after eager import");
                        matches!(
                            result.payload_status.status,
                            PayloadStatusEnum::Valid | PayloadStatusEnum::Accepted
                        )
                    }
                    Err(e) => {
                        warn!(target: "n42::cl::consensus_loop", view, %block_hash, error = %e, "retry fcu failed");
                        false
                    }
                }
            } else {
                false
            }
        } else {
            true
        };

        if finalized {
            // Case A: Block already in reth (leader built + new_payload already done,
            // OR block was previously imported by a follower, OR rescued by eager import).
            debug!(target: "n42::cl::consensus_loop", view, %block_hash, "block finalized in reth");

            // Deferred state root mode: log that state root verification is pending.
            // Future: spawn async state root computation here using reth's provider.
            if reth_evm::n42_defer_state_root() {
                debug!(target: "n42::cl::consensus_loop", view, %block_hash,
                    "N42_DEFER_STATE_ROOT: block finalized with deferred state root (B256::ZERO)");
            }
            self.pending_block_data.clear();
            self.pending_executions.clear();
            if let Some(ref tx) = self.mobile_packet_tx
                && let Err(e) = tx.try_send((block_hash, view))
            {
                warn!(target: "n42::cl::consensus_loop", view, %block_hash, error = %e, "mobile_packet_tx full or closed, verification packet lost");
            }
            // Case A: block already in reth — schedule next build via slot timing.
            if self.engine.is_current_leader() {
                if self.speculative_build_hash == Some(block_hash) {
                    debug!(target: "n42::cl::consensus_loop", next_view = self.engine.current_view(), "leader: speculative build already in progress (Case A)");
                    // Don't clear yet — let handle_view_changed see it too.
                } else {
                    debug!(target: "n42::cl::consensus_loop", next_view = self.engine.current_view(), "leader: scheduling next build (Case A)");
                    self.schedule_payload_build().await;
                }
            }
        } else if let Some(data) = self.pending_block_data.remove(&block_hash) {
            // Case B: BlockData cached but not yet imported (deferred import path).
            info!(target: "n42::cl::consensus_loop", view, %block_hash, "queueing background import for deferred finalization");
            self.pending_block_data.clear();
            self.pending_executions.clear();
            self.enqueue_bg_import(data, block_hash, view);
        } else {
            // Block data not found — this commonly happens when the leader's own block
            // data (sent via leader_payload_tx) hasn't been processed by the select loop
            // yet because block_ready_rx was processed first, triggering the consensus
            // flow before the data was cached. Drain the leader_payload_rx to recover.
            while let Ok((hash, data)) = self.leader_payload_rx.try_recv() {
                if !self.pending_block_data.contains_key(&hash) {
                    if self.pending_block_data.len() >= super::execution_bridge::MAX_PENDING_BLOCK_DATA
                        && let Some(old_key) = self.pending_block_data.keys().next().copied()
                    {
                        self.pending_block_data.remove(&old_key);
                    }
                    self.pending_block_data.insert(hash, data);
                }
            }
            if let Some(data) = self.pending_block_data.remove(&block_hash) {
                // Recovered! Proceed as Case B.
                info!(target: "n42::cl::consensus_loop", view, %block_hash, "recovered block data from leader_payload_rx, proceeding as Case B");
                self.pending_block_data.clear();
                self.pending_executions.clear();
                self.enqueue_bg_import(data, block_hash, view);
            } else {
                // Case C: Decide arrived before BlockData — defer finalization.
                info!(target: "n42::cl::consensus_loop", view, %block_hash, "block not yet in reth, deferring finalization");
                self.defer_finalization(view, block_hash, commit_qc);
            }
        }
    }

    /// Enqueues a block for background import. If no import is in flight, spawns immediately.
    /// Otherwise queues for sequential processing (parent must be imported before child).
    fn enqueue_bg_import(&mut self, data: Vec<u8>, block_hash: B256, view: u64) {
        if self.bg_import_in_flight {
            debug!(target: "n42::cl::consensus_loop", view, %block_hash, queue_len = self.bg_import_queue.len(), "bg import busy, queueing");
            self.bg_import_queue.push_back((data, block_hash, view));
        } else {
            self.spawn_bg_import(data, block_hash, view);
        }
    }

    /// Spawns a single background import task.
    fn spawn_bg_import(&mut self, data: Vec<u8>, block_hash: B256, view: u64) {
        self.bg_import_in_flight = true;
        let done_tx = self.import_done_tx.clone();
        let eh = match &self.beacon_engine {
            Some(h) => h.clone(),
            None => return,
        };
        info!(target: "n42::cl::consensus_loop", view, %block_hash, "spawning background import");
        tokio::spawn(async move {
            let (success, block_ts) = Self::background_import(eh, &data, block_hash).await;
            let _ = done_tx.send((block_hash, view, success, block_ts));
        });
    }

    /// Runs `new_payload` + `fork_choice_updated` in a background task.
    /// Returns (success, block_timestamp).
    async fn background_import(
        engine_handle: ConsensusEngineHandle<EthEngineTypes>,
        data: &[u8],
        block_hash: B256,
    ) -> (bool, u64) {
        let broadcast: super::BlockDataBroadcast = match bincode::deserialize(data) {
            Ok(b) => b,
            Err(e) => {
                warn!(target: "n42::cl::consensus_loop", %block_hash, error = %e, "bg import: failed to deserialize block data");
                return (false, 0);
            }
        };

        // Use the direct timestamp field from the broadcast struct.
        let block_timestamp = broadcast.timestamp;

        let payload_json = match super::decompress_payload(&broadcast.payload_json) {
            Ok(d) => d,
            Err(e) => {
                warn!(target: "n42::cl::consensus_loop", %block_hash, error = %e, "bg import: failed to decompress payload");
                return (false, 0);
            }
        };
        let execution_data = match serde_json::from_slice(&payload_json) {
            Ok(d) => d,
            Err(e) => {
                warn!(target: "n42::cl::consensus_loop", %block_hash, error = %e, "bg import: failed to parse execution payload");
                return (false, 0);
            }
        };

        // Compact Block: inject execution output into payload cache before new_payload.
        if let Some(ref exec_compressed) = broadcast.execution_output
            && super::execution_bridge::compact_block_enabled()
        {
            super::execution_bridge::inject_compact_block(&block_hash, exec_compressed);
        }

        match engine_handle.new_payload(execution_data).await {
            Ok(status)
                if matches!(
                    status.status,
                    PayloadStatusEnum::Valid | PayloadStatusEnum::Accepted
                ) =>
            {
                let fcu = ForkchoiceState {
                    head_block_hash: block_hash,
                    safe_block_hash: block_hash,
                    finalized_block_hash: block_hash,
                };
                if let Err(e) = engine_handle
                    .fork_choice_updated(fcu, None, EngineApiMessageVersion::default())
                    .await
                {
                    error!(target: "n42::cl::consensus_loop", %block_hash, error = %e, "bg import: fcu failed");
                }
                info!(target: "n42::cl::consensus_loop", %block_hash, "bg import: block imported successfully");
                (true, block_timestamp)
            }
            Ok(status) => {
                warn!(target: "n42::cl::consensus_loop", %block_hash, status = ?status.status, "bg import: new_payload rejected");
                (false, 0)
            }
            Err(e) => {
                error!(target: "n42::cl::consensus_loop", %block_hash, error = %e, "bg import: new_payload failed");
                (false, 0)
            }
        }
    }

    /// Records a deferred finalization when block data has not yet been imported.
    fn defer_finalization(&mut self, view: u64, block_hash: B256, commit_qc: QuorumCertificate) {
        self.pending_finalization = Some(super::PendingFinalization {
            view,
            block_hash,
            commit_qc,
        });
        self.pending_executions.insert(block_hash);
    }

    async fn handle_view_changed(&mut self, new_view: u64) {
        counter!("n42_view_changes_total").increment(1);
        self.view_started_at = Some(tokio::time::Instant::now());
        // Note: building_on_parent is NOT cleared here. It's keyed on parent hash,
        // not view number. If a build is in-flight for the current parent, it should
        // still be guarded even after a view change. The guard naturally expires when
        // head_block_hash changes (a new block is imported/committed).

        // Reset eager import block guard on view change: the previous view's proposal
        // was not committed, so the new leader may propose a different block at the
        // same height. The guard must allow reimporting the same block number.
        self.eager_import_block_guard.store(
            self.committed_block_count,
            std::sync::atomic::Ordering::SeqCst,
        );

        info!(target: "n42::cl::consensus_loop", new_view, "view changed");

        // ViewChanged fires immediately after BlockCommitted in f=0 configs;
        // preserve pending state if a committed block is awaiting import.
        if let Some(ref pf) = self.pending_finalization {
            // If pending_finalization is stale (more than 2 views behind), force-clear
            // to prevent indefinite hangs when block data was evicted or lost.
            if new_view > pf.view + 2 {
                warn!(
                    target: "n42::cl::consensus_loop",
                    pending_view = pf.view,
                    new_view,
                    hash = %pf.block_hash,
                    "pending_finalization stale (>2 views behind), clearing and requesting sync"
                );
                counter!("n42_pending_finalization_stale_total").increment(1);
                let stale_view = pf.view;
                self.pending_finalization = None;
                self.pending_block_data.clear();
                self.pending_executions.clear();
                // Trigger sync to recover the missing block
                self.initiate_sync(stale_view, new_view);
            }
        }
        // NOTE: We intentionally do NOT clear pending_block_data here.
        // The cache is bounded by MAX_PENDING_BLOCK_DATA (16 entries) via LRU eviction
        // in handle_block_data. Clearing it would destroy block data that arrives
        // BEFORE Decide (common in fast consensus), causing finalize_committed_block
        // to fall into Case C (deferred finalization) and triggering cascading timeouts.
        self.pending_executions.clear();

        // Exhaust the empty-skip budget to force the next leader to build.
        self.consecutive_empty_skips = max_consecutive_empty_skips();

        if self.engine.is_current_leader() {
            // If a background import is in flight, do NOT build here — the head_block_hash
            // hasn't been updated yet (race condition). handle_import_done will trigger
            // the build once the import completes and head is current.
            if self.bg_import_in_flight || !self.bg_import_queue.is_empty() {
                debug!(target: "n42::cl::consensus_loop", new_view, in_flight = self.bg_import_in_flight, queued = self.bg_import_queue.len(), "leader after view change, deferring build until bg imports complete");
                self.speculative_build_hash = None;
            } else if self.speculative_build_hash.take().is_some() {
                debug!(target: "n42::cl::consensus_loop", new_view, "speculative build already in progress, skipping view-change build");
            } else {
                debug!(target: "n42::cl::consensus_loop", new_view, "became leader after view change, scheduling payload build");
                self.schedule_payload_build().await;
            }
        } else {
            // Not leader for this view — clear any stale speculative build.
            self.speculative_build_hash = None;
        }
    }

    /// Called when a background import task completes.
    pub(super) async fn handle_import_done(&mut self, hash: B256, view: u64, success: bool, block_timestamp: u64) {
        self.bg_import_in_flight = false;
        if success {
            // Pipeline: background import complete — record timing.
            if let Some(timing) = self.pipeline_timings.get_mut(&hash) {
                timing.import_complete = Some(Instant::now());
            }
            info!(target: "n42::cl::consensus_loop", %hash, view, "background import completed, updating head");
            self.head_block_hash = hash;
            if block_timestamp > 0 {
                self.last_committed_timestamp = self.last_committed_timestamp.max(block_timestamp);
            }
            if let Some(ref tx) = self.mobile_packet_tx
                && let Err(e) = tx.try_send((hash, view))
            {
                warn!(target: "n42::cl::consensus_loop", view, %hash, error = %e, "mobile_packet_tx full or closed");
            }
        } else {
            warn!(target: "n42::cl::consensus_loop", %hash, view, "background import FAILED, requesting sync");
            counter!("n42_bg_import_failures_total").increment(1);
            // Clear the queue — parent failed so children would fail too.
            self.bg_import_queue.clear();
            self.initiate_sync(view, self.engine.current_view());
            return;
        }

        // Process next queued import, or trigger build if queue is empty.
        if let Some((data, next_hash, next_view)) = self.bg_import_queue.pop_front() {
            debug!(target: "n42::cl::consensus_loop", view = next_view, %next_hash, remaining = self.bg_import_queue.len(), "processing next queued bg import");
            self.spawn_bg_import(data, next_hash, next_view);
        } else if self.engine.is_current_leader() {
            debug!(target: "n42::cl::consensus_loop", next_view = self.engine.current_view(), "leader: scheduling build after all bg imports done");
            self.schedule_payload_build().await;
        }
    }

    /// Called when an eager import task (leader or follower) completes successfully.
    /// The block is now in reth's engine tree (via new_payload only, no FCU).
    /// We record this so finalize_committed_block knows the block is pre-validated
    /// and its FCU will succeed immediately (Case A).
    pub(super) async fn handle_eager_import_done(&mut self, hash: B256, block_ts: u64) {
        // Record the pre-imported block hash. When finalize_committed_block runs,
        // it will find this block already in reth's engine tree, making FCU instant.
        // We do NOT update head_block_hash here — that should only change via FCU
        // in finalize_committed_block, to avoid canonical chain reorgs from
        // speculative blocks that consensus may not ultimately commit.
        if block_ts > 0 {
            self.last_committed_timestamp = self.last_committed_timestamp.max(block_ts);
        }
        if let Some(timing) = self.pipeline_timings.get_mut(&hash) {
            timing.import_complete = Some(Instant::now());
        }
        debug!(target: "n42::cl::consensus_loop", %hash, "eager import done: block in engine tree (awaiting consensus commit)");
    }

    pub(super) async fn handle_leader_payload_feedback(&mut self, hash: B256, data: Vec<u8>) {
        // Cache the block data for deferred import during finalization.
        // The leader skips new_payload during building (deferred-import optimization),
        // so the block data must be available in pending_block_data for Case B in
        // finalize_committed_block().  GossipSub does not echo to self, so without
        // this cache the leader's own block would fall to Case C (missing data).
        if !self.pending_block_data.contains_key(&hash) {
            if self.pending_block_data.len() >= super::execution_bridge::MAX_PENDING_BLOCK_DATA
                && let Some(old_key) = self.pending_block_data.keys().next().copied()
            {
                self.pending_block_data.remove(&old_key);
            }
            self.pending_block_data.insert(hash, data.clone());
            debug!(target: "n42::cl::consensus_loop", %hash, "cached leader block data for deferred import");
        }

        // Use the direct timestamp field from the broadcast struct.
        if let Ok(broadcast) = bincode::deserialize::<super::BlockDataBroadcast>(&data)
            && broadcast.timestamp > 0
        {
            self.last_committed_timestamp = self.last_committed_timestamp.max(broadcast.timestamp);
        }

        if let Some(block) = self.committed_blocks.iter_mut().rev().find(|b| b.block_hash == hash)
            && block.payload.is_empty()
            && let Ok(broadcast) = bincode::deserialize::<super::BlockDataBroadcast>(&data)
        {
            block.payload = broadcast.payload_json;
            debug!(target: "n42::cl::consensus_loop", %hash, "populated committed block payload from leader build task");
        }

        // If a deferred finalization is pending for this hash, complete it now.
        // This handles the race where commit arrives before leader_payload_rx.
        let should_finalize = self.pending_finalization.as_ref()
            .is_some_and(|pf| pf.block_hash == hash);
        if should_finalize {
            info!(target: "n42::cl::consensus_loop", %hash, "leader block data arrived, completing deferred finalization");
            if let Some(pf) = self.pending_finalization.take() {
                self.finalize_committed_block(pf.view, pf.block_hash, pf.commit_qc).await;
            }
        }
    }

    /// Try to extract a `StateDiff` from serialized `BlockDataBroadcast` for JMT update.
    /// Returns `None` if the data is missing, malformed, or has no compact block execution.
    fn extract_state_diff_for_jmt(data: &[u8]) -> Option<n42_execution::state_diff::StateDiff> {
        let broadcast: super::BlockDataBroadcast = bincode::deserialize(data).ok()?;
        let exec_bytes = broadcast.execution_output.as_ref()?;
        let decompressed = zstd::bulk::decompress(exec_bytes, 64 * 1024 * 1024).ok()?;
        let compact: super::CompactBlockExecution = serde_json::from_slice(&decompressed).ok()?;
        Some(n42_execution::state_diff::StateDiff::from_bundle_state(&compact.bundle_state))
    }

    /// Extract decompressed CompactBlockExecution JSON bytes from raw BlockDataBroadcast.
    /// Returns `None` if the data is missing, malformed, or has no execution output.
    fn extract_bundle_state_json(data: &[u8]) -> Option<Vec<u8>> {
        let broadcast: super::BlockDataBroadcast = bincode::deserialize(data).ok()?;
        let exec_bytes = broadcast.execution_output.as_ref()?;
        zstd::bulk::decompress(exec_bytes, 64 * 1024 * 1024).ok()
    }
}
