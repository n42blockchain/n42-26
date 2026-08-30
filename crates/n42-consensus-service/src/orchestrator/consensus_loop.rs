use super::{AsyncCommitStage, AsyncCommitState, ConsensusService, ImportOutcome};
use super::{LeaderBuildWaitMode, state_mgmt::max_consecutive_empty_skips};
use crate::el::{ExecutionLayer, ExecutionPath};
use crate::exec_cache::ExecutionOutputCache;
use crate::expected_validator_peer_ids_with_policy;
use alloy_primitives::B256;
use alloy_rpc_types_engine::{ForkchoiceState, PayloadStatusEnum};
use metrics::{counter, gauge, histogram};
use n42_consensus::EngineOutput;
use n42_primitives::{ConsensusMessage, QuorumCertificate};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;
use tracing::{debug, error, info, warn};

/// Number of recent blocks to check for emptiness before skipping proposal.
const EMPTY_BLOCK_CHECK_WINDOW: usize = 3;

/// Payload size below which a block is considered "empty" (no meaningful transactions).
const EMPTY_BLOCK_SIZE_THRESHOLD: usize = 512;

fn commit_execution_ready_timeout() -> Duration {
    static TIMEOUT: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *TIMEOUT.get_or_init(|| {
        Duration::from_millis(
            std::env::var("N42_COMMIT_EXECUTION_READY_TIMEOUT_MS")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(150)
                .clamp(10, 5_000),
        )
    })
}

impl ConsensusService {
    pub(super) fn recent_blocks_empty(&self) -> bool {
        let check_count = EMPTY_BLOCK_CHECK_WINDOW.min(self.committed_blocks.len());
        if check_count == 0 {
            return false;
        }
        self.committed_blocks
            .iter()
            .rev()
            .take(check_count)
            .all(|b| b.payload_len < EMPTY_BLOCK_SIZE_THRESHOLD)
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
            static MIN_PROPOSE_DELAY: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
            let min_delay_ms: u64 = *MIN_PROPOSE_DELAY.get_or_init(|| {
                std::env::var("N42_MIN_PROPOSE_DELAY_MS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0)
            });
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
                if self.h2_v4_identity.is_some() {
                    if let Err(e) = self.broadcast_engine_consensus(msg) {
                        error!(target: "n42::interop::h2v4", error = %e, "failed to broadcast H2-v4 consensus message");
                    }
                } else if matches!(&msg, ConsensusMessage::Proposal(_))
                    && self.engine.is_current_leader()
                {
                    self.broadcast_via_rotor(msg);
                } else {
                    // Timeout votes are byte-identical when re-broadcast in the same view.
                    // GossipSub therefore rejects the repeat as a duplicate and a validator
                    // that reconnects after the first publication can never contribute to the
                    // TC. Send timeout/NewView control messages directly to every currently
                    // known validator as well as through GossipSub. This is intentionally
                    // limited to view-change traffic: timeouts are rare, and the direct path
                    // makes late-join recovery independent of GossipSub's message-id cache.
                    if matches!(
                        &msg,
                        ConsensusMessage::Timeout(_) | ConsensusMessage::NewView(_)
                    ) {
                        let my_index = self.engine.my_index();
                        for (validator_index, peer_id) in self.network.all_validator_peers() {
                            if validator_index == my_index {
                                continue;
                            }
                            if let Err(error) = self.network.send_direct(peer_id, msg.clone()) {
                                warn!(
                                    target: "n42::cl::consensus_loop",
                                    validator_index,
                                    %peer_id,
                                    %error,
                                    "failed to send view-change control message directly"
                                );
                            }
                        }
                    }
                    // Fire-and-forget: do NOT await — blocking the consensus loop on
                    // network I/O causes event starvation under 48K+ load, leading to
                    // delayed block imports and invalid-ancestor cascades.
                    if let Err(e) = self.network.broadcast_consensus(msg) {
                        error!(target: "n42::cl::consensus_loop", error = %e, "failed to broadcast consensus message");
                    }
                }
            }
            EngineOutput::SendToValidator(target, msg) => {
                // Direct send + broadcast for reliability. Direct send via request-response
                // can silently fail (QUIC transport issues), so always broadcast as backup.
                // The consensus engine deduplicates votes by (view, voter).
                let send_start = std::time::Instant::now();
                if matches!(
                    msg,
                    ConsensusMessage::Vote(_) | ConsensusMessage::CommitVote(_)
                ) {
                    self.pending_vote_resend = Some(super::PendingVoteResend {
                        view: msg.view(),
                        target,
                        message: msg.clone(),
                        next_at: tokio::time::Instant::now() + Self::vote_resend_delay(),
                    });
                }
                if self.h2_v4_identity.is_none()
                    && let Some(peer_id) = self.network.validator_peer(target)
                {
                    let _ = self.network.send_direct(peer_id, msg.clone());
                }
                // Fire-and-forget broadcast (non-blocking).
                if let Err(e) = self.broadcast_engine_consensus(msg) {
                    error!(target: "n42::cl::consensus_loop", target_validator = target, error = %e, "broadcast failed");
                }
                histogram!("n42_send_to_validator_e2e_ms")
                    .record(send_start.elapsed().as_millis() as f64);
            }
            EngineOutput::ExecuteBlock(block_hash) => {
                self.handle_execute_block(block_hash).await;
            }
            EngineOutput::BlockCommitted {
                view,
                block_hash,
                commit_qc,
                validator_changes,
            } => {
                self.handle_block_committed(view, block_hash, commit_qc, validator_changes)
                    .await;
            }
            EngineOutput::CommittedBlockValidatorChangesRecovered {
                view,
                block_hash,
                validator_changes,
            } => {
                self.recover_committed_block_validator_changes(view, block_hash, validator_changes);
                self.refresh_epoch_status();
            }
            EngineOutput::ViewChanged { new_view } => {
                self.handle_view_changed(new_view).await;
            }
            EngineOutput::SyncRequired {
                local_view,
                target_view,
            } => {
                counter!("n42_sync_required_total").increment(1);
                self.initiate_sync(local_view, target_view);
            }
            EngineOutput::EquivocationDetected {
                view,
                validator,
                hash1,
                hash2,
            } => {
                counter!("n42_equivocations_detected_total").increment(1);
                warn!(
                    target: "n42::cl::consensus_loop",
                    view,
                    validator,
                    %hash1,
                    %hash2,
                    "EQUIVOCATION: validator signed two conflicting consensus values in same view"
                );
                if let Some(ref state) = self.consensus_state {
                    state.record_equivocation(view, validator, hash1, hash2);
                }
            }
            EngineOutput::EpochTransition {
                new_epoch,
                validator_count,
            } => {
                info!(target: "n42::cl::consensus_loop", new_epoch, validator_count, "epoch transition: validator set updated");
                counter!("n42_epoch_transitions_total").increment(1);
                gauge!("n42_epoch_validator_count").set(validator_count as f64);

                let updated_vs = self.engine.epoch_manager().current_validator_set().clone();
                self.validator_set_for_sync = Some(updated_vs.clone());
                if let Some(state) = &self.consensus_state {
                    state.update_validator_set(updated_vs);
                }
                match expected_validator_peer_ids_with_policy(
                    &self
                        .engine
                        .epoch_manager()
                        .current_validator_set()
                        .validator_infos(),
                    self.allow_deterministic_validator_peers,
                ) {
                    Ok(peers) => {
                        if let Err(error) = self
                            .network
                            .replace_expected_validator_peers_reliable(peers)
                            .await
                        {
                            warn!(
                                target: "n42::cl::consensus_loop",
                                error = %error,
                                "failed to refresh validator peer bindings after epoch transition"
                            );
                        }
                    }
                    Err(error) => {
                        warn!(
                            target: "n42::cl::consensus_loop",
                            error = %error,
                            "failed to derive validator peer bindings for new epoch"
                        );
                        if let Err(replace_error) = self
                            .network
                            .replace_expected_validator_peers_reliable(HashMap::new())
                            .await
                        {
                            warn!(
                                target: "n42::cl::consensus_loop",
                                error = %replace_error,
                                "failed to clear validator peer bindings after epoch transition policy error"
                            );
                        }
                    }
                }

                // Update network's validator context for Rotor relay
                self.network
                    .set_validator_context(self.engine.my_index(), validator_count)
                    .await;

                // Pre-stage the validator set for the epoch after next, if scheduled.
                if let Some(schedule) = &self.epoch_schedule
                    && let Some((validators, threshold)) = schedule.get_for_epoch(new_epoch + 1)
                {
                    match self
                        .engine
                        .epoch_manager_mut()
                        .stage_next_epoch(validators, threshold)
                    {
                        Ok(()) => {
                            info!(
                                target: "n42::cl::consensus_loop",
                                next_epoch = new_epoch + 1,
                                validator_count = validators.len(),
                                fault_tolerance = threshold,
                                "staged next epoch validator set from schedule"
                            );
                        }
                        Err(error) => {
                            warn!(
                                target: "n42::cl::consensus_loop",
                                next_epoch = new_epoch + 1,
                                validator_count = validators.len(),
                                fault_tolerance = threshold,
                                error = %error,
                                "failed to stage next epoch validator set from schedule"
                            );
                        }
                    }
                }

                // Update RPC-visible epoch status after the transition.
                self.refresh_epoch_status();
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
            if self.h2_v4_identity.is_none() {
                // Native mode preserves its established optimistic/deferred
                // execution behavior. H2 participant mode waits for the eager
                // new_payload result before emitting BlockImported.
                info!(target: "n42::cl::consensus_loop", %block_hash, "block data available, sending BlockImported to consensus");
                if let Err(e) = self
                    .engine
                    .process_event(n42_consensus::ConsensusEvent::BlockImported(block_hash))
                {
                    error!(target: "n42::cl::consensus_loop", %block_hash, error = %e, "error processing BlockImported");
                }
            }
        } else {
            self.pending_executions.insert(block_hash);
        }
    }

    pub(super) async fn handle_block_committed(
        &mut self,
        view: u64,
        block_hash: B256,
        commit_qc: QuorumCertificate,
        validator_changes: Option<Vec<n42_primitives::consensus::ValidatorChange>>,
    ) {
        let commit_now = Instant::now();
        let pool_depth = self.pool_depth_snapshot();
        // The leader receives its own raw payload through a local feedback
        // channel rather than the network announcement path. Drain anything
        // already available before deriving the execution lineage.
        self.drain_leader_payload_rx(&[block_hash]);
        // Consensus output has higher select-loop priority than eager-import
        // completions. Drain completions here so already validated execution
        // metadata is reused instead of decompressing and parsing the same
        // multi-megabyte payload again on the commit path.
        let mut eager_completions = Vec::new();
        let mut current_completion_seen = false;
        while let Ok((hash, block_ts, parent_hash, block_number, has_staking_target, state_diff)) =
            self.eager_import_done_rx.try_recv()
        {
            current_completion_seen |= hash == block_hash;
            self.record_eager_import_completion(
                hash,
                block_ts,
                parent_hash,
                block_number,
                has_staking_target,
                state_diff,
            );
            // The large state diff has moved into recent execution metadata.
            // Keep only the small fields needed for authenticated post-processing.
            eager_completions.push((hash, block_number, has_staking_target));
        }
        // Never sleep in the consensus output handler. The lifecycle below
        // retains the CommitQC at `Committed` until the matching completion
        // arrives, which gives the old 150 ms grace period its safety without
        // putting that latency on the select loop.
        histogram!("n42_commit_eager_metadata_wait_ms").record(0.0);
        counter!(
            "n42_commit_eager_metadata_wait_total",
            "outcome" => if current_completion_seen { "immediate_hit" } else { "async" }
        )
        .increment(1);
        // A prepared block can survive a timeout as LockedQC and become an
        // execution ancestor of the next block that obtains a CommitQC. Reth
        // then advances by more than one block for a single consensus commit.
        // Recover that complete raw lineage before any pending caches are
        // cleared so block numbering and QMDB/Twig sidecars do not skip it.
        let execution_lineage = self.recent_execution_lineage(block_hash);
        let lineage_ready_at = Instant::now();
        if let Some(prev_commit) = self.last_commit_instant {
            let inter_block_commit_ms = commit_now.duration_since(prev_commit).as_millis() as u64;
            histogram!("n42_inter_block_commit_ms").record(inter_block_commit_ms as f64);
            info!(
                target: "n42::cl::consensus_loop",
                view,
                %block_hash,
                prev_view = self.last_commit_view.unwrap_or_default(),
                prev_hash = ?self.last_commit_hash,
                inter_block_commit_ms,
                pool_pending = pool_depth.pending,
                pool_queued = pool_depth.queued,
                async_finalize_fcu = self.async_finalize_fcu,
                "N42_CADENCE: inter-block commit interval"
            );
        }
        self.last_commit_instant = Some(commit_now);
        self.last_commit_view = Some(view);
        self.last_commit_hash = Some(block_hash);

        let previous_block_count = self.committed_block_count;
        let committed_execution_number = execution_lineage
            .last()
            .and_then(|entry| entry.execution_block_number)
            .or_else(|| {
                self.recent_block_data
                    .iter()
                    .find(|entry| entry.block_hash == block_hash)
                    .and_then(|entry| entry.execution_block_number)
            })
            .or_else(|| {
                self.pending_block_data
                    .get(&block_hash)
                    .and_then(|data| Self::execution_parent_and_number(data))
                    .map(|(_, block_number)| block_number)
            });
        match committed_execution_number {
            Some(block_number) => {
                self.committed_block_count = self.committed_block_count.max(block_number);
            }
            None => self.committed_block_count += 1,
        }
        let execution_blocks_committed = self
            .committed_block_count
            .saturating_sub(previous_block_count)
            .max(1);
        // This counter describes consensus CommitQC events, not the number of
        // execution blocks finalized transitively by one event.
        counter!("n42_blocks_committed_total").increment(1);
        counter!("n42_execution_blocks_finalized_total").increment(execution_blocks_committed);
        if execution_blocks_committed > 1 {
            info!(
                target: "n42::cl::consensus_loop",
                view,
                %block_hash,
                execution_blocks_committed,
                previous_block_count,
                committed_block_count = self.committed_block_count,
                "CommitQC finalized a prepared execution ancestor lineage"
            );
            counter!("n42_implicit_ancestor_commits_total")
                .increment(execution_blocks_committed - 1);
        }
        gauge!("n42_consensus_view").set(view as f64);
        gauge!("n42_pool_pending_at_commit").set(pool_depth.pending as f64);
        gauge!("n42_pool_queued_at_commit").set(pool_depth.queued as f64);
        info!(
            target: "n42::cl::consensus_loop",
            view,
            %block_hash,
            pool_pending = pool_depth.pending,
            pool_queued = pool_depth.queued,
            async_finalize_fcu = self.async_finalize_fcu,
            "N42_POOL_AT_COMMIT"
        );
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
            timing.committed = Some(commit_now);
            timing.emit_metrics();
            Some(timing.summary())
        } else {
            None
        };

        let consensus_timing = self
            .engine
            .last_committed_view_timing()
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
        }

        self.prev_randao_cache =
            alloy_primitives::keccak256(commit_qc.aggregate_signature.to_bytes());
        self.last_commit_qc = Some(commit_qc.clone());

        // Build and persist ConsensusEvidence in MDBX for future verification.
        // Note: evidence root is NOT placed in the block header (parent_beacon_block_root
        // is always B256::ZERO for Cancun compatibility).
        // committed_block_count was aligned to the execution payload above; it
        // equals the new block's number even when a prepared ancestor was
        // finalized transitively by this CommitQC.
        if let Some(ref evidence_store) = self.evidence_store {
            let evidence = n42_jmt::ConsensusEvidence {
                view: commit_qc.view,
                block_hash: block_hash.0,
                aggregate_signature: commit_qc.aggregate_signature.to_bytes(),
                signer_count: u16::try_from(commit_qc.signers.len()).unwrap_or(u16::MAX),
                packed_signers: commit_qc.signers.as_raw_slice().to_vec(),
                // Filled later by the node-side mobile evidence write-back task;
                // committee finality never waits for phone attestations.
                mobile: None,
            };
            let encoded = evidence.encode();
            let root_bytes = *blake3::hash(&encoded).as_bytes();

            let store = Arc::clone(evidence_store);
            let block_number = self.committed_block_count;
            // Fire-and-forget: do NOT await MDBX write on the consensus hot path.
            // Blocking here under 48K load causes event starvation.
            // Evidence may be lost on crash but this is acceptable — the QC
            // is already persisted in the consensus snapshot.
            tokio::task::spawn_blocking(move || {
                if let Err(e) = store.put_raw(block_number, &encoded, &root_bytes) {
                    tracing::warn!(
                        target: "n42::cl::evidence",
                        block_number, error = %e,
                        "failed to store consensus evidence"
                    );
                }
            });
        }

        // Refresh RPC-visible epoch status (captures pending→staged transition from CommitQC).
        self.refresh_epoch_status();

        let store_started_at = Instant::now();
        self.store_committed_block(
            view,
            block_hash,
            commit_qc.clone(),
            validator_changes,
            &execution_lineage,
        );
        let store_done_at = Instant::now();
        let execution_ready = current_completion_seen
            || self
                .eager_execution_validated
                .iter()
                .any(|hash| *hash == block_hash);

        // Leader eager-imported blocks can finalize via Case A before the select
        // loop processes `leader_payload_rx`. Drain it here so state-tree
        // updater can see the local block data even when reth already has the
        // block in its engine tree.
        self.drain_leader_payload_rx(&[block_hash]);

        if let Some(ref state) = self.consensus_state {
            state.notify_block_committed(block_hash, self.committed_block_count);
        }

        // State-tree sidecars: STAGE the diff now (the broadcast data is at hand),
        // apply it only once reth confirms the block executable. Applying at
        // commit time advanced the JMT/Twig trees on blocks reth could later
        // reject - the same three-state divergence class the Go stack hit live
        // (sidecar state walking away from the authoritative state). The apply
        // now hangs off advance_execution_validated_head, the single point every
        // confirmation path (eager import, finalize FCU, bg import, sync) funnels
        // through.
        if self.jmt.is_some() || self.twig.is_some() {
            let lineage_data: Vec<(u64, B256, Option<n42_execution::state_diff::StateDiff>)> =
                if execution_lineage.is_empty() {
                    self.pending_block_data
                        .get(&block_hash)
                        .map(|_| {
                            let cached_diff = self
                                .recent_block_data
                                .iter()
                                .find(|entry| entry.block_hash == block_hash)
                                .and_then(|entry| entry.execution_state_diff.clone());
                            vec![(view, block_hash, cached_diff)]
                        })
                        .unwrap_or_default()
                } else {
                    execution_lineage
                        .iter()
                        .map(|entry| {
                            (
                                entry.view,
                                entry.block_hash,
                                entry.execution_state_diff.clone(),
                            )
                        })
                        .collect()
                };
            for (lineage_view, lineage_hash, cached_diff) in lineage_data {
                if let Some(diff) = cached_diff {
                    self.missing_sidecar_diffs.remove(&lineage_view);
                    self.pending_sidecar_diffs
                        .insert(lineage_view, (lineage_hash, diff));
                } else {
                    self.missing_sidecar_diffs
                        .insert(lineage_view, lineage_hash);
                    debug!(
                        target: "n42::twig",
                        view = lineage_view,
                        %lineage_hash,
                        "committed sidecar diff still computing; pausing QMDB/SBMT updates at this view"
                    );
                    counter!("n42_sidecar_diff_deferred_total").increment(1);
                }
            }
            // The commit may confirm execution in the same breath (eager
            // import validated before the CommitQC): the head already advanced
            // above, so flush every staged lineage diff immediately.
            if view <= self.execution_validated_head_view {
                self.enqueue_confirmed_sidecar_state_diffs(self.execution_validated_head_view);
            }
        }
        let sidecar_done_at = Instant::now();

        // ZK proof sidecar: schedule proof generation (async, non-blocking).
        // Uses committed_block_count (monotonically increasing) for interval check,
        // not view (which can skip on timeouts).
        if let Some(ref scheduler) = self.zk_scheduler
            && let Some(data) = self.pending_block_data.get(&block_hash)
        {
            let block_count = self.committed_block_count;
            // Extract decompressed CompactBlockExecution JSON from the raw
            // BlockDataBroadcast. This is the same decompress path used by
            // JMT and staking scan.
            let bundle_json = match Self::extract_bundle_state_json(block_hash, data) {
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

        // Scan committed block for staking/unstaking transactions.
        if self.staking_sink.is_some() {
            let lineage_data: Vec<(u64, B256, Option<bool>)> = if execution_lineage.is_empty() {
                // If reth already advanced to the child, or the raw child links
                // directly to the current head, this is an ordinary one-block
                // lineage. Otherwise an ancestor is missing: defer all scans so
                // catch-up can replay them in execution-number order.
                self.pending_block_data
                    .get(&block_hash)
                    .filter(|data| {
                        self.head_block_hash == block_hash
                            || Self::execution_parent_and_number(data)
                                .is_some_and(|(parent, _)| parent == self.head_block_hash)
                    })
                    .map(|_| {
                        vec![(
                            committed_execution_number.unwrap_or(self.committed_block_count),
                            block_hash,
                            self.recent_block_data
                                .iter()
                                .find(|entry| entry.block_hash == block_hash)
                                .and_then(|entry| entry.execution_has_staking_target),
                        )]
                    })
                    .unwrap_or_default()
            } else {
                execution_lineage
                    .iter()
                    .map(|entry| {
                        (
                            entry
                                .execution_block_number
                                .unwrap_or(self.committed_block_count),
                            entry.block_hash,
                            entry.execution_has_staking_target,
                        )
                    })
                    .collect()
            };
            for (block_number, lineage_hash, has_staking_target) in lineage_data {
                if let Some(has_staking_target) = has_staking_target {
                    self.queue_validated_staking_scan(
                        lineage_hash,
                        block_number,
                        has_staking_target,
                    );
                } else {
                    counter!("n42_staking_scan_deferred_total").increment(1);
                    debug!(
                        target: "n42::cl::consensus_loop",
                        block_number,
                        %lineage_hash,
                        "staking classification still validating; deferred off commit path"
                    );
                }
            }
        }
        let staking_done_at = Instant::now();

        self.enqueue_async_commit(view, block_hash, commit_qc, execution_ready);
        // Complete the authenticated H2/vote and committed-lifecycle actions
        // for messages prefetched above. They are deliberately not requeued:
        // requeueing could lose a completion when concurrent producers refill
        // the bounded channel, while cloning a potentially large StateDiff.
        for (hash, block_number, has_staking_target) in eager_completions {
            self.finish_eager_import_completion(hash, block_number, has_staking_target)
                .await;
        }
        if !execution_ready {
            let timeout_tx = self.commit_execution_timeout_tx.clone();
            tokio::spawn(async move {
                tokio::time::sleep(commit_execution_ready_timeout()).await;
                let _ = timeout_tx.send((view, block_hash)).await;
            });
        }
        self.drive_async_commits().await;
        let finalize_done_at = Instant::now();
        self.save_consensus_state();
        let persisted_at = Instant::now();
        info!(
            target: "n42::cl::consensus_loop",
            view,
            %block_hash,
            commit_to_lineage_ms = lineage_ready_at.duration_since(commit_now).as_millis() as u64,
            pre_store_ms = store_started_at.duration_since(lineage_ready_at).as_millis() as u64,
            store_ms = store_done_at.duration_since(store_started_at).as_millis() as u64,
            sidecar_ms = sidecar_done_at.duration_since(store_done_at).as_millis() as u64,
            staking_ms = staking_done_at.duration_since(sidecar_done_at).as_millis() as u64,
            finalize_dispatch_ms = finalize_done_at.duration_since(staking_done_at).as_millis() as u64,
            persist_ms = persisted_at.duration_since(finalize_done_at).as_millis() as u64,
            total_ms = persisted_at.duration_since(commit_now).as_millis() as u64,
            "N42_COMMIT_PATH: committed-block service stages"
        );
        crate::qualification_abort_at("commit_qc_persisted");

        // Note: if finalize_committed_block spawned a background import (Case B),
        // we do NOT schedule the next payload build here. It will be triggered by
        // handle_import_done when the background import completes.
        // Case A (block already in reth) schedules the build immediately.
    }

    fn enqueue_async_commit(
        &mut self,
        view: u64,
        block_hash: B256,
        commit_qc: QuorumCertificate,
        execution_ready: bool,
    ) {
        let inserted = match self.async_commit_states.entry(view) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(AsyncCommitState {
                    view,
                    block_hash,
                    commit_qc,
                    stage: AsyncCommitStage::Committed,
                    finalize_in_flight: false,
                    committed_at: Instant::now(),
                });
                true
            }
            std::collections::btree_map::Entry::Occupied(entry) => {
                if entry.get().block_hash != block_hash {
                    error!(
                        target: "n42::cl::consensus_loop",
                        view,
                        existing_hash = %entry.get().block_hash,
                        %block_hash,
                        "conflicting block in asynchronous commit lifecycle"
                    );
                    return;
                }
                false
            }
        };
        if inserted {
            counter!("n42_async_commit_transitions_total", "stage" => "committed").increment(1);
            info!(
                target: "n42::cl::consensus_loop",
                view,
                %block_hash,
                execution_ready,
                "N42_COMMIT_STATE: Committed"
            );
        }
        if execution_ready {
            self.mark_async_commit_execution_ready(block_hash);
        }
        gauge!("n42_async_commits_pending").set(
            self.async_commit_states
                .values()
                .filter(|state| state.stage != AsyncCommitStage::Finalized)
                .count() as f64,
        );
    }

    fn mark_async_commit_execution_ready(&mut self, block_hash: B256) -> bool {
        let Some(state) = self
            .async_commit_states
            .values_mut()
            .find(|state| state.block_hash == block_hash)
        else {
            return false;
        };
        if state.stage == AsyncCommitStage::Committed {
            state.stage = AsyncCommitStage::ExecutionReady;
            counter!("n42_async_commit_transitions_total", "stage" => "execution_ready")
                .increment(1);
            histogram!("n42_commit_to_execution_ready_ms")
                .record(state.committed_at.elapsed().as_secs_f64() * 1_000.0);
            info!(
                target: "n42::cl::consensus_loop",
                view = state.view,
                %block_hash,
                elapsed_ms = state.committed_at.elapsed().as_millis() as u64,
                "N42_COMMIT_STATE: ExecutionReady"
            );
        }
        true
    }

    fn mark_async_commit_finalized(&mut self, view: u64, block_hash: B256) {
        if let Some(state) = self.async_commit_states.get_mut(&view)
            && state.block_hash == block_hash
            && state.stage != AsyncCommitStage::Finalized
        {
            state.stage = AsyncCommitStage::Finalized;
            state.finalize_in_flight = false;
            counter!("n42_async_commit_transitions_total", "stage" => "finalized").increment(1);
            histogram!("n42_commit_to_finalized_ms")
                .record(state.committed_at.elapsed().as_secs_f64() * 1_000.0);
            info!(
                target: "n42::cl::consensus_loop",
                view,
                %block_hash,
                elapsed_ms = state.committed_at.elapsed().as_millis() as u64,
                "N42_COMMIT_STATE: Finalized"
            );
        }
        if let Some(index) = self
            .eager_execution_validated
            .iter()
            .position(|hash| *hash == block_hash)
        {
            self.eager_execution_validated.remove(index);
        }
        while self.async_commit_states.len() > 256
            && self
                .async_commit_states
                .first_key_value()
                .is_some_and(|(_, state)| state.stage == AsyncCommitStage::Finalized)
        {
            self.async_commit_states.pop_first();
        }
        gauge!("n42_async_commits_pending").set(
            self.async_commit_states
                .values()
                .filter(|state| state.stage != AsyncCommitStage::Finalized)
                .count() as f64,
        );
    }

    fn mark_async_commit_retryable(&mut self, view: u64, block_hash: B256) {
        if let Some(state) = self.async_commit_states.get_mut(&view)
            && state.block_hash == block_hash
        {
            state.stage = AsyncCommitStage::Committed;
            state.finalize_in_flight = false;
            counter!("n42_async_commit_transitions_total", "stage" => "retryable").increment(1);
        }
    }

    pub(super) fn handle_commit_execution_timeout(&mut self, view: u64, block_hash: B256) {
        let still_waiting = self.async_commit_states.get(&view).is_some_and(|state| {
            state.block_hash == block_hash && state.stage == AsyncCommitStage::Committed
        });
        if !still_waiting {
            return;
        }
        let retained = self
            .pending_block_data
            .get(&block_hash)
            .cloned()
            .or_else(|| {
                self.committed_blocks
                    .iter()
                    .rev()
                    .find(|block| block.block_hash == block_hash)
                    .and_then(|block| block.block_data.clone())
            });
        if let Some(data) = retained {
            counter!("n42_async_commit_execution_timeout_total", "action" => "local_import")
                .increment(1);
            warn!(
                target: "n42::cl::consensus_loop",
                view,
                %block_hash,
                timeout_ms = commit_execution_ready_timeout().as_millis() as u64,
                "ExecutionReady deadline expired; starting retained-data import off the consensus loop"
            );
            self.enqueue_bg_import(data, block_hash, view);
        } else {
            counter!("n42_async_commit_execution_timeout_total", "action" => "sync").increment(1);
            self.initiate_execution_catchup_sync(view, self.engine.current_view());
        }
    }

    /// Dispatches canonical FCUs in deterministic CommitQC order. In async-FCU
    /// mode exactly one lifecycle entry remains in flight; its completion wakes
    /// the next entry from the orchestrator select loop.
    pub(super) async fn drive_async_commits(&mut self) {
        loop {
            let next = self
                .async_commit_states
                .iter()
                .find(|(_, state)| state.stage != AsyncCommitStage::Finalized)
                .map(|(view, state)| {
                    (
                        *view,
                        state.block_hash,
                        state.commit_qc.clone(),
                        state.stage,
                        state.finalize_in_flight,
                    )
                });
            let Some((view, block_hash, commit_qc, stage, in_flight)) = next else {
                break;
            };
            if stage != AsyncCommitStage::ExecutionReady || in_flight {
                break;
            }
            if let Some(state) = self.async_commit_states.get_mut(&view) {
                state.finalize_in_flight = true;
            }
            self.finalize_committed_block(view, block_hash, commit_qc)
                .await;
            if self.async_finalize_fcu {
                break;
            }
        }
    }

    pub(super) async fn finalize_committed_block(
        &mut self,
        view: u64,
        block_hash: B256,
        commit_qc: QuorumCertificate,
    ) {
        let engine_handle = match &self.el {
            Some(el) => el.clone(),
            None => {
                self.pending_block_data.clear();
                self.pending_executions.clear();
                self.advance_execution_validated_head(
                    view,
                    block_hash,
                    "EL-less execution-ready finalize",
                );
                self.mark_async_commit_finalized(view, block_hash);
                return;
            }
        };

        let fcu_state = ForkchoiceState {
            head_block_hash: block_hash,
            safe_block_hash: block_hash,
            finalized_block_hash: block_hash,
        };

        if self.async_finalize_fcu {
            let done_tx = self.finalize_done_tx.clone();
            tokio::spawn(async move {
                let fcu_start = std::time::Instant::now();
                let finalized = match engine_handle
                    .fork_choice_updated_for(ExecutionPath::LIVE_SEQUENTIAL, fcu_state)
                    .await
                {
                    Ok(result) => {
                        let elapsed_ms = fcu_start.elapsed().as_millis() as u64;
                        let outcome = match result.payload_status.status {
                            PayloadStatusEnum::Valid => "valid",
                            PayloadStatusEnum::Syncing => "syncing",
                            PayloadStatusEnum::Accepted => "accepted",
                            PayloadStatusEnum::Invalid { .. } => "invalid",
                        };
                        counter!("n42_async_finalize_fcu_outcomes_total", "status" => outcome)
                            .increment(1);
                        histogram!("n42_fcu_latency_ms", "attempt" => "first")
                            .record(elapsed_ms as f64);
                        info!(target: "n42::cl::consensus_loop", view, %block_hash, status = ?result.payload_status.status, elapsed_ms, "N42_FCU: async finalize fcu");
                        // Only `Valid` is a real finalize. `Accepted` (stored,
                        // not executed) must not advance the validated head (F3).
                        matches!(result.payload_status.status, PayloadStatusEnum::Valid)
                    }
                    Err(e) => {
                        counter!("n42_async_finalize_fcu_outcomes_total", "status" => "error")
                            .increment(1);
                        histogram!("n42_fcu_latency_ms", "attempt" => "first_err")
                            .record(fcu_start.elapsed().as_millis() as f64);
                        warn!(target: "n42::cl::consensus_loop", view, %block_hash, error = %e, "async fork_choice_updated failed");
                        false
                    }
                };

                if done_tx
                    .send((view, block_hash, commit_qc, finalized))
                    .await
                    .is_err()
                {
                    debug!(target: "n42::cl::consensus_loop", view, %block_hash, "async finalize completion receiver dropped");
                }
            });
            counter!("n42_async_finalize_fcu_spawned_total").increment(1);
            debug!(target: "n42::cl::consensus_loop", view, %block_hash, "spawned async finalize fcu");
            return;
        }

        let fcu_start = std::time::Instant::now();
        let finalized = match engine_handle
            .fork_choice_updated_for(ExecutionPath::LIVE_SEQUENTIAL, fcu_state)
            .await
        {
            Ok(result) => {
                let elapsed_ms = fcu_start.elapsed().as_millis() as u64;
                histogram!("n42_fcu_latency_ms", "attempt" => "first").record(elapsed_ms as f64);
                info!(target: "n42::cl::consensus_loop", view, %block_hash, status = ?result.payload_status.status, elapsed_ms, "N42_FCU: finalize fcu");
                // Only `Valid` is a real finalize. `Accepted` (stored, not
                // executed) must not advance the validated head (F3).
                matches!(result.payload_status.status, PayloadStatusEnum::Valid)
            }
            Err(e) => {
                histogram!("n42_fcu_latency_ms", "attempt" => "first_err")
                    .record(fcu_start.elapsed().as_millis() as f64);
                warn!(target: "n42::cl::consensus_loop", view, %block_hash, error = %e, "fork_choice_updated failed");
                false
            }
        };

        // If FCU failed (block not yet in reth engine tree), check if an eager import
        // (new_payload only, no FCU) completed during the FCU round-trip. If so, retry
        // FCU — the block should now be in the engine tree and FCU will succeed.
        let finalized = if !finalized {
            let mut found_match = self.eager_execution_validated.contains(&block_hash);
            let mut eager_completions = Vec::new();
            while let Ok((hash, ts, parent_hash, block_number, has_staking_target, state_diff)) =
                self.eager_import_done_rx.try_recv()
            {
                self.record_eager_import_completion(
                    hash,
                    ts,
                    parent_hash,
                    block_number,
                    has_staking_target,
                    state_diff,
                );
                if hash == block_hash {
                    found_match = true;
                }
                eager_completions.push((hash, block_number, has_staking_target));
            }
            // A synchronous FCU rescue must not consume H2 completion events
            // without their vote/fetch and lifecycle post-processing.
            for (hash, block_number, has_staking_target) in eager_completions {
                self.finish_eager_import_completion(hash, block_number, has_staking_target)
                    .await;
            }
            if found_match {
                if let Some(index) = self
                    .eager_execution_validated
                    .iter()
                    .position(|hash| *hash == block_hash)
                {
                    self.eager_execution_validated.remove(index);
                }
                info!(target: "n42::cl::consensus_loop", view, %block_hash, "eager import completed during FCU, retrying");
                counter!("n42_eager_import_rescued_total").increment(1);
                let retry_fcu = ForkchoiceState {
                    head_block_hash: block_hash,
                    safe_block_hash: block_hash,
                    finalized_block_hash: block_hash,
                };
                let retry_start = std::time::Instant::now();
                match engine_handle
                    .fork_choice_updated_for(ExecutionPath::LIVE_SEQUENTIAL, retry_fcu)
                    .await
                {
                    Ok(result) => {
                        let retry_ms = retry_start.elapsed().as_millis() as u64;
                        histogram!("n42_fcu_latency_ms", "attempt" => "retry")
                            .record(retry_ms as f64);
                        info!(target: "n42::cl::consensus_loop", view, %block_hash, status = ?result.payload_status.status, retry_ms, "N42_FCU_RETRY: retry fcu after eager import");
                        // Only `Valid` is a real finalize. `Accepted` (stored,
                        // not executed) must not advance the validated head (F3).
                        matches!(result.payload_status.status, PayloadStatusEnum::Valid)
                    }
                    Err(e) => {
                        histogram!("n42_fcu_latency_ms", "attempt" => "retry_err")
                            .record(retry_start.elapsed().as_millis() as f64);
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

        self.handle_finalize_done(view, block_hash, commit_qc, finalized)
            .await;
    }

    pub(super) async fn handle_finalize_done(
        &mut self,
        view: u64,
        block_hash: B256,
        commit_qc: QuorumCertificate,
        finalized: bool,
    ) {
        if finalized {
            self.mark_async_commit_finalized(view, block_hash);
            // Case A: Block already in reth (leader built + new_payload already done,
            // OR block was previously imported by a follower, OR rescued by eager import).
            debug!(target: "n42::cl::consensus_loop", view, %block_hash, "block finalized in reth");
            self.advance_execution_validated_head(view, block_hash, "finalize fcu");

            // Deferred state root mode: log that state root verification is pending.
            // Future: spawn async state root computation here using reth's provider.
            if self.defer_state_root {
                debug!(target: "n42::cl::consensus_loop", view, %block_hash,
                    "N42_DEFER_STATE_ROOT: block finalized with deferred state root (B256::ZERO)");
            }
            self.pending_block_data.clear();
            self.pending_executions.clear();
            self.enqueue_mobile_packet(block_hash, view, "block finalized")
                .await;
            // Case A: block already in reth — schedule next build via slot timing.
            if self.engine.is_current_leader() {
                if self.speculative_build_hash == Some(block_hash) {
                    debug!(target: "n42::cl::consensus_loop", next_view = self.engine.current_view(), "leader: speculative build already in progress (Case A)");
                    // Don't clear yet — let handle_view_changed see it too.
                } else {
                    debug!(target: "n42::cl::consensus_loop", next_view = self.engine.current_view(), "leader: scheduling next build (Case A)");
                    self.begin_leader_build_wait(LeaderBuildWaitMode::Scheduled, None);
                    self.evaluate_leader_build_wait(None).await;
                }
            }
        } else if let Some(data) = self.pending_block_data.remove(&block_hash) {
            self.mark_async_commit_retryable(view, block_hash);
            // Case B: BlockData cached but not yet imported (deferred import path).
            info!(target: "n42::cl::consensus_loop", view, %block_hash, "queueing background import for deferred finalization");
            self.pending_block_data.clear();
            self.pending_executions.clear();
            self.enqueue_bg_import(data, block_hash, view);
        } else {
            self.mark_async_commit_retryable(view, block_hash);
            // Block data not found — this commonly happens when the leader's own block
            // data (sent via leader_payload_tx) hasn't been processed by the select loop
            // yet because block_ready_rx was processed first, triggering the consensus
            // flow before the data was cached. Drain the leader_payload_rx to recover.
            self.drain_leader_payload_rx(&[block_hash]);
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
    fn enqueue_bg_import(&mut self, data: super::SharedBlockData, block_hash: B256, view: u64) {
        if self
            .bad_blocks
            .should_skip(block_hash, "committed_import_enqueue")
        {
            if self
                .import_done_tx
                .try_send((block_hash, view, ImportOutcome::Invalid, 0))
                .is_err()
            {
                error!(
                    target: "n42::cl::consensus_loop",
                    view,
                    %block_hash,
                    "failed to report cached-invalid committed block"
                );
            }
            return;
        }
        if !self.bg_import_hashes.insert(block_hash) {
            counter!("n42_bg_import_duplicate_enqueue_total").increment(1);
            info!(
                target: "n42::cl::consensus_loop",
                view,
                %block_hash,
                in_flight = self.bg_import_in_flight,
                queued = self.bg_import_queue.len(),
                "bg import already queued or running, skipping duplicate enqueue"
            );
            return;
        }

        if self.bg_import_in_flight {
            debug!(target: "n42::cl::consensus_loop", view, %block_hash, queue_len = self.bg_import_queue.len(), "bg import busy, queueing");
            self.bg_import_queue.push_back((data, block_hash, view));
        } else {
            self.spawn_bg_import(data, block_hash, view);
        }
    }

    /// Spawns a single background import task.
    fn spawn_bg_import(&mut self, data: super::SharedBlockData, block_hash: B256, view: u64) {
        self.bg_import_in_flight = true;
        let done_tx = self.import_done_tx.clone();
        let eh = match &self.el {
            Some(el) => el.clone(),
            None => {
                self.bg_import_in_flight = false;
                self.bg_import_hashes.remove(&block_hash);
                return;
            }
        };
        let exec_cache = self.exec_output_cache.clone();
        let bad_blocks = self.bad_blocks.clone();
        info!(target: "n42::cl::consensus_loop", view, %block_hash, "spawning background import");
        tokio::spawn(async move {
            let (outcome, block_ts) =
                Self::background_import(eh, exec_cache, bad_blocks, data.as_slice(), block_hash)
                    .await;
            if done_tx
                .send((block_hash, view, outcome, block_ts))
                .await
                .is_err()
            {
                debug!(target: "n42::cl::consensus_loop", view, %block_hash, "background import completion receiver dropped");
            }
        });
    }

    /// Runs `new_payload` + `fork_choice_updated` in a background task.
    /// Returns (execution outcome, block_timestamp).
    async fn background_import(
        engine_handle: Arc<dyn ExecutionLayer>,
        exec_cache: Option<Arc<dyn ExecutionOutputCache>>,
        bad_blocks: super::bad_block_cache::BadBlockCache,
        data: &[u8],
        block_hash: B256,
    ) -> (ImportOutcome, u64) {
        if bad_blocks.should_skip(block_hash, "committed_import") {
            return (ImportOutcome::Invalid, 0);
        }
        let broadcast: super::BlockDataBroadcast = match bincode::deserialize(data) {
            Ok(b) => b,
            Err(e) => {
                warn!(target: "n42::cl::consensus_loop", %block_hash, error = %e, "bg import: failed to deserialize block data");
                return (ImportOutcome::Invalid, 0);
            }
        };

        // Use the direct timestamp field from the broadcast struct.
        let block_timestamp = broadcast.timestamp;

        let payload_wire = match super::decompress_payload(&broadcast.payload_json) {
            Ok(d) => d,
            Err(e) => {
                warn!(target: "n42::cl::consensus_loop", %block_hash, error = %e, "bg import: failed to decompress payload");
                return (ImportOutcome::Invalid, 0);
            }
        };
        let execution_data: alloy_rpc_types_engine::ExecutionData =
            match super::decode_execution_payload_owned(payload_wire) {
                Ok(d) => d,
                Err(e) => {
                    warn!(target: "n42::cl::consensus_loop", %block_hash, error = %e, "bg import: failed to parse execution payload");
                    return (ImportOutcome::Invalid, 0);
                }
            };
        if broadcast.block_hash != block_hash || execution_data.block_hash() != block_hash {
            warn!(
                target: "n42::cl::consensus_loop",
                %block_hash,
                envelope_hash = %broadcast.block_hash,
                payload_hash = %execution_data.block_hash(),
                "bg import: block hash mismatch; refusing untrusted envelope"
            );
            counter!("n42_block_data_payload_hash_mismatch_total").increment(1);
            return (ImportOutcome::Invalid, 0);
        }
        if bad_blocks.should_skip(block_hash, "committed_import_pre_submit") {
            return (ImportOutcome::Invalid, 0);
        }

        // Compact Block: load execution output into payload cache before `new_payload`.
        let compact_injected = if let Some(ref exec_compressed) = broadcast.execution_output
            && super::execution_bridge::compact_block_enabled()
            && let Some(ref cache) = exec_cache
        {
            cache.inject(block_hash, exec_compressed, "consensus_loop")
        } else {
            false
        };

        match engine_handle
            .new_payload_for(ExecutionPath::LIVE_SEQUENTIAL, execution_data)
            .await
        {
            Ok(status) if matches!(status.status, PayloadStatusEnum::Valid) => {
                let fcu = ForkchoiceState {
                    head_block_hash: block_hash,
                    safe_block_hash: block_hash,
                    finalized_block_hash: block_hash,
                };
                match engine_handle
                    .fork_choice_updated_for(ExecutionPath::LIVE_SEQUENTIAL, fcu)
                    .await
                {
                    Ok(result)
                        if matches!(result.payload_status.status, PayloadStatusEnum::Valid) =>
                    {
                        info!(target: "n42::cl::consensus_loop", %block_hash, "bg import: block imported successfully");
                        (ImportOutcome::Valid, block_timestamp)
                    }
                    Ok(result)
                        if matches!(
                            result.payload_status.status,
                            PayloadStatusEnum::Syncing | PayloadStatusEnum::Accepted
                        ) =>
                    {
                        warn!(target: "n42::cl::consensus_loop", %block_hash, status = ?result.payload_status.status, "bg import: fcu did not execute the committed block; requesting catch-up");
                        (ImportOutcome::Syncing, 0)
                    }
                    Ok(result) => {
                        error!(target: "n42::cl::consensus_loop", %block_hash, status = ?result.payload_status.status, "bg import: fcu rejected the committed block");
                        (ImportOutcome::Invalid, 0)
                    }
                    Err(e) => {
                        // Transport/internal FCU errors do not prove payload
                        // invalidity. Preserve the staged sidecar diff and retry
                        // through the execution catch-up path.
                        error!(target: "n42::cl::consensus_loop", %block_hash, error = %e, "bg import: fcu failed; treating as retryable");
                        (ImportOutcome::Syncing, 0)
                    }
                }
            }
            Ok(status)
                if matches!(
                    status.status,
                    PayloadStatusEnum::Syncing | PayloadStatusEnum::Accepted
                ) =>
            {
                if compact_injected && let Some(ref cache) = exec_cache {
                    cache.evict(block_hash);
                }
                // `Accepted` = stored as a side chain, not executed (Engine
                // API). Bucket it with `Syncing` so handle_import_done drives a
                // catch-up rather than promoting an unexecuted block (F3).
                info!(target: "n42::cl::consensus_loop", %block_hash, status = ?status.status, "bg import: block not yet executable, retrying via catch-up");
                (ImportOutcome::Syncing, 0)
            }
            Ok(status) => {
                if compact_injected && let Some(ref cache) = exec_cache {
                    cache.evict(block_hash);
                }
                // The committed background path consumes the same unauthenticated
                // compact bytes as eager/sync imports. A rejection after injection
                // describes those bytes, not necessarily the committed block, so
                // evict above and keep the hash retryable (HIGH-1).
                if !compact_injected {
                    bad_blocks.insert_if_invalid(block_hash, &status.status, "committed_import");
                }
                warn!(target: "n42::cl::consensus_loop", %block_hash, status = ?status.status, "bg import: new_payload rejected");
                (ImportOutcome::Invalid, 0)
            }
            Err(e) => {
                if compact_injected && let Some(ref cache) = exec_cache {
                    cache.evict(block_hash);
                }
                // An Engine API transport/internal error is not an `Invalid`
                // verdict. Keep the committed diff staged and retry ancestors.
                error!(target: "n42::cl::consensus_loop", %block_hash, error = %e, "bg import: new_payload failed; treating as retryable");
                (ImportOutcome::Syncing, 0)
            }
        }
    }

    /// Records a deferred finalization when block data has not yet been imported.
    pub(super) fn defer_finalization(
        &mut self,
        view: u64,
        block_hash: B256,
        commit_qc: QuorumCertificate,
    ) {
        self.pending_finalization = Some(super::PendingFinalization {
            view,
            block_hash,
            commit_qc,
        });
        self.pending_executions.insert(block_hash);
    }

    pub(super) async fn handle_view_changed(&mut self, new_view: u64) {
        counter!("n42_view_changes_total").increment(1);
        self.record_timeout_diag_view_changed(new_view);
        self.view_started_at = Some(tokio::time::Instant::now());
        self.pending_vote_resend = None;
        // Note: building_on_parent is NOT cleared here. It's keyed on parent hash,
        // not view number. If a build is in-flight for the current parent, it should
        // still be guarded even after a view change. The guard naturally expires when
        // head_block_hash changes (a new block is imported/committed).

        // Reset eager import block guard on view change: the previous view's proposal
        // was not committed, so the new leader may propose a different block at the
        // same height. The guard must allow reimporting the same block number.
        self.eager_import_block_guard
            .store(0, std::sync::atomic::Ordering::SeqCst);

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
                let stale_hash = pf.block_hash;
                self.pending_finalization = None;
                self.pending_block_data.clear();
                self.pending_executions.clear();
                // A COMMITTED block whose broadcast we still hold must be
                // re-driven locally - dropping straight to sync gambled that a
                // peer retained the only copy of data this node already had
                // (the committed_blocks ring keeps the raw broadcast exactly
                // for this). Only a truly data-less stall goes to sync.
                let local = self
                    .committed_blocks
                    .iter()
                    .rev()
                    .find(|b| b.block_hash == stale_hash)
                    .and_then(|b| {
                        b.block_data.clone().or_else(|| {
                            (!b.payload.is_empty())
                                .then(|| super::BlockDataBroadcast {
                                    block_hash: b.block_hash,
                                    view: b.view,
                                    payload_json: b.payload.clone(),
                                    timestamp: 0,
                                    execution_output: None,
                                    leader_ready_unix_ms: 0,
                                })
                                .and_then(|broadcast| bincode::serialize(&broadcast).ok())
                                .map(Into::into)
                        })
                    });
                if let Some(raw) = local {
                    counter!("n42_pending_finalization_local_redrive_total").increment(1);
                    info!(
                        target: "n42::cl::consensus_loop",
                        stale_view,
                        new_view,
                        hash = %stale_hash,
                        "stale pending_finalization re-driven from the retained committed broadcast"
                    );
                    self.enqueue_bg_import(raw, stale_hash, stale_view);
                } else {
                    // Trigger sync to recover the missing block
                    self.initiate_sync(stale_view, new_view);
                }
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
                self.begin_leader_build_wait(LeaderBuildWaitMode::Scheduled, None);
                self.evaluate_leader_build_wait(None).await;
            }
        } else {
            // Not leader for this view — clear any stale speculative build.
            self.speculative_build_hash = None;
        }
    }

    fn record_eager_execution_validated(&mut self, hash: B256) {
        const MAX_EAGER_VALIDATED_BLOCKS: usize = 32;
        if self.eager_execution_validated.contains(&hash) {
            return;
        }
        if self.eager_execution_validated.len() >= MAX_EAGER_VALIDATED_BLOCKS {
            self.eager_execution_validated.pop_front();
        }
        self.eager_execution_validated.push_back(hash);
    }

    /// True when importing `(view, hash)` would move reth's canonical head
    /// backward or sideways relative to the execution-validated head — the same
    /// refusal `advance_execution_validated_head` applies, but checkable
    /// *before* the fork-choice-update side effect so the backward FCU never
    /// reaches reth in the first place (F1: the FCU used to fire ahead of the
    /// guard, letting a stale sync import regress reth's canonical head).
    pub(super) fn import_would_regress_head(&self, view: u64, hash: B256) -> bool {
        view < self.execution_validated_head_view
            || (view == self.execution_validated_head_view
                && self.execution_validated_head_view != 0
                && self.head_block_hash != hash)
    }

    /// Floor view for an execution-lineage catch-up request.
    ///
    /// Only the execution-validated view is a sound floor. In particular, a
    /// joining validator can learn and commit a descendant before it has the
    /// ancestor payloads; using `last_commit_qc.view` in that case skips the
    /// exact lineage it needs. Restart recovery seeds this field from a
    /// view/hash pair proven against reth's canonical head, so recovered nodes
    /// still start from their durable execution floor.
    pub(super) fn execution_catchup_floor(&self) -> u64 {
        self.execution_validated_head_view
    }

    /// Advances the local payload-building head only after reth has confirmed the
    /// block executable. Completions can arrive out of order when async FCU is
    /// enabled, so an older confirmation must never regress the head.
    pub(super) fn advance_execution_validated_head(
        &mut self,
        view: u64,
        hash: B256,
        source: &'static str,
    ) {
        if self.jmt.is_some() || self.twig.is_some() {
            if let Some(existing) = self.execution_validated_sidecar_hashes.get(&view)
                && *existing != hash
            {
                error!(
                    target: "n42::twig",
                    view,
                    %hash,
                    %existing,
                    source,
                    "conflicting execution-valid hashes for one sidecar view; refusing binding"
                );
                counter!("n42_sidecar_canonical_hash_conflicts_total").increment(1);
                return;
            }
            self.execution_validated_sidecar_hashes.insert(view, hash);
        }
        if view < self.execution_validated_head_view {
            self.enqueue_confirmed_sidecar_state_diffs(self.execution_validated_head_view);
            debug!(
                target: "n42::cl::consensus_loop",
                view,
                %hash,
                source,
                current_validated_view = self.execution_validated_head_view,
                current_head = %self.head_block_hash,
                "ignoring stale execution-validity completion"
            );
            return;
        }
        if view == self.execution_validated_head_view
            && self.execution_validated_head_view != 0
            && self.head_block_hash != hash
        {
            error!(
                target: "n42::cl::consensus_loop",
                view,
                %hash,
                source,
                current_head = %self.head_block_hash,
                "conflicting execution-validity result at the same committed view; refusing head change"
            );
            return;
        }

        self.head_block_hash = hash;
        self.execution_validated_head_view = view;
        if let Some(block_number) = self.h2_v4_block_numbers.remove(&hash) {
            self.head_block_number = self.head_block_number.max(block_number);
            self.prune_executed_h2_v4_catchup_history();
        }
        if let Some(path) = self.state_file.as_deref()
            && let Err(error) = crate::persistence::append_execution_lineage_proof(path, view, hash)
        {
            error!(
                target: "n42::cl::sync",
                view,
                %hash,
                %error,
                "failed to persist execution-valid lineage proof"
            );
        }
        // A canonical FCU can complete after the BlockCommitted snapshot was
        // written. Persist the exact (view, hash) proof at the point execution
        // validity advances so a crash/restart never observes a newer Reth head
        // paired with a stale execution guard.
        self.save_consensus_state();
        // Execution is now confirmed for this block: flush its staged sidecar
        // state-tree diff (no-op when nothing is staged - e.g. catch-up blocks
        // whose broadcast never reached us; the sidecar catch-up path owns those).
        self.enqueue_confirmed_sidecar_state_diffs(view);
        debug!(
            target: "n42::cl::consensus_loop",
            view,
            %hash,
            source,
            "advanced execution-validated head"
        );
    }

    /// Called when a background import task completes.
    pub(super) async fn handle_import_done(
        &mut self,
        hash: B256,
        view: u64,
        outcome: ImportOutcome,
        block_timestamp: u64,
    ) {
        self.bg_import_in_flight = false;
        self.bg_import_hashes.remove(&hash);
        if outcome == ImportOutcome::Valid {
            // Pipeline: background import complete — record timing.
            if let Some(timing) = self.pipeline_timings.get_mut(&hash) {
                timing.import_complete = Some(Instant::now());
            }
            info!(target: "n42::cl::consensus_loop", %hash, view, "background import completed, updating head");
            self.advance_execution_validated_head(view, hash, "background import");
            self.mark_async_commit_finalized(view, hash);
            if block_timestamp > 0 {
                self.last_committed_timestamp = self.last_committed_timestamp.max(block_timestamp);
            }
            self.enqueue_mobile_packet(hash, view, "background import completed")
                .await;
        } else {
            let committed = self
                .committed_blocks
                .iter()
                .any(|block| block.view == view && block.block_hash == hash);
            if committed && outcome == ImportOutcome::Invalid {
                error!(
                    target: "n42::cl::consensus_loop",
                    %hash,
                    view,
                    execution_validated_head_view = self.execution_validated_head_view,
                    execution_validated_head = %self.head_block_hash,
                    "COMMITTED block is unexecutable; preserving last execution-validated head and requesting sync"
                );
                counter!("n42_committed_block_unexecutable_total").increment(1);
                self.discard_unvalidated_sidecar_diff(view, hash);
            } else if committed {
                warn!(
                    target: "n42::cl::consensus_loop",
                    %hash,
                    view,
                    execution_validated_head_view = self.execution_validated_head_view,
                    "committed block is waiting for an execution ancestor; requesting catch-up sync"
                );
            } else {
                warn!(target: "n42::cl::consensus_loop", %hash, view, "speculative background import failed, requesting sync");
            }
            counter!("n42_bg_import_failures_total").increment(1);
            // Clear the queue — parent failed so children would fail too.
            for (_, queued_hash, _) in self.bg_import_queue.drain(..) {
                self.bg_import_hashes.remove(&queued_hash);
            }
            let sync_from = if committed {
                self.execution_catchup_floor()
            } else {
                view
            };
            if committed && outcome == ImportOutcome::Syncing {
                self.initiate_execution_catchup_sync(sync_from, self.engine.current_view());
            } else {
                self.initiate_sync(sync_from, self.engine.current_view());
            }
            return;
        }

        // Process next queued import, or trigger build if queue is empty.
        if let Some((data, next_hash, next_view)) = self.bg_import_queue.pop_front() {
            debug!(target: "n42::cl::consensus_loop", view = next_view, %next_hash, remaining = self.bg_import_queue.len(), "processing next queued bg import");
            self.spawn_bg_import(data, next_hash, next_view);
        } else if self.engine.is_current_leader() {
            debug!(target: "n42::cl::consensus_loop", next_view = self.engine.current_view(), "leader: scheduling build after all bg imports done");
            self.begin_leader_build_wait(LeaderBuildWaitMode::Scheduled, None);
            self.evaluate_leader_build_wait(None).await;
        }
    }

    fn record_eager_import_completion(
        &mut self,
        hash: B256,
        block_ts: u64,
        parent_hash: B256,
        block_number: u64,
        has_staking_target: bool,
        state_diff: Option<n42_execution::state_diff::StateDiff>,
    ) {
        if block_ts > 0 {
            self.last_committed_timestamp = self.last_committed_timestamp.max(block_ts);
        }
        if let Some(timing) = self.pipeline_timings.get_mut(&hash) {
            timing.import_complete = Some(Instant::now());
        }
        self.record_validated_execution_metadata(
            hash,
            parent_hash,
            block_number,
            has_staking_target,
            state_diff,
        );
        self.record_eager_execution_validated(hash);
    }

    async fn finish_eager_import_completion(
        &mut self,
        hash: B256,
        block_number: u64,
        has_staking_target: bool,
    ) {
        debug!(target: "n42::cl::consensus_loop", %hash, "eager import done: block in engine tree (awaiting consensus commit)");

        // Gov5-compatible votes are execution attestations. Only a successful
        // reth new_payload result reaches this callback, so this is the first
        // safe point to release an H2 participant's deferred vote.
        if self.h2_v4_identity.is_some() {
            // Same reasoning retires the Gov5 fetch here rather than on arrival
            // of the block-data envelope: reth accepting this payload is what
            // proves the hash, so a forged envelope can no longer cancel an
            // in-flight ancestry fetch. Native mode never fetches from Gov5, so
            // it stays out of this path entirely.
            self.retire_h2_v4_fetch_satisfied_elsewhere(hash).await;

            if let Err(error) = self
                .engine
                .process_event(n42_consensus::ConsensusEvent::BlockImported(hash))
            {
                error!(target: "n42::interop::h2v4", %hash, %error, "failed to release execution-gated H2 vote");
            }
        }

        // If commit/finalize raced ahead of eager new_payload, re-drive the
        // committed FCU now that reth has explicitly accepted the payload. This
        // closes the late-eager Case C gap without making speculative imports
        // canonical.
        let committed = self
            .committed_blocks
            .iter()
            .rev()
            .find(|block| block.block_hash == hash)
            .map(|block| (block.view, block.commit_qc.clone()));
        if let Some((view, commit_qc)) = committed {
            self.queue_validated_staking_scan(hash, block_number, has_staking_target);
            // Tests and restart recovery can materialize the committed ring
            // before the in-memory lifecycle map. Reconstruct the entry, then
            // promote it with the exact hash accepted by new_payload.
            self.enqueue_async_commit(view, hash, commit_qc, true);
            self.mark_async_commit_execution_ready(hash);
        }
    }

    /// Called when an eager import task (leader or follower) completes successfully.
    /// The block is now in reth's engine tree (via new_payload only, no FCU).
    /// We record this so finalize_committed_block knows the block is pre-validated
    /// and its FCU will succeed immediately (Case A).
    pub(super) async fn handle_eager_import_done(
        &mut self,
        hash: B256,
        block_ts: u64,
        parent_hash: B256,
        block_number: u64,
        has_staking_target: bool,
        state_diff: Option<n42_execution::state_diff::StateDiff>,
    ) {
        // The engine's extends rule reads the parent of an imported block;
        // eager import knows it, and the vote this import releases is checked
        // against it.
        self.engine.remember_parent(hash, parent_hash);

        // Record the pre-imported block hash. When finalize_committed_block runs,
        // it will find this block already in reth's engine tree, making FCU instant.
        // We do NOT update head_block_hash here — that should only change via FCU
        // in finalize_committed_block, to avoid canonical chain reorgs from
        // speculative blocks that consensus may not ultimately commit.
        self.record_eager_import_completion(
            hash,
            block_ts,
            parent_hash,
            block_number,
            has_staking_target,
            state_diff,
        );
        self.finish_eager_import_completion(hash, block_number, has_staking_target)
            .await;
        self.drive_async_commits().await;
    }

    /// Drains validated staking classifications strictly in execution-block
    /// order. The common negative case advances with `{}` and never decodes the
    /// multi-megabyte payload on the commit event loop.
    fn queue_validated_staking_scan(
        &mut self,
        hash: B256,
        block_number: u64,
        has_staking_target: bool,
    ) {
        let Some(staking_sink) = self.staking_sink.as_ref() else {
            return;
        };
        if block_number <= staking_sink.last_scanned_block() {
            return;
        }
        self.pending_staking_scans
            .insert(block_number, (hash, has_staking_target));

        loop {
            let last_scanned = self
                .staking_sink
                .as_ref()
                .map(|sink| sink.last_scanned_block())
                .unwrap_or_default();
            let next_block = last_scanned.saturating_add(1);
            let Some((next_hash, next_has_target)) =
                self.pending_staking_scans.get(&next_block).copied()
            else {
                break;
            };

            let payload = if next_has_target {
                self.recent_block_data
                    .iter()
                    .find(|entry| entry.block_hash == next_hash)
                    .and_then(|entry| {
                        bincode::deserialize::<super::BlockDataBroadcast>(
                            entry.block_data.as_slice(),
                        )
                        .ok()
                    })
                    .and_then(|broadcast| super::decompress_payload(&broadcast.payload_json).ok())
                    .and_then(|payload| super::execution_payload_json_for_staking(&payload).ok())
            } else {
                Some(Vec::new())
            };
            let Some(payload) = payload else {
                warn!(target: "n42::cl::consensus_loop", block_number = next_block, %next_hash, "validated staking payload unavailable; preserving ordered scan barrier");
                break;
            };

            if let Some(staking_sink) = self.staking_sink.as_ref() {
                staking_sink.scan_committed_block(
                    next_block,
                    if next_has_target { &payload } else { b"{}" },
                );
            }
            self.pending_staking_scans.remove(&next_block);
            counter!("n42_staking_scan_deferred_drained_total").increment(1);
        }
    }

    /// Completes auxiliary QMDB/Twig staging without delaying the execution-
    /// validity notification that drives finalize FCU and the next leader.
    pub(super) fn handle_state_diff_ready(
        &mut self,
        hash: B256,
        state_diff: n42_execution::state_diff::StateDiff,
    ) {
        let Some(view) = self
            .recent_block_data
            .iter()
            .find(|entry| entry.block_hash == hash)
            .map(|entry| entry.view)
        else {
            counter!("n42_state_diff_ready_orphan_total").increment(1);
            debug!(target: "n42::twig", %hash, "state-diff completion arrived after raw metadata eviction");
            return;
        };

        let committed_gap = self
            .missing_sidecar_diffs
            .get(&view)
            .is_some_and(|missing_hash| *missing_hash == hash);
        if !committed_gap {
            if let Some(entry) = self
                .recent_block_data
                .iter_mut()
                .find(|entry| entry.block_hash == hash)
            {
                entry.execution_state_diff = Some(state_diff);
            }
            return;
        }

        self.missing_sidecar_diffs.remove(&view);
        self.pending_sidecar_diffs.insert(view, (hash, state_diff));
        counter!("n42_sidecar_diff_deferred_ready_total").increment(1);
        debug!(target: "n42::twig", view, %hash, "asynchronous committed sidecar diff ready");
        if view <= self.execution_validated_head_view {
            self.enqueue_confirmed_sidecar_state_diffs(self.execution_validated_head_view);
        }
    }

    pub(super) async fn handle_leader_payload_feedback(
        &mut self,
        hash: B256,
        data: super::SharedBlockData,
    ) {
        let decoded_broadcast =
            match bincode::deserialize::<super::BlockDataBroadcast>(data.as_slice()) {
                Ok(broadcast) => Some(broadcast),
                Err(error) => {
                    warn!(
                        target: "n42::cl::consensus_loop",
                        %hash,
                        error = %error,
                        "failed to decode leader block data broadcast"
                    );
                    None
                }
            };
        // Cache the block data for deferred import during finalization.
        // The leader skips new_payload during building (deferred-import optimization),
        // so the block data must be available in pending_block_data for Case B in
        // finalize_committed_block().  GossipSub does not echo to self, so without
        // this cache the leader's own block would fall to Case C (missing data).
        let pending_finalization_hash = self
            .pending_finalization
            .as_ref()
            .map(|pending| pending.block_hash)
            .unwrap_or(hash);
        let cached = if let Some(broadcast) = decoded_broadcast.as_ref() {
            self.cache_pending_block_data_with_metadata(
                hash,
                Arc::clone(&data),
                &[hash, pending_finalization_hash],
                broadcast.view,
                broadcast.payload_json.len(),
            )
        } else {
            self.cache_pending_block_data(
                hash,
                Arc::clone(&data),
                &[hash, pending_finalization_hash],
            )
        };
        if cached {
            counter!(
                "n42_block_data_copy_avoided_bytes_total",
                "site" => "leader_feedback"
            )
            .increment(data.len() as u64);
            debug!(target: "n42::cl::consensus_loop", %hash, "cached leader block data for deferred import");
        }

        if let Some(ref broadcast) = decoded_broadcast
            && broadcast.timestamp > 0
        {
            self.last_committed_timestamp = self.last_committed_timestamp.max(broadcast.timestamp);
        }

        if let Some(block) = self
            .committed_blocks
            .iter_mut()
            .rev()
            .find(|b| b.block_hash == hash)
            && block.block_data.is_none()
        {
            block.block_data = Some(Arc::clone(&data));
            if let Some(ref broadcast) = decoded_broadcast {
                block.payload_len = broadcast.payload_json.len();
            }
            debug!(target: "n42::cl::consensus_loop", %hash, "linked committed block to shared leader envelope");
        }

        // If a deferred finalization is pending for this hash, complete it now.
        // This handles the race where commit arrives before leader_payload_rx.
        let should_finalize = self
            .pending_finalization
            .as_ref()
            .is_some_and(|pf| pf.block_hash == hash);
        if should_finalize {
            info!(target: "n42::cl::consensus_loop", %hash, "leader block data arrived, completing deferred finalization");
            if let Some(pf) = self.pending_finalization.take() {
                self.finalize_committed_block(pf.view, pf.block_hash, pf.commit_qc)
                    .await;
            }
        }
    }

    fn decode_block_data_broadcast(
        block_hash: B256,
        data: &[u8],
        context: &'static str,
    ) -> Option<super::BlockDataBroadcast> {
        match bincode::deserialize(data) {
            Ok(broadcast) => Some(broadcast),
            Err(error) => {
                warn!(
                    target: "n42::cl::consensus_loop",
                    %block_hash,
                    context,
                    error = %error,
                    "failed to decode block data broadcast"
                );
                None
            }
        }
    }

    /// Moves every staged diff through `confirmed_view` into a single FIFO
    /// background worker. Reth can confirm later blocks before earlier async
    /// completions are observed; draining by committed view preserves the
    /// state-tree lineage and prevents separately spawned tasks from racing.
    pub(super) fn enqueue_confirmed_sidecar_state_diffs(&mut self, confirmed_view: u64) {
        let last_applied_view = self
            .last_sidecar_applied_view
            .load(std::sync::atomic::Ordering::Acquire);
        let stale_bindings: Vec<_> = self
            .execution_validated_sidecar_hashes
            .keys()
            .filter(|view| {
                **view <= last_applied_view
                    && !self.pending_sidecar_diffs.contains_key(view)
                    && !self.missing_sidecar_diffs.contains_key(view)
            })
            .copied()
            .collect();
        for view in stale_bindings {
            self.execution_validated_sidecar_hashes.remove(&view);
        }

        // Never apply across a missing committed diff. Views can skip on
        // HotStuff timeouts, so the barrier is the first explicitly recorded
        // missing view rather than `last + 1` arithmetic.
        let blocked_view = self
            .missing_sidecar_diffs
            .range(..=confirmed_view)
            .next()
            .map(|(view, _)| *view);
        let mut confirmed_views = Vec::new();
        let mut canonical_mismatch = None;
        for (view, (staged_hash, _)) in self.pending_sidecar_diffs.range(..=confirmed_view) {
            if blocked_view.is_some_and(|blocked| *view >= blocked) {
                break;
            }
            let Some(canonical_hash) = self.execution_validated_sidecar_hashes.get(view) else {
                // A later execution completion does not by itself authorize an
                // arbitrary staged diff at an earlier view. Wait for the exact
                // per-view `Valid` result.
                break;
            };
            if canonical_hash != staged_hash {
                canonical_mismatch = Some((*view, *staged_hash, *canonical_hash));
                break;
            }
            confirmed_views.push(*view);
        }

        if let Some((view, staged_hash, canonical_hash)) = canonical_mismatch {
            self.pending_sidecar_diffs.remove(&view);
            self.missing_sidecar_diffs.insert(view, canonical_hash);
            counter!("n42_sidecar_canonical_hash_mismatch_total").increment(1);
            error!(
                target: "n42::twig",
                view,
                %staged_hash,
                %canonical_hash,
                "discarded sidecar diff that is not bound to the execution-valid canonical hash"
            );
        }
        if confirmed_views.is_empty() {
            return;
        }

        {
            let mut queue = self
                .sidecar_apply_queue
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            for view in confirmed_views {
                if let Some((hash, diff)) = self.pending_sidecar_diffs.remove(&view) {
                    queue.push_back((view, hash, diff));
                }
                self.execution_validated_sidecar_hashes.remove(&view);
            }
        }

        if self
            .sidecar_apply_worker_running
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return;
        }

        let queue = Arc::clone(&self.sidecar_apply_queue);
        let running = Arc::clone(&self.sidecar_apply_worker_running);
        let last_applied = Arc::clone(&self.last_sidecar_applied_view);
        let jmt = self.jmt.clone();
        let twig = self.twig.clone();
        let state = self.consensus_state.clone();
        tokio::task::spawn_blocking(move || {
            // F7: the append-ordered twig root demands diffs apply in committed
            // order. The queue is view-ordered by construction; this tracks the
            // last applied view so a reordering bug is caught, not silently
            // baked into a diverged root.
            loop {
                let work = queue
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .pop_front();
                let Some((view, block_hash, diff)) = work else {
                    running.store(false, std::sync::atomic::Ordering::Release);
                    let has_more = !queue
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .is_empty();
                    if !has_more
                        || running
                            .compare_exchange(
                                false,
                                true,
                                std::sync::atomic::Ordering::AcqRel,
                                std::sync::atomic::Ordering::Acquire,
                            )
                            .is_err()
                    {
                        break;
                    }
                    continue;
                };

                // F7 defense-in-depth: refuse an out-of-order apply. Applying a
                // lower view after a higher one would diverge the append-ordered
                // twig root permanently, so skip and surface it loudly.
                let last_applied_view = last_applied.load(std::sync::atomic::Ordering::Acquire);
                if view <= last_applied_view {
                    error!(
                        target: "n42::twig",
                        view,
                        last_applied_view,
                        %block_hash,
                        "sidecar apply queue duplicate/out of order; skipping to preserve root determinism"
                    );
                    counter!("n42_sidecar_apply_out_of_order").increment(1);
                    continue;
                }
                last_applied.store(view, std::sync::atomic::Ordering::Release);

                if let Some(ref jmt) = jmt {
                    let start = std::time::Instant::now();
                    // StateSink::apply_diff locks the SBMT internally, applies
                    // in-memory, appends the WAL (durable per block) and checkpoints
                    // a snapshot every interval. It returns a Result because the
                    // WAL/snapshot IO can fail.
                    match jmt.apply_diff(block_hash, &diff) {
                        Ok((version, root)) => {
                            let elapsed_ms = start.elapsed().as_millis();
                            gauge!("n42_jmt_latest_root").set(version as f64);
                            histogram!("n42_state_root_apply_diff_ms").record(elapsed_ms as f64);
                            info!(
                                target: "n42::jmt",
                                view,
                                %block_hash,
                                version,
                                %root,
                                accounts = diff.len(),
                                storage_changes = diff.total_storage_changes(),
                                elapsed_ms,
                                "SBMT updated"
                            );
                            if let Some(ref state) = state {
                                state.update_jmt_root(version, root);
                            }
                        }
                        Err(e) => {
                            // A failed apply poisons the sink (F1b): it now
                            // refuses every later diff, so update_jmt_root is
                            // never called again and the node stops publishing a
                            // root that has silently diverged. Surface it loudly.
                            error!(target: "n42::jmt", view, %block_hash, error = %e, "SBMT apply_diff/persist failed; sink poisoned, no further roots published");
                            counter!("n42_jmt_sink_poisoned").increment(1);
                        }
                    }
                }
                if let Some(ref twig) = twig {
                    let start = std::time::Instant::now();
                    // StateSink::apply_diff locks the Twig tree internally.
                    match twig.apply_diff(block_hash, &diff) {
                        Ok((version, root)) => {
                            let elapsed_ms = start.elapsed().as_millis();
                            gauge!("n42_twig_latest_root").set(version as f64);
                            histogram!("n42_twig_apply_diff_ms").record(elapsed_ms as f64);
                            // Pair the duration with the forest size. Part of the
                            // per-block cost scales with twig count and not with
                            // the block, so the two series together separate
                            // "this block was big" from "the tree has grown" —
                            // which is not decidable from the duration alone.
                            let twig_count = twig.node_count();
                            if let Some(count) = twig_count {
                                gauge!("n42_twig_count").set(count as f64);
                            }
                            info!(
                                target: "n42::twig",
                                view,
                                %block_hash,
                                version,
                                %root,
                                accounts = diff.len(),
                                storage_changes = diff.total_storage_changes(),
                                twig_count,
                                elapsed_ms,
                                "Twig updated"
                            );
                            if let Some(ref state) = state {
                                state.update_twig_root(version, root);
                            }
                        }
                        Err(e) => {
                            // A failed apply poisons the sink (F1b): it now
                            // refuses every later diff, so update_twig_root is
                            // never called again and the node stops publishing a
                            // root that has silently diverged. Surface it loudly.
                            error!(target: "n42::twig", view, %block_hash, error = %e, "Twig apply_diff/persist failed; sink poisoned, no further roots published");
                            counter!("n42_twig_sink_poisoned").increment(1);
                        }
                    }
                }
            }
        });
    }

    /// Try to extract a `StateDiff` from serialized `BlockDataBroadcast` for a state-tree update.
    /// Returns `None` if the data is missing, malformed, or has no compact block execution.
    pub(super) fn extract_state_diff_for_state_tree(
        block_hash: B256,
        data: &[u8],
    ) -> Option<n42_execution::state_diff::StateDiff> {
        let broadcast = Self::decode_block_data_broadcast(block_hash, data, "state_tree_diff")?;
        let exec_bytes = match broadcast.execution_output.as_ref() {
            Some(exec_bytes) => exec_bytes,
            None => {
                debug!(
                    target: "n42::cl::consensus_loop",
                    %block_hash,
                    "block data broadcast missing execution output for state-tree update"
                );
                return None;
            }
        };
        Self::extract_state_diff_from_execution_output(block_hash, exec_bytes)
    }

    /// Derive the sidecar delta directly from the compressed compact execution
    /// output. Eager import tasks use this entry point so parsing overlaps reth
    /// validation and consensus instead of delaying the next leader at commit.
    pub(super) fn extract_state_diff_from_execution_output(
        block_hash: B256,
        exec_bytes: &[u8],
    ) -> Option<n42_execution::state_diff::StateDiff> {
        let decompressed = match zstd::bulk::decompress(
            exec_bytes,
            super::MAX_DECOMPRESSED_BLOCK_COMPONENT_SIZE,
        ) {
            Ok(decompressed) => decompressed,
            Err(error) => {
                warn!(
                    target: "n42::cl::consensus_loop",
                    %block_hash,
                    error = %error,
                    "failed to decompress execution output for state-tree update"
                );
                return None;
            }
        };
        let compact: super::CompactBlockExecution = match serde_json::from_slice(&decompressed) {
            Ok(compact) => compact,
            Err(error) => {
                warn!(
                    target: "n42::cl::consensus_loop",
                    %block_hash,
                    error = %error,
                    "failed to decode compact execution JSON for state-tree update"
                );
                return None;
            }
        };
        // Diagnostic for the devlog-71 `storage_changes=0` follow-up: count the
        // raw storage slots present in the broadcast bundle BEFORE StateDiff
        // filtering. If this is 0 for a contract-heavy block, the storage is
        // missing from the leader's captured bundle (reth payload-build capture),
        // not dropped by StateDiff extraction. See docs/devlog-71.
        let raw_bundle_storage: usize = compact
            .bundle_state
            .state
            .values()
            .map(|acc| acc.storage.len())
            .sum();
        let diff = n42_execution::state_diff::StateDiff::from_bundle_state(&compact.bundle_state);
        debug!(
            target: "n42::cl::consensus_loop",
            %block_hash,
            bundle_accounts = compact.bundle_state.state.len(),
            raw_bundle_storage_slots = raw_bundle_storage,
            diff_accounts = diff.len(),
            diff_storage_changes = diff.total_storage_changes(),
            "STATE_DIFF_DIAG: raw broadcast bundle vs extracted diff"
        );
        Some(diff)
    }

    /// Extract decompressed CompactBlockExecution JSON bytes from raw BlockDataBroadcast.
    /// Returns `None` if the data is missing, malformed, or has no execution output.
    fn extract_bundle_state_json(block_hash: B256, data: &[u8]) -> Option<Vec<u8>> {
        let broadcast = Self::decode_block_data_broadcast(block_hash, data, "bundle_state_json")?;
        let exec_bytes = match broadcast.execution_output.as_ref() {
            Some(exec_bytes) => exec_bytes,
            None => {
                debug!(
                    target: "n42::cl::consensus_loop",
                    %block_hash,
                    "block data broadcast missing execution output for bundle JSON extraction"
                );
                return None;
            }
        };
        match zstd::bulk::decompress(exec_bytes, super::MAX_DECOMPRESSED_BLOCK_COMPONENT_SIZE) {
            Ok(bundle_json) => Some(bundle_json),
            Err(error) => {
                warn!(
                    target: "n42::cl::consensus_loop",
                    %block_hash,
                    error = %error,
                    "failed to decompress execution output for bundle JSON extraction"
                );
                None
            }
        }
    }
}
