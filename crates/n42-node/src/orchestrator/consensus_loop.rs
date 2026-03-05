use super::ConsensusOrchestrator;
use super::state_mgmt::max_consecutive_empty_skips;
use alloy_primitives::B256;
use alloy_rpc_types_engine::{ForkchoiceState, PayloadStatusEnum};
use n42_consensus::EngineOutput;
use n42_primitives::QuorumCertificate;
use reth_payload_primitives::EngineApiMessageVersion;
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
                let (slot_ts, delay) = self.next_slot_boundary();
                self.next_build_at = Some(Instant::now() + delay);
                self.next_slot_timestamp = Some(slot_ts);
            }
            return;
        }
        self.consecutive_empty_skips = 0;

        if self.slot_time.is_zero() {
            self.do_trigger_payload_build(None).await;
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
                if let Some(peer_id) = self.network.validator_peer(target) {
                    if let Err(e) = self.network.send_direct(peer_id, msg.clone()) {
                        warn!(target: "n42::cl::consensus_loop", target_validator = target, error = %e, "direct send failed, falling back to broadcast");
                        if let Err(e2) = self.network.broadcast_consensus(msg) {
                            error!(target: "n42::cl::consensus_loop", target_validator = target, error = %e2, "broadcast fallback also failed");
                        }
                    }
                } else {
                    // Peer not yet identified — broadcast as fallback; this is normal
                    // during startup or when the Identify exchange hasn't completed.
                    debug!(target: "n42::cl::consensus_loop", target_validator = target, "validator peer unknown, broadcasting");
                    if let Err(e) = self.network.broadcast_consensus(msg) {
                        error!(target: "n42::cl::consensus_loop", error = %e, "failed to send message to validator (broadcast fallback)");
                    }
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
        debug!(
            target: "n42::cl::consensus_loop",
            %block_hash,
            pending_data = self.pending_block_data.contains_key(&block_hash),
            "ExecuteBlock requested"
        );

        if let Some(data) = self.pending_block_data.remove(&block_hash) {
            match bincode::deserialize(&data) {
                Ok(broadcast) => {
                    self.import_and_notify(broadcast).await;
                }
                Err(e) => {
                    warn!(target: "n42::cl::consensus_loop", %block_hash, error = %e, "failed to deserialize cached block data");
                }
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
        let elapsed_ms = self.view_started_at.take().map(|t| t.elapsed().as_millis() as u64);
        if let Some(ms) = elapsed_ms {
            histogram!("n42_consensus_commit_latency_ms").record(ms as f64);
        }
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
            "view committed"
        );

        if let Some(ref state) = self.consensus_state {
            state.update_committed_qc(commit_qc.clone());
            state.notify_block_committed(block_hash, view);
        }

        self.store_committed_block(view, block_hash, commit_qc.clone());
        self.head_block_hash = block_hash;

        // Scan committed block for staking/unstaking transactions.
        if let Some(ref staking_mgr) = self.staking_manager
            && let Some(data) = self.committed_blocks.back().map(|b| b.payload.as_slice())
            && !data.is_empty()
        {
            let mut mgr = staking_mgr.lock().unwrap_or_else(|e| e.into_inner());
            mgr.scan_committed_block(self.committed_block_count, data);
        }

        self.finalize_committed_block(view, block_hash, commit_qc).await;
        self.save_consensus_state();

        if self.engine.is_current_leader() {
            debug!(target: "n42::cl::consensus_loop", next_view = self.engine.current_view(), "leader for next view");
            self.schedule_payload_build().await;
        }
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

        let finalized = match engine_handle
            .fork_choice_updated(fcu_state, None, EngineApiMessageVersion::default())
            .await
        {
            Ok(result) => {
                debug!(target: "n42::cl::consensus_loop", view, %block_hash, status = ?result.payload_status.status, "fcu result");
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

        if finalized {
            // Case A: Block already in reth (leader path, or fast follower).
            debug!(target: "n42::cl::consensus_loop", view, %block_hash, "block finalized in reth");
            self.pending_block_data.clear();
            self.pending_executions.clear();
            if let Some(ref tx) = self.mobile_packet_tx
                && let Err(e) = tx.try_send((block_hash, view))
            {
                warn!(target: "n42::cl::consensus_loop", view, %block_hash, error = %e, "mobile_packet_tx full or closed, verification packet lost");
            }
        } else if let Some(data) = self.pending_block_data.remove(&block_hash) {
            // Case B: BlockData cached but not yet imported.
            info!(target: "n42::cl::consensus_loop", view, %block_hash, "block data cached, importing for deferred finalization");
            match bincode::deserialize(&data) {
                Ok(broadcast) => {
                    self.import_and_notify(broadcast).await;
                    self.pending_block_data.clear();
                    self.pending_executions.clear();
                    if let Some(ref tx) = self.mobile_packet_tx
                        && let Err(e) = tx.try_send((block_hash, view))
                    {
                        warn!(target: "n42::cl::consensus_loop", view, %block_hash, error = %e, "mobile_packet_tx full or closed, verification packet lost");
                    }
                }
                Err(e) => {
                    warn!(target: "n42::cl::consensus_loop", %block_hash, error = %e, "failed to deserialize cached block data");
                    self.defer_finalization(view, block_hash, commit_qc);
                }
            }
        } else {
            // Case C: Decide arrived before BlockData — defer finalization.
            info!(target: "n42::cl::consensus_loop", view, %block_hash, "block not yet in reth, deferring finalization");
            self.defer_finalization(view, block_hash, commit_qc);
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
        } else {
            self.pending_block_data.clear();
            self.pending_executions.clear();
        }

        // Exhaust the empty-skip budget to force the next leader to build.
        self.consecutive_empty_skips = max_consecutive_empty_skips();

        if self.engine.is_current_leader() {
            debug!(target: "n42::cl::consensus_loop", new_view, "became leader after view change, triggering payload build");
            self.schedule_payload_build().await;
        }
    }

    pub(super) fn handle_leader_payload_feedback(&mut self, hash: B256, data: Vec<u8>) {
        if let Some(block) = self.committed_blocks.iter_mut().rev().find(|b| b.block_hash == hash)
            && block.payload.is_empty()
            && let Ok(broadcast) = bincode::deserialize::<super::BlockDataBroadcast>(&data)
        {
            block.payload = broadcast.payload_json;
            debug!(target: "n42::cl::consensus_loop", %hash, "populated committed block payload from leader build task");
        }
    }
}
