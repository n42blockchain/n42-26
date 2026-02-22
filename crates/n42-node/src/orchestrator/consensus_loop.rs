use super::ConsensusOrchestrator;
use super::state_mgmt::max_consecutive_empty_skips;
use alloy_primitives::B256;
use alloy_rpc_types_engine::{ForkchoiceState, PayloadStatusEnum};
use n42_consensus::EngineOutput;
use n42_primitives::QuorumCertificate;
use reth_payload_primitives::EngineApiMessageVersion;
use std::time::Duration;
use tokio::time::Instant;
use metrics::{counter, gauge};
use tracing::{debug, error, info, warn};

impl ConsensusOrchestrator {
    /// Returns true if the last few committed blocks all had empty (or near-empty) payloads.
    /// Used to decide whether to skip empty-block proposals.
    pub(super) fn recent_blocks_empty(&self) -> bool {
        let check_count = 3.min(self.committed_blocks.len());
        if check_count == 0 {
            return false;
        }
        self.committed_blocks
            .iter()
            .rev()
            .take(check_count)
            .all(|b| b.payload.is_empty() || b.payload.len() < 512)
    }

    /// Schedules the next payload build, applying empty-block skip optimization.
    ///
    /// If slot_time > 0, computes the next wall-clock boundary and delays until then.
    /// If the mempool looks empty and skip budget remains, defers to the next slot.
    pub(super) async fn schedule_payload_build(&mut self) {
        if self.recent_blocks_empty()
            && self.consecutive_empty_skips < max_consecutive_empty_skips()
        {
            self.consecutive_empty_skips += 1;
            counter!("n42_empty_block_skips_total").increment(1);
            debug!(
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
                slot_timestamp = slot_ts,
                delay_ms = delay.as_millis() as u64,
                "scheduled payload build at next slot boundary"
            );
        }
    }

    /// Computes the next wall-clock-aligned slot boundary.
    ///
    /// Returns `(slot_timestamp_secs, delay_until_boundary)`.
    /// Slot boundaries are defined as the smallest multiple of `slot_ms` greater than `now_ms`.
    pub(super) fn next_slot_boundary(&self) -> (u64, Duration) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let now_ms = now.as_millis() as u64;
        let slot_ms = self.slot_time.as_millis() as u64;

        if slot_ms == 0 {
            tracing::error!("slot_time is zero, defaulting to 1-second delay");
            return (now_ms / 1000 + 1, Duration::from_secs(1));
        }

        let current_slot = now_ms / slot_ms;
        let next_boundary_ms = (current_slot + 1) * slot_ms;
        let delay_ms = next_boundary_ms - now_ms;
        let slot_timestamp = next_boundary_ms / 1000;

        (slot_timestamp, Duration::from_millis(delay_ms))
    }

    /// Dispatches an engine output to the appropriate destination.
    pub(super) async fn handle_engine_output(&mut self, output: EngineOutput) {
        match output {
            EngineOutput::BroadcastMessage(msg) => {
                if let Err(e) = self.network.broadcast_consensus(msg) {
                    error!(error = %e, "failed to broadcast consensus message");
                }
            }
            EngineOutput::SendToValidator(target, msg) => {
                if let Some(peer_id) = self.network.validator_peer(target) {
                    if let Err(e) = self.network.send_direct(peer_id, msg.clone()) {
                        warn!(target_validator = target, error = %e, "direct send failed, falling back to broadcast");
                        let _ = self.network.broadcast_consensus(msg);
                    }
                } else if let Err(e) = self.network.broadcast_consensus(msg) {
                    error!(error = %e, "failed to send message to validator (broadcast fallback)");
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
                self.initiate_sync(local_view, target_view);
            }
            EngineOutput::EquivocationDetected { view, validator, hash1, hash2 } => {
                counter!("n42_equivocations_detected_total").increment(1);
                warn!(
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
                info!(new_epoch, validator_count, "epoch transition: validator set updated");
                let updated_vs = self.engine.epoch_manager().current_validator_set().clone();
                self.validator_set_for_sync = Some(updated_vs);
            }
        }
    }

    async fn handle_execute_block(&mut self, block_hash: B256) {
        debug!(
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
                    warn!(%block_hash, error = %e, "failed to deserialize cached block data");
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
        counter!("n42_blocks_committed_total").increment(1);
        gauge!("n42_consensus_view").set(view as f64);
        info!(view, %block_hash, "block committed by consensus");

        if let Some(ref state) = self.consensus_state {
            state.update_committed_qc(commit_qc.clone());
            state.notify_block_committed(block_hash, view);
        }

        self.store_committed_block(view, block_hash, commit_qc.clone());
        self.head_block_hash = block_hash;

        self.finalize_committed_block(view, block_hash, commit_qc).await;
        self.save_consensus_state();

        if self.engine.is_current_leader() {
            info!(next_view = self.engine.current_view(), "leader for next view");
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
                debug!(view, %block_hash, status = ?result.payload_status.status, "fcu result");
                matches!(
                    result.payload_status.status,
                    PayloadStatusEnum::Valid | PayloadStatusEnum::Accepted
                )
            }
            Err(e) => {
                warn!(view, %block_hash, error = %e, "fork_choice_updated failed");
                false
            }
        };

        if finalized {
            // Case A: Block already in reth (leader path, or fast follower).
            debug!(view, %block_hash, "block finalized in reth");
            self.pending_block_data.clear();
            self.pending_executions.clear();
            if let Some(ref tx) = self.mobile_packet_tx {
                let _ = tx.try_send((block_hash, view));
            }
        } else if let Some(data) = self.pending_block_data.remove(&block_hash) {
            // Case B: BlockData cached but not yet imported.
            info!(view, %block_hash, "block data cached, importing for deferred finalization");
            match bincode::deserialize(&data) {
                Ok(broadcast) => {
                    self.import_and_notify(broadcast).await;
                    self.pending_block_data.clear();
                    self.pending_executions.clear();
                    if let Some(ref tx) = self.mobile_packet_tx {
                        let _ = tx.try_send((block_hash, view));
                    }
                }
                Err(e) => {
                    warn!(%block_hash, error = %e, "failed to deserialize cached block data");
                    self.pending_finalization = Some(super::PendingFinalization {
                        view,
                        block_hash,
                        commit_qc,
                    });
                    self.pending_executions.insert(block_hash);
                }
            }
        } else {
            // Case C: Decide arrived before BlockData — defer finalization.
            info!(view, %block_hash, "block not yet in reth, deferring finalization");
            self.pending_finalization = Some(super::PendingFinalization {
                view,
                block_hash,
                commit_qc,
            });
            self.pending_executions.insert(block_hash);
        }
    }

    async fn handle_view_changed(&mut self, new_view: u64) {
        counter!("n42_view_changes_total").increment(1);
        info!(new_view, "view changed");

        // Preserve pending state if a committed block is awaiting import.
        // ViewChanged fires immediately after BlockCommitted in f=0 configs.
        if self.pending_finalization.is_none() {
            self.pending_block_data.clear();
            self.pending_executions.clear();
        }

        // Force the next leader to build by exhausting the empty-skip budget.
        // Resetting to 0 would still allow skipping; setting to max bypasses it.
        self.consecutive_empty_skips = max_consecutive_empty_skips();

        if self.engine.is_current_leader() {
            info!(new_view, "became leader after view change, triggering payload build");
            self.schedule_payload_build().await;
        }
    }

    /// Populates the committed_blocks entry for a leader-built block when
    /// the build task sends back the payload data.
    pub(super) fn handle_leader_payload_feedback(&mut self, hash: B256, data: Vec<u8>) {
        if let Some(block) = self.committed_blocks.iter_mut().rev().find(|b| b.block_hash == hash) {
            if block.payload.is_empty() {
                if let Ok(broadcast) = bincode::deserialize::<super::BlockDataBroadcast>(&data) {
                    block.payload = broadcast.payload_json;
                    debug!(%hash, "populated committed block payload from leader build task");
                }
            }
        }
    }
}
