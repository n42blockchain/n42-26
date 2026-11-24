use super::{BlobSidecarBroadcast, BlockDataBroadcast, ConsensusService, EagerImportDone};
use crate::blob_port::BlobStorePort;
use crate::el::{BuiltBlock, ExecutionLayer, ExecutionPath, ResolveKind};
use crate::exec_cache::ExecutionOutputCache;
use crate::ingest::note_virtual_block_credit;
use crate::net_port::ConsensusNetwork;
use crate::now_unix_ms;
use alloy_primitives::B256;
use alloy_rpc_types_engine::{ForkchoiceState, PayloadAttributes, PayloadId, PayloadStatusEnum};
use n42_consensus::ConsensusEvent;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::{Instrument, debug, error, info, warn};

/// Whether Compact Block (follower EVM skip) is enabled.
/// Controlled by `N42_COMPACT_BLOCK` env var: "0" to disable, anything else or absent = enabled.
pub fn compact_block_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("N42_COMPACT_BLOCK")
            .map(|v| v != "0")
            .unwrap_or(true)
    })
}

/// Whether validators on one machine share one execution of each block.
/// Controlled by `N42_SHARED_EXECUTION=1`; off by default.
///
/// With it on, an H2-v4 leader sends its execution output next to the block,
/// and a follower loads that output instead of executing the block again. The
/// follower still derives the QMDB state root and the receipts root from the
/// output and compares both with the header before it votes. It does not check
/// that the output is what the transactions produce: that is the trust placed
/// in the co-located leader, so the setting is for validators of one operator
/// on one machine only.
pub fn shared_execution_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("N42_SHARED_EXECUTION").is_ok_and(|v| v == "1"))
}

/// Whether a leader that also leads the next view starts building that
/// view's block as soon as its own execution layer has accepted the current
/// one, instead of at the commit (`N42_BUILD_ON_SEAL=1`). Useful with
/// `N42_LEADER_TENURE` above 1. The proposal rules are unchanged: the block
/// built ahead is proposed in its own view under the QC of its parent, and is
/// dropped if that QC does not form. In H2-v4 participant mode only.
///
/// The execution layer is told to treat the leader's own uncommitted block as
/// its head while the child is built. With the frame queue, transactions of a
/// block that is then not committed are not offered again.
pub fn build_on_seal_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("N42_BUILD_ON_SEAL").is_ok_and(|v| v == "1"))
}

/// `N42_BUILD_ON_OUTPUT=1`, on top of [`build_on_seal_enabled`]: the child of
/// a leader's own block is built from the block's execution output as soon as
/// the block is sealed, beside the block's own insert, instead of once the
/// engine has the block. The block's validation then only moves the
/// execution layer's head to it; a child build that produces nothing is
/// started again the usual way.
pub fn build_on_output_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        build_on_seal_enabled() && std::env::var("N42_BUILD_ON_OUTPUT").is_ok_and(|v| v == "1")
    })
}

/// `N42_BUILD_AHEAD_DEPTH=2`, on top of [`build_on_output_enabled`]: a leader
/// that leads the coming views builds up to two blocks past the current
/// view's, the second on the first one's sealed output, instead of one. Each
/// block is still proposed only in its own view under the QC of its parent,
/// and is dropped, with every block built on it, if that QC does not form.
/// Any other value, or the switch without build on output, is one block.
pub fn build_ahead_depth() -> u64 {
    static DEPTH: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *DEPTH.get_or_init(|| {
        let two = build_on_output_enabled()
            && std::env::var("N42_BUILD_AHEAD_DEPTH").is_ok_and(|v| v.trim() == "2");
        if two { 2 } else { 1 }
    })
}

/// `N42_FINALIZE_ON_TIP_CHAIN=1`: with one block built ahead, a finalize
/// names as head the newest own block on the committed block's chain, as
/// two blocks ahead does, instead of only the newest own block's parent's
/// child. The engine does not move finalized when the head it is given is an
/// ancestor of its head, so the single tip, once replaced, left finalized a
/// block behind on every other commit.
pub fn finalize_on_tip_chain() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("N42_FINALIZE_ON_TIP_CHAIN").is_ok_and(|v| v == "1"))
}

/// Where a leader's build comes from.
pub(super) enum BuildSource {
    /// A payload job started by a forkchoice update.
    Job(PayloadId),
    /// A build on an own block the engine may not have yet.
    SealedParent {
        parent: B256,
        attributes: PayloadAttributes,
    },
}

fn should_broadcast_execution_output(h2_v4_participant: bool) -> bool {
    should_broadcast_execution_output_with(h2_v4_participant, shared_execution_enabled())
}

fn should_broadcast_execution_output_with(h2_v4_participant: bool, shared_execution: bool) -> bool {
    compact_block_enabled() && (!h2_v4_participant || shared_execution)
}

fn elapsed_since_unix_ms(start_ms: u64) -> Option<u64> {
    (start_ms > 0).then(|| now_unix_ms().saturating_sub(start_ms))
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

fn eager_import_already_validated(
    guard: &std::sync::atomic::AtomicU64,
    block_number: u64,
) -> Option<u64> {
    let validated = guard.load(std::sync::atomic::Ordering::Acquire);
    (validated >= block_number).then_some(validated)
}

fn mark_eager_import_valid(guard: &std::sync::atomic::AtomicU64, block_number: u64) -> u64 {
    guard.fetch_max(block_number, std::sync::atomic::Ordering::AcqRel)
}

/// Only Engine API `VALID` means the payload was fully executed and may advance
/// the eager-import watermark. `ACCEPTED` and `SYNCING` are not validation.
fn payload_was_fully_validated(status: &PayloadStatusEnum) -> bool {
    matches!(status, PayloadStatusEnum::Valid)
}

#[derive(Debug, PartialEq, Eq)]
enum BuiltPayloadLinkError {
    Parent { actual: B256, required: B256 },
    Hash { declared: B256, payload: B256 },
}

/// Check the two identity links that must hold before a built block is
/// broadcast: the requested parent and the payload's own declared hash.
fn validate_built_payload_linkage(
    declared_hash: B256,
    payload_hash: B256,
    actual_parent: B256,
    required_parent: B256,
) -> Result<(), BuiltPayloadLinkError> {
    if actual_parent != required_parent {
        return Err(BuiltPayloadLinkError::Parent {
            actual: actual_parent,
            required: required_parent,
        });
    }
    if payload_hash != declared_hash {
        return Err(BuiltPayloadLinkError::Hash {
            declared: declared_hash,
            payload: payload_hash,
        });
    }
    Ok(())
}

/// Whether `N42_GOV5_COPY_LATE=1` is set (see the leader's emit path).
fn gov5_copy_late() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("N42_GOV5_COPY_LATE").is_ok_and(|value| value == "1"))
}

/// Whether `N42_COLOCATED_VOTE=1` is set (see `handle_block_data`).
fn colocated_vote_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("N42_COLOCATED_VOTE").is_ok_and(|value| value == "1"))
}

impl ConsensusService {
    /// The `parentBeaconRoot` a payload built on `parent` must carry. Without
    /// a committee pool it is the zero placeholder every node agrees on; with
    /// one it is gov5's `Blake3(parent committee evidence)`, rebuilt from the
    /// parent's native header (number, gov5 hash, native receipts root) as
    /// remembered from the wire. A parent whose native header is unknown
    /// cannot be stamped and the build must not proceed.
    pub(super) fn committee_parent_beacon_root(&self, parent: B256) -> Result<B256, String> {
        let Some(pool) = &self.committee_pool else {
            return Ok(B256::ZERO);
        };
        // A Profile-1 parent is remembered in its 24-element form; its alloy
        // view carries the same number and receipts root followers link to.
        let header = n42_consensus::remembered_gov5_native_header(&parent)
            .map(|native| native.header)
            .or_else(|| n42_consensus::remembered_profile1_header(&parent).map(|p1| p1.header))
            .ok_or_else(|| format!("native header of parent {parent} is not remembered"))?;
        pool.child_beacon_root(header.number, &parent, &header.receipts_root)
            .map_err(|error| format!("committee evidence for parent {parent}: {error}"))
    }

    /// Builds `PayloadAttributes` with timestamp correction and reward withdrawal injection.
    pub(super) fn build_payload_attributes(
        &mut self,
        slot_timestamp: Option<u64>,
        parent_beacon_block_root: B256,
    ) -> PayloadAttributes {
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

        // Mobile rewards (epoch boundary) + matured stake returns, with reward
        // addresses resolved to staker EVM addresses, computed behind the
        // WithdrawalSource port. Empty when neither manager is wired.
        let withdrawals = self
            .withdrawal_source
            .as_ref()
            .map(|src| src.withdrawals_for_block(self.committed_block_count + 1))
            .unwrap_or_default();

        PayloadAttributes {
            timestamp,
            // Profile 1 (B3): prevRandao (mixHash) is zero in every header.
            prev_randao: if self.profile1.is_some() {
                B256::ZERO
            } else {
                self.prev_randao_cache
            },
            suggested_fee_recipient: self.fee_recipient,
            withdrawals: Some(withdrawals),
            // N42 has no beacon chain. Without a committee pool B256::ZERO is a
            // deterministic placeholder that all nodes agree on; with one it is
            // gov5's committee-evidence root (`committee_parent_beacon_root`).
            // The EIP-4788 system contract executes with this value, producing
            // identical state on leader and followers. None is invalid for
            // Cancun — reth rejects attributes without it.
            parent_beacon_block_root: Some(parent_beacon_block_root),
            slot_number: None,
            // alloy 2.1: optional EL gas-limit hint. None = use the node's
            // configured gas limit (prior behavior before the field existed).
            target_gas_limit: None,
        }
    }

    /// Triggers payload building via fork_choice_updated, then spawns a task to resolve it.
    pub(super) async fn do_trigger_payload_build(&mut self, slot_timestamp: Option<u64>) {
        self.do_trigger_payload_build_on(None, slot_timestamp).await;
    }

    /// [`Self::do_trigger_payload_build`], or with `ahead` the build of a
    /// later view's block on an own block that is not committed yet.
    pub(super) async fn do_trigger_payload_build_on(
        &mut self,
        ahead: Option<super::PayloadBuildContext>,
        slot_timestamp: Option<u64>,
    ) {
        if self.leader_disabled {
            info!(target: "n42::cl::exec_bridge", view = self.engine.current_view(),
                "leader build skipped: this member cannot seal a gov5 block; the view will time out");
            self.next_build_at = None;
            self.next_slot_timestamp = None;
            return;
        }
        let el = match &self.el {
            Some(e) => e.clone(),
            None => {
                debug!(target: "n42::cl::exec_bridge", "no execution layer configured, skipping payload build");
                return;
            }
        };

        let Some(build_context) = ahead.or_else(|| self.required_payload_build_context()) else {
            error!(target: "n42::cl::exec_bridge", view = self.engine.current_view(),
                locked_view = self.engine.locked_qc().view,
                "refusing payload build: non-genesis LockedQC has a zero block hash");
            metrics::counter!("n42_locked_qc_parent_unavailable_total").increment(1);
            self.schedule_build_retry();
            return;
        };

        // Guard: prevent duplicate builds on the same parent hash.
        // Without this, eager import + speculative build can race with the finalize path,
        // spawning multiple resolve tasks that produce different blocks at the same height
        // (same parent, different timestamps). This floods reth with conflicting new_payload
        // calls, triggering pipeline sync and permanent chain stalls.
        let parent = build_context.parent_hash;
        let is_ahead = ahead.is_some();
        if let Some(building) = self.ahead.building_on(parent) {
            debug!(target: "n42::cl::exec_bridge", %parent, build_view = building.view,
                "the build started ahead covers this parent, skipping");
            if !is_ahead && building == build_context {
                // That build is this view's build.
                self.next_build_at = None;
                self.next_slot_timestamp = None;
                self.clear_leader_build_wait();
            }
            return;
        }
        if !is_ahead && self.ahead.is_emitting(build_context) {
            debug!(target: "n42::cl::exec_bridge", %parent, view = build_context.view,
                "this view's block was built ahead and is being released, skipping");
            self.next_build_at = None;
            self.next_slot_timestamp = None;
            self.clear_leader_build_wait();
            return;
        }
        if let Some(building) = self.building_on_parent
            && building.parent_hash == parent
        {
            debug!(target: "n42::cl::exec_bridge", %parent, build_view = building.view,
                current_view = build_context.view, "build already in progress on this parent, skipping");
            return;
        }
        if self.engine.last_voted_view() >= build_context.view {
            debug!(
                target: "n42::cl::exec_bridge",
                view = build_context.view,
                last_voted_view = self.engine.last_voted_view(),
                %parent,
                "leader already released a proposal/vote for this view; suppressing duplicate build"
            );
            self.next_build_at = None;
            self.next_slot_timestamp = None;
            self.clear_leader_build_wait();
            return;
        }

        // This build supersedes any retry timer left behind by an earlier FCU
        // Syncing result. Catch-up completion can invoke this method before that
        // timer fires; leaving it armed would build a second child after the first
        // proposal has already been released.
        if !is_ahead {
            self.next_build_at = None;
            self.next_slot_timestamp = None;
            self.clear_leader_build_wait();
        }
        let build_start = Instant::now();
        let pool_depth = self.pool_depth_snapshot();
        let view = build_context.view;
        self.record_timeout_diag_build_start(
            view,
            parent,
            pool_depth.pending,
            pool_depth.queued,
            build_start,
        );
        metrics::gauge!("n42_pool_pending_at_build_start").set(pool_depth.pending as f64);
        metrics::gauge!("n42_pool_queued_at_build_start").set(pool_depth.queued as f64);
        info!(
            target: "n42::cl::exec_bridge",
            view,
            %parent,
            pool_pending = pool_depth.pending,
            pool_queued = pool_depth.queued,
            async_finalize_fcu = self.async_finalize_fcu,
            "N42_POOL_AT_BUILD_START"
        );
        let should_record_commit_to_build = self.last_commit_hash == Some(parent)
            && self.commit_to_build_recorded_parent != Some(parent);
        if should_record_commit_to_build && let Some(last_commit) = self.last_commit_instant {
            let commit_to_build_start_ms =
                build_start.duration_since(last_commit).as_millis() as u64;
            metrics::histogram!("n42_commit_to_build_start_ms")
                .record(commit_to_build_start_ms as f64);
            self.commit_to_build_recorded_parent = Some(parent);
            info!(
                target: "n42::cl::exec_bridge",
                view,
                %parent,
                last_commit_view = self.last_commit_view.unwrap_or_default(),
                last_commit_hash = ?self.last_commit_hash,
                commit_to_build_start_ms,
                pool_pending = pool_depth.pending,
                pool_queued = pool_depth.queued,
                async_finalize_fcu = self.async_finalize_fcu,
                "N42_CADENCE: commit->build_start"
            );
        }
        let parent_beacon_block_root = match self.committee_parent_beacon_root(parent) {
            Ok(root) => root,
            Err(error) => {
                metrics::counter!("n42_gov5_committee_evidence_stamp_failed_total").increment(1);
                error!(
                    target: "n42::cl::exec_bridge",
                    view,
                    %parent,
                    %error,
                    "REFUSING payload build: cannot stamp the committee-evidence root (parentBeaconRoot); the view will time out"
                );
                self.next_build_at = None;
                self.next_slot_timestamp = None;
                return;
            }
        };
        if is_ahead {
            self.ahead.start_building(build_context);
        } else {
            self.building_on_parent = Some(build_context);
            self.build_triggered_at = Some(build_start);
        }

        let attrs = self.build_payload_attributes(slot_timestamp, parent_beacon_block_root);
        let timestamp = attrs.timestamp;

        let same_execution_branch = parent == self.head_block_hash;
        let committed_tags = self.fcu_state(self.head_block_hash, self.head_block_hash);
        let fcu_state = ForkchoiceState {
            // Build on the branch certified by LockedQC. When the local head
            // differs, it may be a sibling rather than an ancestor; advertising
            // it as safe/finalized would make the FCU internally inconsistent.
            // Zero means "no new safe/finalized assertion". If reth has not
            // imported the LockedQC block, it returns Syncing/no payload and the
            // existing retry path defers instead of falling back to another fork.
            head_block_hash: parent,
            // E14: on Profile 1 the committed head's tags are its parent
            // and a persisted ancestor, not the head itself.
            safe_block_hash: if same_execution_branch {
                committed_tags.safe_block_hash
            } else {
                B256::ZERO
            },
            finalized_block_hash: if same_execution_branch {
                committed_tags.finalized_block_hash
            } else {
                B256::ZERO
            },
        };

        debug!(target: "n42::cl::exec_bridge", %parent,
            execution_head = %self.head_block_hash, locked_view = self.engine.locked_qc().view,
            timestamp, "triggering payload build on LockedQC branch via fork_choice_updated");

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

            match el
                .fork_choice_updated_with_attrs_for(
                    ExecutionPath::LIVE_SEQUENTIAL,
                    fcu_state,
                    try_attrs,
                )
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
                            el.clone(),
                            BuildSource::Job(payload_id),
                            build_start,
                            build_context,
                        );
                    } else {
                        if is_ahead {
                            // Nothing is owed yet: the view's own build
                            // starts when the view does.
                            self.ahead.end_building(build_context);
                            last_err = None;
                            break;
                        }
                        warn!(target: "n42::cl::exec_bridge", "fork_choice_updated did not return payload_id, scheduling retry");
                        // FCU returned SYNCING — reth hasn't caught up yet.
                        // Clear both guards so the retry can re-attempt.
                        self.building_on_parent = None;
                        self.build_triggered_at = None;
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
        if is_ahead {
            if let Some(e) = last_err {
                debug!(target: "n42::cl::exec_bridge", error = %e, "build ahead not started");
                self.ahead.end_building(build_context);
            }
        } else if let Some(e) = last_err {
            error!(target: "n42::cl::exec_bridge", error = %e, "fork_choice_updated failed after retry");
            // Clear both guards so retry can re-attempt.
            self.building_on_parent = None;
            self.build_triggered_at = None;
            self.speculative_build_hash = None;
            // Also schedule retry on FCU error — the execution layer may recover.
            self.schedule_build_retry();
        }
    }

    /// Starts the build of `context` on its parent's execution output: the
    /// parent is an own block that is sealed, which the engine may not have
    /// yet (`N42_BUILD_ON_OUTPUT`).
    pub(super) async fn build_on_output(&mut self, context: super::PayloadBuildContext) {
        if self.leader_disabled {
            return;
        }
        let Some(el) = self.el.clone() else {
            return;
        };
        let parent = context.parent_hash;
        let covered = |building: Option<super::PayloadBuildContext>| {
            building.is_some_and(|building| building.parent_hash == parent)
        };
        if self.ahead.building_on(parent).is_some()
            || covered(self.building_on_parent)
            || self.engine.last_voted_view() >= context.view
        {
            debug!(target: "n42::cl::exec_bridge", %parent, view = context.view,
                "a build on this parent is under way or its view is voted, not building on its output");
            return;
        }
        let parent_beacon_block_root = match self.committee_parent_beacon_root(parent) {
            Ok(root) => root,
            Err(error) => {
                warn!(target: "n42::cl::exec_bridge", %parent, %error,
                    "cannot stamp the committee-evidence root yet; the child is built once the block is validated");
                return;
            }
        };
        let attributes = self.build_payload_attributes(None, parent_beacon_block_root);
        self.last_committed_timestamp = self.last_committed_timestamp.max(attributes.timestamp);
        self.ahead.start_building(context);
        self.ahead.start_output_build(context);
        metrics::counter!("n42_build_on_output_started_total").increment(1);
        info!(target: "n42::cl::exec_bridge", view = context.view, %parent,
            views_ahead = context.view.saturating_sub(self.engine.current_view()),
            "building on the sealed own block's output");
        self.spawn_payload_resolve_task(
            el,
            BuildSource::SealedParent { parent, attributes },
            Instant::now(),
            context,
        );
    }

    /// Makes the own block `head` the execution layer's head, as the build
    /// of its child through a forkchoice update would have.
    pub(super) async fn announce_own_head(&mut self, head: B256) {
        let Some(el) = self.el.clone() else {
            return;
        };
        let state = ForkchoiceState {
            head_block_hash: head,
            safe_block_hash: B256::ZERO,
            finalized_block_hash: B256::ZERO,
        };
        match el
            .fork_choice_updated_for(ExecutionPath::LIVE_SEQUENTIAL, state)
            .await
        {
            Ok(updated) => debug!(target: "n42::cl::exec_bridge", %head,
                status = ?updated.payload_status.status, "own block announced as head"),
            Err(error) => warn!(target: "n42::cl::exec_bridge", %head, %error,
                "own block could not be announced as head"),
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

    /// Returns the only parent an honest leader may build on in the current
    /// view. Genesis has no signed parent certificate, so it uses reth's
    /// execution-confirmed head. Every later view is anchored to LockedQC.
    pub(super) fn required_payload_build_context(&self) -> Option<super::PayloadBuildContext> {
        let locked_qc = self.engine.locked_qc();
        let parent_hash = if locked_qc.view == 0 {
            self.head_block_hash
        } else if locked_qc.block_hash == B256::ZERO {
            return None;
        } else {
            locked_qc.block_hash
        };
        Some(super::PayloadBuildContext {
            view: self.engine.current_view(),
            parent_hash,
        })
    }

    fn spawn_payload_resolve_task(
        &self,
        el: Arc<dyn ExecutionLayer>,
        source: BuildSource,
        build_start: Instant,
        build_context: super::PayloadBuildContext,
    ) {
        let block_ready_tx = self.block_ready_tx.clone();
        let network = self.network.clone();
        let leader_payload_tx = self.leader_payload_tx.clone();
        let current_view = build_context.view;
        let blob_store = self.blob_store.clone();
        let eager_import_done_tx = self.eager_import_done_tx.clone();
        // The diff feeds only the JMT/Twig sidecars; without one, decoding the
        // execution output a second time is wasted work on every block.
        let state_diff_ready_tx =
            (self.jmt.is_some() || self.twig.is_some()).then(|| self.state_diff_ready_tx.clone());
        let build_complete_tx = self.build_complete_tx.clone();
        let completed_context = build_context;
        let block_guard = self.eager_import_block_guard.clone();
        let exec_output_cache = self.exec_output_cache.clone();
        let bad_blocks = self.bad_blocks.clone();
        let h2_v4_participant = self.h2_v4_identity.is_some();
        // The last CommitQC held when the build starts goes into the header
        // (sync standard section 3.2). It names a committed block, which is an
        // ancestor of every block on the locked branch, ahead builds included.
        let header_qc = h2_v4_participant
            .then(|| n42_network::Gov5HeaderQc::from_commit_qc(self.engine.last_committed_qc()))
            .flatten();
        // Profile 1: the built block is sealed into its 24-element header.
        let profile1_seal_key = self.profile1.as_ref().and_then(|gate| gate.seal_key());
        let profile1 = self.profile1.is_some();

        let handle = tokio::spawn(async move {
            let build_el = el.clone();
            let on_sealed = matches!(source, BuildSource::SealedParent { .. });
            let resolve = async move {
                match source {
                    BuildSource::Job(payload_id) => {
                        // Allow builder time to pack transactions from the pool.
                        let warmup = builder_warmup_delay();
                        if !warmup.is_zero() {
                            tokio::time::sleep(warmup).await;
                        }
                        build_el
                            .resolve_payload(payload_id, ResolveKind::WaitForPending)
                            .await
                    }
                    BuildSource::SealedParent { parent, attributes } => {
                        build_el.build_on_sealed(parent, attributes).await
                    }
                }
            };
            let resolve_result = tokio::time::timeout(PAYLOAD_BUILD_TIMEOUT, resolve).await;

            let payload_opt = match resolve_result {
                Ok(result) => result,
                Err(_) => {
                    error!(target: "n42::cl::exec_bridge", "payload build timed out after {}s", PAYLOAD_BUILD_TIMEOUT.as_secs());
                    return;
                }
            };

            match payload_opt {
                Some(Ok(built)) => {
                    handle_built_payload(
                        built,
                        el,
                        network,
                        block_ready_tx,
                        leader_payload_tx,
                        current_view,
                        h2_v4_participant,
                        blob_store,
                        exec_output_cache,
                        bad_blocks,
                        eager_import_done_tx,
                        state_diff_ready_tx,
                        block_guard,
                        build_start,
                        build_context,
                        header_qc,
                        profile1.then_some(profile1_seal_key),
                    )
                    .await;
                }
                // A build on a sealed block that cannot run is built again
                // the usual way once the block is validated.
                Some(Err(e)) if on_sealed => {
                    warn!(target: "n42::cl::exec_bridge", error = %e, "build on the sealed own block did not run")
                }
                Some(Err(e)) => {
                    error!(target: "n42::cl::exec_bridge", error = %e, "payload build failed")
                }
                None => {
                    warn!(target: "n42::cl::exec_bridge", "payload not found (already resolved or expired)")
                }
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
            if build_complete_tx.send(completed_context).await.is_err() {
                debug!(target: "n42::cl::exec_bridge", "build completion receiver dropped");
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
        let mut data = data;
        let mut broadcast: BlockDataBroadcast = match bincode::deserialize(&data) {
            Ok(b) => b,
            Err(e) => {
                warn!(target: "n42::cl::exec_bridge", "invalid block data broadcast: {e}");
                return;
            }
        };
        // A frame-referenced payload is put back into the full form here, so
        // everything below, and every later reader of the cached bytes, sees
        // an ordinary block. Without the frames nothing is cached for this
        // hash: the block's full copy (the Gov5 body in H2-v4) is taken when
        // it arrives.
        if super::frame_ref::is_frame_ref_payload(&broadcast.payload_json) {
            let expand_start = std::time::Instant::now();
            let wire_bytes = broadcast.payload_json.len();
            match super::frame_ref::expand_frame_ref_payload(
                &broadcast.payload_json,
                n42_tx_types::FrameStore::global(),
            ) {
                Ok(full) => {
                    broadcast.payload_json = full;
                    data = match bincode::serialize(&broadcast) {
                        Ok(data) => data,
                        Err(error) => {
                            warn!(target: "n42::cl::exec_bridge", %error, "failed to re-encode expanded block data");
                            return;
                        }
                    };
                    metrics::counter!("n42_frame_ref_blocks_expanded_total", "outcome" => "ok")
                        .increment(1);
                    info!(
                        target: "n42::cl::exec_bridge",
                        hash = %broadcast.block_hash,
                        wire_bytes,
                        full_bytes = broadcast.payload_json.len(),
                        expand_ms = expand_start.elapsed().as_millis() as u64,
                        "N42_FRAME_REF: block payload rebuilt from held frames"
                    );
                }
                Err(error) => {
                    let outcome = match error {
                        super::frame_ref::FrameRefError::Missing { .. } => "missing",
                        super::frame_ref::FrameRefError::Malformed(_) => "malformed",
                    };
                    metrics::counter!("n42_frame_ref_blocks_expanded_total", "outcome" => outcome)
                        .increment(1);
                    warn!(
                        target: "n42::cl::exec_bridge",
                        hash = %broadcast.block_hash,
                        %error,
                        "N42_FRAME_REF: cannot rebuild block payload; waiting for the full copy"
                    );
                    return;
                }
            }
        }

        let hash = broadcast.block_hash;
        if self.profile1.is_some() {
            // Profile 1: the 24-element header is checked against its
            // admitted parent before anything is cached, imported or voted.
            if !self.profile1_admit_hash(
                hash,
                broadcast.native_header_rlp.as_deref(),
                "follower_import",
            ) {
                return;
            }
        } else if let Some(raw) = broadcast.native_header_rlp.as_deref()
            && !super::remember_direct_native_header(hash, raw)
        {
            warn!(target: "n42::cl::exec_bridge", %hash, "block data carries an invalid native header");
            return;
        }
        let payload_len = broadcast.payload_json.len();
        // The Gov5 fetch is NOT retired here. At this point `hash` is only a
        // self-declared field of an unauthenticated bincode envelope — the
        // payload behind it is not checked against it until the eager import
        // below. Retiring on it would let anyone who can reach this port cancel
        // an in-flight ancestry fetch and suppress re-requests for 30s by
        // gossiping a forged envelope carrying the victim's target hash.
        // `handle_eager_import_done` retires it instead: reth has accepted that
        // exact payload by then, so the hash is proven, not merely claimed.
        if self.bad_blocks.should_skip(hash, "block_data") {
            return;
        }
        let duplicate_bytes = data.len();
        self.drop_rejected_block_data();
        let duplicate = self.pending_block_data.contains_key(&hash);
        self.record_timeout_diag_block_data_received(
            broadcast.view,
            hash,
            duplicate_bytes,
            broadcast.leader_ready_unix_ms,
            duplicate,
        );

        // Dedup: skip if we already have this block (direct push + GossipSub overlap).
        if duplicate {
            metrics::counter!("n42_block_data_dup_hash_drop_total").increment(1);
            metrics::counter!("n42_block_data_dup_hash_drop_bytes_total")
                .increment(duplicate_bytes as u64);
            metrics::histogram!("n42_block_data_dup_hash_drop_bytes")
                .record(duplicate_bytes as f64);
            debug!(
                target: "n42::cl::exec_bridge",
                %hash,
                bytes = duplicate_bytes,
                "N42_DUP_HASH_DROP: duplicate block data, skipping"
            );
            return;
        }

        self.pending_executions.remove(&hash);

        // Pipeline: follower received block data — create timing entry.
        // `build_complete` is set immediately (the leader already built it).
        let mut timing = super::PipelineTiming::new_follower();
        timing.build_complete = Some(tokio::time::Instant::now());
        self.record_pipeline_timing(hash, timing);

        // `broadcast.timestamp` is unauthenticated and must not reach the build
        // floor: `last_committed_timestamp` sets the next block's timestamp, and
        // no post-merge header rule bounds it. The eager import below feeds the
        // floor from the payload once reth has returned Valid for it.

        let pending_finalization_hash = self
            .pending_finalization
            .as_ref()
            .map(|pending| pending.block_hash)
            .unwrap_or(hash);
        if !self.cache_pending_block_data_with_metadata(
            hash,
            data,
            &[hash, pending_finalization_hash],
            broadcast.view,
            payload_len,
        ) {
            warn!(
                target: "n42::cl::exec_bridge",
                %hash,
                "failed to cache block data, skipping consensus import notification"
            );
            return;
        }

        // Native mode preserves optimistic voting on cached data. Gov5 H2
        // participant mode releases its vote only from eager_import_done after
        // reth returns Valid for this exact payload.
        //
        // N42_COLOCATED_VOTE=1 (measurement route, default off) releases the
        // H2 vote on cached data as well: the validators of one operator on
        // one machine vote on the check the block's builder made on its own
        // block, and import behind the vote. A follower then no longer
        // executes before it votes, so the setting is wrong for validators
        // that do not trust each other.
        if self.h2_v4_identity.is_none() || colocated_vote_enabled() {
            debug!(target: "n42::cl::exec_bridge", %hash, "block data cached, notifying consensus (deferred execution)");
            if let Err(e) = self
                .engine
                .process_event(ConsensusEvent::BlockImported(hash))
            {
                error!(target: "n42::cl::exec_bridge", error = %e, "error processing BlockImported for deferred execution");
            }
        }

        // Follower eager import: start new_payload + fcu in parallel with consensus voting.
        // By the time finalize_committed_block runs after consensus commit, the block is
        // likely already in reth (Case A), eliminating the ~200ms background import stall.
        if let Some(ref el) = self.el {
            let eh = el.clone();
            let payload_compressed = broadcast.payload_json;
            let execution_output_compressed = broadcast.execution_output;
            let view = broadcast.view;
            let leader_ready_unix_ms = broadcast.leader_ready_unix_ms;
            let block_data_received = std::time::Instant::now();
            let eager_done_tx = self.eager_import_done_tx.clone();
            let state_diff_ready_tx = (self.jmt.is_some() || self.twig.is_some())
                .then(|| self.state_diff_ready_tx.clone());
            let block_guard = self.eager_import_block_guard.clone();
            let exec_cache = self.exec_output_cache.clone();
            let bad_blocks = self.bad_blocks.clone();
            let rejected_block_data = self.rejected_block_data.clone();
            // The cached envelope turned out not to be the block of `hash`.
            let reject_block_data = move || {
                rejected_block_data
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(hash);
            };
            let eager_span = tracing::info_span!(
                target: "n42.cl.exec_bridge.eager_import",
                "follower_eager_import",
                %hash,
                view,
            );
            let in_flight =
                super::EagerImportInFlight::enter(&self.eager_imports_in_flight, hash);
            tokio::spawn(async move {
                let _in_flight = in_flight;
                let decompress_start = std::time::Instant::now();
                let payload_wire = match super::decompress_payload(&payload_compressed) {
                    Ok(d) => d,
                    Err(_) => {
                        reject_block_data();
                        return;
                    }
                };
                let decompress_ms = decompress_start.elapsed().as_millis() as u64;
                // Deserialize first, then use typed accessor for block number.
                let deser_start = std::time::Instant::now();
                let payload_format = super::execution_payload_wire_format(&payload_wire);
                let decompressed_len = payload_wire.len();
                let execution_data: alloy_rpc_types_engine::ExecutionData =
                    match super::decode_execution_payload_owned(payload_wire) {
                        Ok(data) => data,
                        Err(error) => {
                            // The same bytes decode on the other validators,
                            // so decode them once more from a fresh
                            // decompression before giving the block up.
                            let retried = super::decompress_payload_independent(&payload_compressed)
                                .and_then(super::decode_execution_payload_owned);
                            match retried {
                                Ok(data) => {
                                    metrics::counter!("n42_payload_decode_retry_total", "outcome" => "recovered").increment(1);
                                    warn!(target: "n42::cl::exec_bridge", %hash, %error, payload_format, "execution payload decoded on the second attempt");
                                    data
                                }
                                Err(retry_error) => {
                                    metrics::counter!("n42_payload_decode_retry_total", "outcome" => "failed").increment(1);
                                    warn!(target: "n42::cl::exec_bridge", %hash, %error, %retry_error, payload_format, "failed to decode execution payload");
                                    reject_block_data();
                                    return;
                                }
                            }
                        }
                    };
                if execution_data.block_hash() != hash {
                    warn!(
                        target: "n42::cl::exec_bridge",
                        %hash,
                        payload_hash = %execution_data.block_hash(),
                        "block-data envelope hash does not match execution payload; dropping"
                    );
                    metrics::counter!("n42_block_data_payload_hash_mismatch_total").increment(1);
                    reject_block_data();
                    return;
                }
                let block_ts = execution_data.payload.timestamp();
                let deser_ms = deser_start.elapsed().as_millis() as u64;
                let ready_to_decode_ms = elapsed_since_unix_ms(leader_ready_unix_ms);
                if let Some(elapsed) = ready_to_decode_ms {
                    metrics::histogram!("n42_follower_ready_to_decode_ms").record(elapsed as f64);
                }
                info!(
                    target: "n42::cl::exec_bridge",
                    %hash,
                    payload_format,
                    compressed_kb = payload_compressed.len() / 1024,
                    decompressed_kb = decompressed_len / 1024,
                    decompress_ms,
                    deser_ms,
                    leader_ready_unix_ms,
                    has_leader_ready_ts = leader_ready_unix_ms > 0,
                    ready_to_decode_ms = ready_to_decode_ms.unwrap_or_default(),
                    has_compact_block = execution_output_compressed.is_some(),
                    "N42_DECOMPRESS: follower payload decoded"
                );
                // Only a block previously accepted as Valid may suppress another
                // submission at this height. Advancing this watermark before
                // `new_payload` returns lets an out-of-order child that returns
                // Syncing poison the whole catch-up batch by suppressing its
                // missing parents.
                let block_number = execution_data.block_number();
                let parent_hash = execution_data.parent_hash();
                let tx_count = execution_data.transaction_count();
                let has_staking_target =
                    super::execution_might_contain_staking_target(&execution_data);
                if bad_blocks.should_skip(hash, "block_data_eager") {
                    return;
                }
                if let Some(validated) =
                    eager_import_already_validated(block_guard.as_ref(), block_number)
                {
                    info!(target: "n42::cl::exec_bridge", %hash, view, block_number, validated, "follower eager import: skipping already validated block number");
                    return;
                }

                // The compact execution bundle is already needed by reth's
                // cache injection. Derive the sidecar delta on a blocking
                // worker while new_payload and consensus progress, then expose
                // it only if reth validates this exact payload below.
                let state_diff_task = state_diff_ready_tx.clone().zip(execution_output_compressed.as_ref()).map(|(state_diff_ready_tx, exec_bytes)| {
                    let exec_bytes = exec_bytes.clone();
                    (state_diff_ready_tx, tokio::task::spawn_blocking(move || {
                        ConsensusService::extract_state_diff_from_execution_output(
                            hash,
                            &exec_bytes,
                        )
                    }))
                });

                // Compact Block: load execution output into payload cache before `new_payload`.
                // This lets reth skip EVM re-execution (cache hit path), reducing import from
                // ~209ms to ~22ms. Safety: state root is still verified by reth's new_payload.
                let compact_injected = execution_output_compressed.as_ref().is_some_and(|exec| {
                    compact_block_enabled()
                        && exec_cache
                            .as_ref()
                            .map(|c| c.inject_for_payload(&execution_data, exec, "block_data_eager"))
                            .unwrap_or(false)
                });
                let ready_to_compact_inject_ms = execution_output_compressed
                    .as_ref()
                    .and(elapsed_since_unix_ms(leader_ready_unix_ms));
                if let Some(elapsed) = ready_to_compact_inject_ms {
                    metrics::histogram!("n42_follower_ready_to_compact_inject_ms")
                        .record(elapsed as f64);
                }

                // Follower eager import: only run new_payload (no FCU).
                // new_payload inserts the block into reth's engine tree so that
                // finalize_committed_block's FCU can accept it instantly (Case A).
                // We intentionally skip fork_choice_updated here to avoid changing
                // the canonical chain — speculative blocks may not match what consensus
                // ultimately commits, and premature FCU causes reorgs that stall the chain.
                let import_start = std::time::Instant::now();
                // A verdict reached with a peer's execution output describes
                // that output. If it is not Valid, drop the output and have
                // the block executed here once before judging the block.
                let retry_data = compact_injected.then(|| execution_data.clone());
                let mut compact_injected = compact_injected;
                let mut import_result = eh
                    .new_payload_for(ExecutionPath::LIVE_SEQUENTIAL, execution_data)
                    .await;
                if let Some(retry_data) = retry_data
                    && !matches!(&import_result, Ok(status) if payload_was_fully_validated(&status.status))
                {
                    if let Some(ref cache) = exec_cache {
                        cache.evict(hash);
                    }
                    metrics::counter!("n42_compact_block_cache_misses").increment(1);
                    metrics::counter!("n42_shared_execution_refused_total").increment(1);
                    warn!(
                        target: "n42::cl::exec_bridge",
                        %hash,
                        view,
                        verdict = ?import_result.as_ref().map(|status| &status.status),
                        "peer execution output not accepted; executing the block locally"
                    );
                    compact_injected = false;
                    import_result = eh
                        .new_payload_for(ExecutionPath::LIVE_SEQUENTIAL, retry_data)
                        .await;
                }
                match import_result {
                    // Only `Valid` marks the block eager-validated. `Accepted`
                    // (stored, not executed) must fall through to the stale arm
                    // so a later commit never promotes an unexecuted block (F3).
                    Ok(status) if payload_was_fully_validated(&status.status) => {
                        mark_eager_import_valid(block_guard.as_ref(), block_number);
                        let np_elapsed = import_start.elapsed().as_millis() as u64;
                        let follower_import_ms = block_data_received.elapsed().as_millis() as u64;
                        let ready_to_accept_ms = elapsed_since_unix_ms(leader_ready_unix_ms);
                        note_virtual_block_credit(tx_count, "follower_payload_accepted");
                        metrics::histogram!("n42_follower_import_ms")
                            .record(follower_import_ms as f64);
                        if let Some(elapsed) = ready_to_accept_ms {
                            metrics::histogram!("n42_follower_ready_to_accept_ms")
                                .record(elapsed as f64);
                        }
                        info!(
                            target: "n42::cl::exec_bridge",
                            %hash,
                            view,
                            np_elapsed,
                            compact_injected,
                            leader_ready_unix_ms,
                            has_leader_ready_ts = leader_ready_unix_ms > 0,
                            ready_to_decode_ms = ready_to_decode_ms.unwrap_or_default(),
                            ready_to_compact_inject_ms =
                                ready_to_compact_inject_ms.unwrap_or_default(),
                            ready_to_accept_ms = ready_to_accept_ms.unwrap_or_default(),
                            follower_import_ms,
                            "follower eager import: new_payload accepted (no FCU)"
                        );
                        info!(
                            target: "n42::cl::exec_bridge",
                            %hash,
                            view,
                            leader_ready_unix_ms,
                            ready_to_decode_ms = ready_to_decode_ms.unwrap_or_default(),
                            ready_to_compact_inject_ms =
                                ready_to_compact_inject_ms.unwrap_or_default(),
                            ready_to_accept_ms = ready_to_accept_ms.unwrap_or_default(),
                            np_elapsed,
                            follower_import_ms,
                            compact_injected,
                            "N42_FOLLOWER_PATH: ready->decode->inject->accept"
                        );
                        info!(
                            target: "n42::cl::exec_bridge",
                            %hash,
                            view,
                            compressed_kb = payload_compressed.len() / 1024,
                            tx_count,
                            decompress_ms,
                            deser_ms,
                            np_elapsed,
                            follower_import_ms,
                            compact_injected,
                            "N42_FOLLOWER_IMPORT: block_data->accepted"
                        );
                        if compact_injected {
                            metrics::counter!("n42_compact_block_cache_hits").increment(1);
                        }
                        metrics::counter!("n42_follower_eager_import_hits_total").increment(1);
                        metrics::counter!(
                            "n42_eager_import_outcomes_total",
                            "role" => "follower", "outcome" => "hit"
                        )
                        .increment(1);
                        if eager_done_tx
                            .send((
                                hash,
                                block_ts,
                                parent_hash,
                                block_number,
                                has_staking_target,
                                None,
                            ))
                            .await
                            .is_err()
                        {
                            debug!(target: "n42::cl::exec_bridge", %hash, view, "eager import completion receiver dropped");
                        }
                        if let Some((state_diff_ready_tx, task)) = state_diff_task {
                            match task.await {
                                Ok(Some(state_diff)) => {
                                    if state_diff_ready_tx.send((hash, state_diff)).await.is_err() {
                                        debug!(target: "n42::cl::exec_bridge", %hash, view, "state-diff completion receiver dropped");
                                    }
                                }
                                Ok(None) => {
                                    metrics::counter!("n42_state_diff_precompute_missing_total", "role" => "follower").increment(1);
                                }
                                Err(error) => {
                                    warn!(target: "n42::cl::exec_bridge", %hash, view, %error, "state-diff precompute task failed");
                                }
                            }
                        }
                    }
                    Ok(status) => {
                        if compact_injected && let Some(ref cache) = exec_cache {
                            cache.evict(hash);
                        }
                        // The compact output was supplied by an unauthenticated
                        // peer. A non-Valid verdict can therefore describe that
                        // injected bundle, not the block identified by `hash`.
                        // Evict it above and leave the block retryable.
                        if !compact_injected {
                            bad_blocks.insert_if_invalid(
                                hash,
                                &status.status,
                                "block_data_eager",
                            );
                        }
                        // Syncing and Accepted leave the data standing: the
                        // parent may only be late. An invalid payload is not
                        // the block a later copy of this hash may carry.
                        if matches!(status.status, PayloadStatusEnum::Invalid { .. }) {
                            reject_block_data();
                        }
                        warn!(target: "n42::cl::exec_bridge", %hash, view, %parent_hash, status = ?status.status, compact_injected, elapsed_ms = import_start.elapsed().as_millis() as u64, "follower eager import: not accepted");
                        if compact_injected {
                            metrics::counter!("n42_compact_block_cache_misses").increment(1);
                        }
                        metrics::counter!(
                            "n42_eager_import_outcomes_total",
                            "role" => "follower", "outcome" => "stale"
                        )
                        .increment(1);
                    }
                    Err(e) => {
                        if compact_injected && let Some(ref cache) = exec_cache {
                            cache.evict(hash);
                        }
                        warn!(target: "n42::cl::exec_bridge", %hash, view, %parent_hash, error = %e, compact_injected, elapsed_ms = import_start.elapsed().as_millis() as u64, "follower eager import: failed");
                        if compact_injected {
                            metrics::counter!("n42_compact_block_cache_misses").increment(1);
                        }
                        metrics::counter!(
                            "n42_eager_import_outcomes_total",
                            "role" => "follower", "outcome" => "error"
                        )
                        .increment(1);
                    }
                }
            }.instrument(eager_span));
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
            // RLP decode + insert (and per-sidecar decode/insert failure logging)
            // happen inside the BlobStorePort adapter, byte-identical to before.
            blob_store.insert_rlp(tx_hash, &sidecar_rlp);
        }

        debug!(
            target: "n42::cl::exec_bridge",
            block_hash = %broadcast.block_hash,
            sidecars = sidecar_count,
            "processed blob sidecar broadcast"
        );
    }

    /// Imports a block via new_payload; queues for retry on Syncing status.
    pub(super) async fn import_and_notify(&mut self, broadcast: BlockDataBroadcast) -> bool {
        if self
            .bad_blocks
            .should_skip(broadcast.block_hash, "sync_import")
        {
            self.discard_unvalidated_sidecar_diff(broadcast.view, broadcast.block_hash);
            return false;
        }
        if !self.profile1_admit_hash(
            broadcast.block_hash,
            broadcast.native_header_rlp.as_deref(),
            "sync_import",
        ) {
            self.discard_unvalidated_sidecar_diff(broadcast.view, broadcast.block_hash);
            return false;
        }
        let engine_handle = match self.el {
            Some(ref el) => el.clone(),
            None => return false,
        };

        let payload_wire = match super::decompress_payload(&broadcast.payload_json) {
            Ok(d) => d,
            Err(e) => {
                warn!(target: "n42::cl::exec_bridge", hash = %broadcast.block_hash, "failed to decompress payload: {e}");
                return false;
            }
        };
        let execution_data: alloy_rpc_types_engine::ExecutionData =
            match super::decode_execution_payload_owned(payload_wire) {
                Ok(data) => data,
                Err(e) => {
                    warn!(target: "n42::cl::exec_bridge", hash = %broadcast.block_hash, "failed to deserialize execution payload: {e}");
                    return false;
                }
            };
        if execution_data.block_hash() != broadcast.block_hash {
            warn!(
                target: "n42::cl::exec_bridge",
                hash = %broadcast.block_hash,
                payload_hash = %execution_data.block_hash(),
                "sync envelope hash does not match execution payload; dropping"
            );
            metrics::counter!("n42_block_data_payload_hash_mismatch_total").increment(1);
            return false;
        }
        let execution_parent_hash = execution_data.parent_hash();
        let payload_timestamp = execution_data.payload.timestamp();
        if self
            .bad_blocks
            .should_skip(broadcast.block_hash, "sync_import_pre_submit")
        {
            return false;
        }

        // Compact Block: load execution output into payload cache before `new_payload`.
        let compact_injected = if let Some(ref exec_compressed) = broadcast.execution_output
            && compact_block_enabled()
            && let Some(ref cache) = self.exec_output_cache
        {
            cache.inject(broadcast.block_hash, exec_compressed, "import_and_notify")
        } else {
            false
        };

        match engine_handle
            .new_payload_for(ExecutionPath::HISTORICAL_SEQUENTIAL, execution_data)
            .await
        {
            Ok(status) => {
                if matches!(status.status, PayloadStatusEnum::Valid) {
                    self.last_committed_timestamp =
                        self.last_committed_timestamp.max(payload_timestamp);
                    self.handle_valid_import(
                        &broadcast,
                        execution_parent_hash,
                        &engine_handle,
                        &status,
                    )
                    .await;
                    true
                } else if matches!(
                    status.status,
                    PayloadStatusEnum::Syncing | PayloadStatusEnum::Accepted
                ) {
                    if compact_injected && let Some(ref cache) = self.exec_output_cache {
                        cache.evict(broadcast.block_hash);
                    }
                    // Engine API: `Accepted` means the payload was stored for a
                    // side chain WITHOUT being executed. Advancing the validated
                    // head here would flush the staged sidecar diff for an
                    // unexecuted block (re-audit F3). Treat it like `Syncing`:
                    // queue for retry until reth executes it and returns `Valid`.
                    if matches!(status.status, PayloadStatusEnum::Accepted) {
                        metrics::counter!("n42_new_payload_accepted_total").increment(1);
                    }
                    self.queue_syncing_block(&broadcast);
                    true
                } else {
                    if compact_injected && let Some(ref cache) = self.exec_output_cache {
                        cache.evict(broadcast.block_hash);
                    }
                    self.discard_unvalidated_sidecar_diff(broadcast.view, broadcast.block_hash);
                    // A compact output injected immediately before this call is
                    // peer-controlled. Its rejection must not blacklist the
                    // otherwise honest declared block hash (HIGH-1).
                    if !compact_injected {
                        self.bad_blocks.insert_if_invalid(
                            broadcast.block_hash,
                            &status.status,
                            "sync_import",
                        );
                    }
                    warn!(
                        target: "n42::cl::exec_bridge",
                        hash = %broadcast.block_hash,
                        status = ?status.status,
                        "new_payload rejected block"
                    );
                    false
                }
            }
            Err(e) => {
                if compact_injected && let Some(ref cache) = self.exec_output_cache {
                    cache.evict(broadcast.block_hash);
                }
                self.discard_unvalidated_sidecar_diff(broadcast.view, broadcast.block_hash);
                error!(target: "n42::cl::exec_bridge", hash = %broadcast.block_hash, error = %e, "new_payload failed");
                false
            }
        }
    }

    async fn handle_valid_import(
        &mut self,
        broadcast: &BlockDataBroadcast,
        parent_hash: B256,
        engine_handle: &Arc<dyn ExecutionLayer>,
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

        // Never let an import move reth's canonical head backward. The sync path
        // hoists this same guard ahead of the import (T2b), but a block queued
        // while Syncing can also be retried through here after the validated head
        // has since advanced past it — suppress the FCU before it reaches reth.
        if self.import_would_regress_head(broadcast.view, broadcast.block_hash) {
            debug!(
                target: "n42::cl::exec_bridge",
                hash = %broadcast.block_hash,
                view = broadcast.view,
                execution_validated_head_view = self.execution_validated_head_view,
                "skipping fork-choice update for a block at or below the execution-validated head"
            );
            metrics::counter!("n42_import_fcu_skipped_backward_total").increment(1);
            return;
        }

        let fcu_state = self.fcu_state(broadcast.block_hash, broadcast.block_hash);
        match engine_handle
            .fork_choice_updated_for(ExecutionPath::HISTORICAL_SEQUENTIAL, fcu_state)
            .await
        {
            Ok(result) if matches!(result.payload_status.status, PayloadStatusEnum::Valid) => {}
            Ok(result)
                if matches!(
                    result.payload_status.status,
                    PayloadStatusEnum::Syncing | PayloadStatusEnum::Accepted
                ) =>
            {
                warn!(
                    target: "n42::cl::exec_bridge",
                    hash = %broadcast.block_hash,
                    status = ?result.payload_status.status,
                    "fork_choice_updated has not executed the imported block; queuing retry"
                );
                self.queue_syncing_block(broadcast);
                return;
            }
            Ok(result) => {
                error!(
                    target: "n42::cl::exec_bridge",
                    hash = %broadcast.block_hash,
                    status = ?result.payload_status.status,
                    "fork_choice_updated rejected imported block"
                );
                return;
            }
            Err(e) => {
                error!(
                    target: "n42::cl::exec_bridge",
                    hash = %broadcast.block_hash,
                    error = %e,
                    "fork_choice_updated failed for imported block; queuing retry"
                );
                self.queue_syncing_block(broadcast);
                return;
            }
        }

        self.advance_execution_validated_head(broadcast.view, broadcast.block_hash, "sync import");
        crate::qualification_abort_at("execution_validated");
        self.complete_deferred_finalization(broadcast).await;

        if let Err(e) = self
            .engine
            .process_event(ConsensusEvent::BlockImportedWithParent {
                block_hash: broadcast.block_hash,
                parent_hash,
            })
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
        self.prune_pending_block_data_through(deferred_view);
        self.pending_executions.clear();

        self.enqueue_mobile_packet(
            broadcast.block_hash,
            deferred_view,
            "deferred finalization completed",
        )
        .await;

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
        if self
            .bad_blocks
            .should_skip(broadcast.block_hash, "syncing_queue")
        {
            return;
        }
        info!(target: "n42::cl::exec_bridge", hash = %broadcast.block_hash, "new_payload returned Syncing, queuing for retry");
        match bincode::serialize(broadcast) {
            Ok(data) => {
                if self.syncing_blocks.len() >= MAX_SYNCING_QUEUE_SIZE {
                    self.syncing_blocks.pop_front();
                }
                self.syncing_blocks.push_back((data, 0));
            }
            Err(error) => {
                warn!(
                    target: "n42::cl::exec_bridge",
                    hash = %broadcast.block_hash,
                    error = %error,
                    "failed to serialize syncing block for retry queue"
                );
            }
        }
    }

    async fn retry_syncing_blocks(&mut self, engine_handle: &Arc<dyn ExecutionLayer>) {
        let queued: Vec<(Vec<u8>, u32)> = self.syncing_blocks.drain(..).collect();
        info!(target: "n42::cl::exec_bridge", count = queued.len(), "retrying previously-syncing blocks");

        const MAX_SYNCING_RETRIES: u32 = 3;

        for (data, retry_count) in queued {
            let retry_broadcast = match bincode::deserialize::<BlockDataBroadcast>(&data) {
                Ok(b) => b,
                Err(_) => continue,
            };
            let retry_hash = retry_broadcast.block_hash;
            if self.bad_blocks.should_skip(retry_hash, "syncing_retry") {
                continue;
            }
            let retry_payload = match super::decompress_payload(&retry_broadcast.payload_json) {
                Ok(d) => d,
                Err(_) => continue,
            };
            let retry_exec: alloy_rpc_types_engine::ExecutionData =
                match super::decode_execution_payload_owned(retry_payload) {
                    Ok(d) => d,
                    Err(e) => {
                        warn!(target: "n42::cl::exec_bridge", %retry_hash, error = %e, "failed to deserialize retry payload");
                        continue;
                    }
                };
            if retry_exec.block_hash() != retry_hash {
                warn!(
                    target: "n42::cl::exec_bridge",
                    %retry_hash,
                    payload_hash = %retry_exec.block_hash(),
                    "retry envelope hash does not match execution payload; dropping"
                );
                metrics::counter!("n42_block_data_payload_hash_mismatch_total").increment(1);
                continue;
            }
            let retry_parent_hash = retry_exec.parent_hash();
            let retry_timestamp = retry_exec.payload.timestamp();
            if self
                .bad_blocks
                .should_skip(retry_hash, "syncing_retry_pre_submit")
            {
                continue;
            }

            // Compact Block: load on retry path too.
            let compact_injected = if let Some(ref exec_compressed) =
                retry_broadcast.execution_output
                && compact_block_enabled()
                && let Some(ref cache) = self.exec_output_cache
            {
                cache.inject(retry_hash, exec_compressed, "retry_syncing")
            } else {
                false
            };

            match engine_handle
                .new_payload_for(ExecutionPath::HISTORICAL_SEQUENTIAL, retry_exec)
                .await
            {
                Ok(rs) if matches!(rs.status, PayloadStatusEnum::Valid) => {
                    self.last_committed_timestamp =
                        self.last_committed_timestamp.max(retry_timestamp);
                    if self.import_would_regress_head(retry_broadcast.view, retry_hash) {
                        debug!(
                            target: "n42::cl::exec_bridge",
                            %retry_hash,
                            view = retry_broadcast.view,
                            execution_validated_head_view = self.execution_validated_head_view,
                            "skipping retry fork-choice update for a block at or below the execution-validated head"
                        );
                        metrics::counter!("n42_import_fcu_skipped_backward_total").increment(1);
                        continue;
                    }
                    info!(target: "n42::cl::exec_bridge", %retry_hash, "syncing block retry succeeded");
                    let fcu = self.fcu_state(retry_hash, retry_hash);
                    match engine_handle
                        .fork_choice_updated_for(ExecutionPath::HISTORICAL_SEQUENTIAL, fcu)
                        .await
                    {
                        Ok(result)
                            if matches!(result.payload_status.status, PayloadStatusEnum::Valid) =>
                        {
                            self.advance_execution_validated_head(
                                retry_broadcast.view,
                                retry_hash,
                                "sync import retry",
                            );
                            if let Err(e) =
                                self.engine
                                    .process_event(ConsensusEvent::BlockImportedWithParent {
                                        block_hash: retry_hash,
                                        parent_hash: retry_parent_hash,
                                    })
                            {
                                error!(target: "n42::cl::exec_bridge", error = %e, "error processing BlockImported for retry");
                            }
                        }
                        Ok(result)
                            if matches!(
                                result.payload_status.status,
                                PayloadStatusEnum::Syncing | PayloadStatusEnum::Accepted
                            ) =>
                        {
                            let next_retry = retry_count + 1;
                            if next_retry >= MAX_SYNCING_RETRIES {
                                warn!(target: "n42::cl::exec_bridge", %retry_hash, retries = next_retry, status = ?result.payload_status.status, "retry FCU exceeded max retries, dropping");
                            } else {
                                debug!(target: "n42::cl::exec_bridge", %retry_hash, retry = next_retry, status = ?result.payload_status.status, "retry FCU still not executable, re-queuing");
                                self.syncing_blocks.push_back((data, next_retry));
                            }
                        }
                        Ok(result) => {
                            warn!(target: "n42::cl::exec_bridge", %retry_hash, status = ?result.payload_status.status, "retry FCU rejected block");
                        }
                        Err(error) => {
                            let next_retry = retry_count + 1;
                            if next_retry >= MAX_SYNCING_RETRIES {
                                warn!(target: "n42::cl::exec_bridge", %retry_hash, retries = next_retry, %error, "retry FCU failed and exceeded max retries, dropping");
                            } else {
                                warn!(target: "n42::cl::exec_bridge", %retry_hash, retry = next_retry, %error, "retry FCU failed, re-queuing");
                                self.syncing_blocks.push_back((data, next_retry));
                            }
                        }
                    }
                }
                Ok(rs)
                    if matches!(
                        rs.status,
                        PayloadStatusEnum::Syncing | PayloadStatusEnum::Accepted
                    ) =>
                {
                    if compact_injected && let Some(ref cache) = self.exec_output_cache {
                        cache.evict(retry_hash);
                    }
                    // `Accepted` (stored, not executed) is retried exactly like
                    // `Syncing`: never treated as a validated head (F3).
                    let next_retry = retry_count + 1;
                    if next_retry >= MAX_SYNCING_RETRIES {
                        warn!(target: "n42::cl::exec_bridge", %retry_hash, retries = next_retry, status = ?rs.status, "syncing/accepted block exceeded max retries, dropping");
                    } else {
                        debug!(target: "n42::cl::exec_bridge", %retry_hash, retry = next_retry, status = ?rs.status, "retry still not executable, re-queuing");
                        self.syncing_blocks.push_back((data, next_retry));
                    }
                }
                Ok(rs) => {
                    if compact_injected && let Some(ref cache) = self.exec_output_cache {
                        cache.evict(retry_hash);
                    }
                    self.discard_unvalidated_sidecar_diff(retry_broadcast.view, retry_hash);
                    // Retry uses the same peer-provided compact output as the
                    // first import. Never derive a bad-block verdict from it.
                    if !compact_injected {
                        self.bad_blocks
                            .insert_if_invalid(retry_hash, &rs.status, "syncing_retry");
                    }
                    warn!(target: "n42::cl::exec_bridge", %retry_hash, status = ?rs.status, "retry rejected");
                }
                Err(e) => {
                    if compact_injected && let Some(ref cache) = self.exec_output_cache {
                        cache.evict(retry_hash);
                    }
                    self.discard_unvalidated_sidecar_diff(retry_broadcast.view, retry_hash);
                    warn!(target: "n42::cl::exec_bridge", %retry_hash, error = %e, "retry new_payload failed");
                }
            }
        }
    }
}

// ── Free functions for the spawned payload build task ──

/// A copy of `data`, its transactions copied side by side.
///
/// A transaction's bytes straight from the builder are promoted to shared
/// storage by their first copy, one allocation each; one after another that
/// is milliseconds of a full block, on the leader's way to its own
/// validation. The list is taken out for the copy and put back.
fn copy_payload(
    data: &mut alloy_rpc_types_engine::ExecutionData,
) -> alloy_rpc_types_engine::ExecutionData {
    use rayon::prelude::*;
    let transactions = std::mem::take(&mut data.payload.as_v1_mut().transactions);
    let mut copy = data.clone();
    copy.payload.as_v1_mut().transactions = if transactions.len() >= 4096 {
        transactions.par_iter().with_min_len(1024).cloned().collect()
    } else {
        transactions.clone()
    };
    data.payload.as_v1_mut().transactions = transactions;
    copy
}

/// How long a Profile-1 leader waits for its parent's execution result. The
/// builder could only build on an executed parent, so the result is normally
/// filed already; the wait covers the job that is still filing it.
const PROFILE1_PARENT_RESULT_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// The parent's execution result a Profile-1 header carries (A10/AM25), as
/// this node filed it; `None` after [`PROFILE1_PARENT_RESULT_WAIT`].
async fn profile1_parent_result(parent: B256) -> Option<n42_consensus::ExecutedFields> {
    let deadline = Instant::now() + PROFILE1_PARENT_RESULT_WAIT;
    loop {
        if let Some(fields) = n42_consensus::executed_fields(&parent) {
            return Some(fields);
        }
        if Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

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
#[tracing::instrument(
    target = "n42.cl.exec_bridge.leader_emit",
    name = "leader_emit",
    skip_all,
    fields(view = current_view, hash = tracing::field::Empty, block_number = tracing::field::Empty)
)]
async fn handle_built_payload(
    built: BuiltBlock,
    el: Arc<dyn ExecutionLayer>,
    network: Arc<dyn ConsensusNetwork>,
    block_ready_tx: mpsc::Sender<super::PayloadBuildReady>,
    leader_payload_tx: mpsc::Sender<(B256, super::SharedBlockData)>,
    current_view: u64,
    h2_v4_participant: bool,
    blob_store: Option<Arc<dyn BlobStorePort>>,
    exec_output_cache: Option<Arc<dyn ExecutionOutputCache>>,
    bad_blocks: super::bad_block_cache::BadBlockCache,
    eager_import_done_tx: mpsc::Sender<EagerImportDone>,
    state_diff_ready_tx: Option<mpsc::Sender<super::StateDiffReady>>,
    block_guard: Arc<std::sync::atomic::AtomicU64>,
    build_start: Instant,
    build_context: super::PayloadBuildContext,
    header_qc: Option<n42_network::Gov5HeaderQc>,
    // `Some` on a Profile-1 chain: the key the block is sealed with (B2).
    profile1_seal: Option<Option<Arc<n42_primitives::BlsSecretKey>>>,
) {
    let BuiltBlock {
        mut hash,
        number: block_number,
        timestamp: block_timestamp,
        tx_count,
        mut execution_data,
        blob_tx_hashes,
    } = built;
    let original_hash = hash;
    let emit_started = Instant::now();
    if h2_v4_participant {
        let (gov5_state_root, gov5_receipts_root) = match exec_output_cache
            .as_ref()
            .and_then(|cache| cache.take_gov5_normalization(&execution_data))
        {
            Some(cached) => cached,
            None => {
                error!(
                    target: "n42::interop::h2v4",
                    %original_hash,
                    block_number,
                    tx_count,
                    "refusing to normalize Gov5 payload without its executed QMDB state and receipts"
                );
                return;
            }
        };
        let transactions_root = exec_output_cache
            .as_ref()
            .and_then(|cache| cache.built_transactions_root(original_hash));
        let normalize_started = Instant::now();
        if let Some(seal_key) = profile1_seal {
            // Profile 1: the 24-element header sealed by this leader
            // (B1-B12). It carries the parent's execution result (A10/AM25);
            // the block's own goes to the registry under the sealed hash,
            // where its child's build and every import look for it.
            let Some(seal_key) = seal_key else {
                error!(target: "n42::profile1", %original_hash, "refusing to propose: this node holds no sealing key");
                return;
            };
            let parent = execution_data.parent_hash();
            let Some(carried) = profile1_parent_result(parent).await else {
                // Never guess or zero-fill: the build is dropped and the
                // view's pacemaker retries once the parent is executed.
                error!(target: "n42::profile1", %original_hash, %parent, "not proposing: the parent's execution result is not filed here");
                metrics::counter!("n42_profile1_build_parent_result_missing_total").increment(1);
                return;
            };
            let own = n42_network::profile1_built_own_fields(
                &execution_data,
                gov5_state_root,
                gov5_receipts_root,
            );
            let sealed = match n42_network::seal_built_payload_for_profile1(
                &execution_data,
                current_view,
                header_qc.as_ref(),
                &carried,
                B256::ZERO,
                &seal_key,
            ) {
                Ok(sealed) => sealed,
                Err(error) => {
                    error!(target: "n42::profile1", %original_hash, %error, "refusing to propose a payload that cannot be sealed as a Profile-1 block");
                    return;
                }
            };
            let preserves_execution_input = n42_network::profile1_seal_preserves_execution_input(
                &execution_data,
                &sealed.payload,
            );
            hash = sealed.payload.block_hash();
            if let Err(error) = n42_consensus::remember_executed_fields(hash, own) {
                error!(target: "n42::profile1", %hash, %error, "refusing to propose: the block's own execution result cannot be filed");
                return;
            }
            if let Some(ref cache) = exec_output_cache
                && (!preserves_execution_input || !cache.rekey_gov5_built(original_hash, hash))
            {
                cache.evict(original_hash);
            }
            execution_data = sealed.payload;
            metrics::histogram!("n42_leader_emit_stage_ms", "stage" => "normalize")
                .record(normalize_started.elapsed().as_secs_f64() * 1000.0);
        } else {
        let normalized = match n42_network::normalize_built_payload_for_gov5_h2(
            execution_data,
            current_view,
            gov5_state_root,
            gov5_receipts_root,
            transactions_root,
            header_qc.as_ref(),
        ) {
            Ok(normalized) => normalized,
            Err(error) => {
                error!(target: "n42::interop::h2v4", original_hash = %hash, %error, "refusing to propose a payload that cannot be normalized to the gov5 H2 header profile");
                return;
            }
        };
        hash = normalized.payload.block_hash();
        // Native Cancun normalization changes commitments and H2 metadata,
        // but preserves the EVM inputs. Move the builder result to the new
        // hash only after the adapter checks that invariant. newPayload still
        // performs full post-execution and QMDB validation before proposal.
        if hash != original_hash
            && let Some(ref cache) = exec_output_cache
        {
            let reuse_enabled = std::env::var_os("N42_GOV5_REUSE_BUILDER_EXECUTION")
                .is_none_or(|value| value != "0");
            if !reuse_enabled
                || !normalized.preserves_execution_input
                || !cache.rekey_gov5_built(original_hash, hash)
            {
                cache.evict(original_hash);
            }
        }
        execution_data = normalized.payload;
        metrics::histogram!("n42_leader_emit_stage_ms", "stage" => "normalize")
            .record(normalize_started.elapsed().as_secs_f64() * 1000.0);
        }
    }
    let actual_parent = execution_data.parent_hash();
    let payload_hash = execution_data.block_hash();
    if let Err(link_error) =
        validate_built_payload_linkage(hash, payload_hash, actual_parent, build_context.parent_hash)
    {
        match link_error {
            BuiltPayloadLinkError::Parent { actual, required } => {
                error!(target: "n42::cl::exec_bridge", %hash, %actual,
                    required_parent = %required, view = build_context.view,
                    "payload builder returned a block outside the requested LockedQC branch");
                metrics::counter!("n42_payload_parent_mismatch_total").increment(1);
            }
            BuiltPayloadLinkError::Hash { declared, payload } => {
                error!(target: "n42::cl::exec_bridge", %declared,
                    payload_hash = %payload,
                    "built payload hash mismatch; refusing to broadcast");
                metrics::counter!("n42_built_payload_hash_mismatch_total").increment(1);
            }
        }
        return;
    }
    if bad_blocks.should_skip(hash, "leader_built_payload") {
        return;
    }
    // H2 participants release the proposal only after new_payload(Valid) of
    // the normalized block. Start that validation now and let it run while
    // the payload is serialized and sent: followers validate independently
    // before voting, so sending first does not let an invalid block gather
    // votes, and it overlaps ~0.5 s of leader validation with the transfer.
    let leader_validation = h2_v4_participant.then(|| {
        let el = el.clone();
        let copy_started = Instant::now();
        let data = copy_payload(&mut execution_data);
        metrics::histogram!("n42_leader_emit_stage_ms", "stage" => "validation_copy")
            .record(copy_started.elapsed().as_secs_f64() * 1000.0);
        let import_start = std::time::Instant::now();
        let validated_tx = build_on_seal_enabled().then(|| block_ready_tx.clone());
        let task = tokio::spawn(async move {
            let outcome = el
                .new_payload_for(ExecutionPath::LIVE_SEQUENTIAL, data)
                .await;
            // The block is in the local tree now: a child can be built on it
            // while the Gov5 copy is still being encoded and sent.
            if let Some(validated_tx) = validated_tx
                && let Ok(status) = &outcome
                && payload_was_fully_validated(&status.status)
            {
                let _ = validated_tx
                    .send(super::PayloadBuildReady {
                        context: build_context,
                        block_hash: hash,
                        kind: super::ReadyKind::OwnBlockValidated,
                    })
                    .await;
            }
            outcome
        });
        (import_start, task)
    });
    if leader_validation.is_some() {
        metrics::histogram!("n42_leader_emit_stage_ms", "stage" => "to_validation_spawn")
            .record(emit_started.elapsed().as_secs_f64() * 1000.0);
    }
    // The block's insert publishes its output for a child built on it now.
    if leader_validation.is_some()
        && build_on_output_enabled()
        && block_ready_tx
            .send(super::PayloadBuildReady {
                context: build_context,
                block_hash: hash,
                kind: super::ReadyKind::OwnBlockSealed,
            })
            .await
            .is_err()
    {
        debug!(target: "n42::cl::exec_bridge", %hash, "block_ready receiver dropped");
    }
    // The Gov5 copy is encoded beside serialization and the native send; it
    // is still pushed, and the proposal still released, only after both.
    let gov5_encode = h2_v4_participant.then(|| {
        let data = copy_payload(&mut execution_data);
        tokio::task::spawn_blocking(move || {
            let encode_start = Instant::now();
            let encoded = n42_network::encode_gov5_block_rlp(&data);
            (encoded, encode_start.elapsed().as_millis() as u64)
        })
    });
    {
        let span = tracing::Span::current();
        span.record("hash", tracing::field::display(&hash));
        span.record("block_number", block_number);
    }
    // A task spawned from a runtime worker waits in that worker's own slot,
    // which no other worker takes from, until this task next yields - and the
    // serialization below does not yield. Without this the validation reached
    // the engine only after the broadcast, 60-80 ms later (run64).
    if leader_validation.is_some() {
        tokio::task::yield_now().await;
    }

    // The execution payload and compact execution output are independent wire
    // representations of the same finished block.  Large blocks spend roughly
    // 40-50 ms in each serializer, so running them serially leaves a core idle
    // on the leader's critical path.  Keep small blocks inline to avoid paying
    // thread-spawn overhead when there is no useful work to overlap.
    let should_take_execution_output = should_broadcast_execution_output(h2_v4_participant);
    let (payload_wire_result, ser_ms, execution_output_bytes) = if should_take_execution_output
        && tx_count >= 1_024
    {
        std::thread::scope(|scope| {
            let compact_task = scope.spawn(|| {
                exec_output_cache
                    .as_ref()
                    .and_then(|cache| cache.take_serialized(hash))
            });
            let ser_start = std::time::Instant::now();
            let payload_wire_result = super::encode_execution_payload(&mut execution_data);
            let ser_ms = ser_start.elapsed().as_millis() as u64;
            let execution_output_bytes = match compact_task.join() {
                Ok(bytes) => bytes,
                Err(_) => {
                    error!(
                        target: "n42::cl::exec_bridge",
                        %hash,
                        "compact-block serialization worker panicked; broadcasting payload without execution output"
                    );
                    None
                }
            };
            (payload_wire_result, ser_ms, execution_output_bytes)
        })
    } else {
        let ser_start = std::time::Instant::now();
        let payload_wire_result = super::encode_execution_payload(&mut execution_data);
        let ser_ms = ser_start.elapsed().as_millis() as u64;
        let execution_output_bytes = if should_take_execution_output {
            exec_output_cache
                .as_ref()
                .and_then(|cache| cache.take_serialized(hash))
        } else {
            None
        };
        (payload_wire_result, ser_ms, execution_output_bytes)
    };
    let encoded_payload = match payload_wire_result {
        Ok(payload) => payload,
        Err(e) => {
            error!(target: "n42::cl::exec_bridge", %hash, error = %e, "CRITICAL: failed to serialize execution payload");
            return;
        }
    };
    let has_staking_target = super::execution_might_contain_staking_target(&execution_data);
    let payload_format = encoded_payload.format;
    let payload_header_bytes = encoded_payload.header_bytes;
    let payload_transaction_bytes = encoded_payload.transaction_bytes;
    let payload_wire = encoded_payload.bytes;

    info!(
        target: "n42::cl::exec_bridge",
        %hash,
        tx_count,
        payload_format,
        payload_kb = payload_wire.len() / 1024,
        payload_bytes = payload_wire.len(),
        header_bytes = payload_header_bytes,
        transaction_bytes = payload_transaction_bytes,
        ser_ms,
        block_timestamp,
        "N42_LEADER_SERIALIZE: payload serialized"
    );

    // Compact Block: the execution output was serialized alongside payload JSON
    // above so followers can skip EVM re-execution without extending the leader
    // critical path by another full serialization pass.
    let state_diff_task = state_diff_ready_tx
        .zip(execution_output_bytes.as_ref())
        .map(|(state_diff_ready_tx, exec_bytes)| {
            let exec_bytes = exec_bytes.clone();
            let task = tokio::task::spawn_blocking(move || {
                ConsensusService::extract_state_diff_from_execution_output(hash, &exec_bytes)
            });
            (state_diff_ready_tx, task)
        });
    let leader_ready_unix_ms = now_unix_ms();
    info!(
        target: "n42::cl::exec_bridge",
        %hash,
        current_view,
        leader_ready_unix_ms,
        tx_count,
        has_compact_block = execution_output_bytes.is_some(),
        "N42_LEADER_READY: payload ready for broadcast"
    );
    info!(
        target: "n42::cl::timeout_diag",
        view = current_view,
        %hash,
        tx_count,
        leader_ready_unix_ms,
        has_compact_block = execution_output_bytes.is_some(),
        "N42_TIMEOUT_VIEW: leader_ready"
    );
    // The native header was remembered during normalization; it does not
    // depend on the Gov5 encoding below.
    let native_header_rlp = if h2_v4_participant {
        match n42_consensus::gov5_native_header_rlp(&hash) {
            Some(raw) => Some(raw.to_vec()),
            None => {
                error!(target: "n42::interop::h2v4", %hash, "normalized native header is unavailable for direct block broadcast");
                return;
            }
        }
    } else {
        None
    };
    // 1. Broadcast block data + blob sidecars to followers. Native peers start
    //    validating now; the Gov5 copy below costs ~130 ms per full block and
    //    used to run first.
    broadcast_block_data(
        network.clone(),
        &leader_payload_tx,
        hash,
        current_view,
        &payload_wire,
        block_timestamp,
        execution_output_bytes,
        leader_ready_unix_ms,
        native_header_rlp,
        build_start,
        // Only where a full copy of the block also travels (the Gov5 body):
        // a follower missing a frame falls back to it.
        h2_v4_participant && n42_tx_types::frame_ref_blocks_enabled(),
    )
    .await;
    broadcast_blob_sidecars(
        network.as_ref(),
        blob_tx_hashes,
        hash,
        current_view,
        blob_store,
    );
    // The proposal is still released only after the Gov5 copy is out, unless
    // N42_GOV5_COPY_LATE=1 (measurement route, default off): then the copy is
    // pushed behind the proposal, and a failure to encode or push it is
    // logged instead of withholding the proposal. Gov5 members receive the
    // body after the proposal that names it.
    let gov5_encode = if gov5_copy_late() {
        if let Some(gov5_encode) = gov5_encode {
            let network = network.clone();
            tokio::spawn(async move {
                match gov5_encode.await {
                    Ok((Ok(gov5_rlp), gov5_encode_ms)) => {
                        metrics::histogram!("n42_gov5_block_encode_ms")
                            .record(gov5_encode_ms as f64);
                        info!(target: "n42::cl::exec_bridge", %hash, tx_count, gov5_encode_ms,
                            gov5_bytes = gov5_rlp.len(), late = true,
                            "N42_LEADER_GOV5_ENCODE: native block encoded");
                        if let Err(error) = network.broadcast_gov5_block_reliable(gov5_rlp).await {
                            error!(target: "n42::interop::h2v4", %hash, %error, "late gov5 block broadcast failed");
                        }
                    }
                    Ok((Err(error), _)) => {
                        error!(target: "n42::interop::h2v4", %hash, %error, "late gov5 block encoding failed");
                    }
                    Err(join_error) => {
                        error!(target: "n42::interop::h2v4", %hash, %join_error, "late Gov5 block encoding task failed");
                    }
                }
            });
        }
        None
    } else {
        gov5_encode
    };
    if let Some(gov5_encode) = gov5_encode {
        let (gov5_rlp, gov5_encode_ms) = match gov5_encode.await {
            Ok((Ok(encoded), encode_ms)) => (encoded, encode_ms),
            Ok((Err(error), _)) => {
                error!(target: "n42::interop::h2v4", %hash, %error, "refusing to propose a payload that cannot be encoded for gov5 peers");
                return;
            }
            Err(join_error) => {
                error!(target: "n42::interop::h2v4", %hash, %join_error, "Gov5 block encoding task failed");
                return;
            }
        };
        metrics::histogram!("n42_gov5_block_encode_ms").record(gov5_encode_ms as f64);
        info!(target: "n42::cl::exec_bridge", %hash, tx_count, gov5_encode_ms,
            gov5_bytes = gov5_rlp.len(), "N42_LEADER_GOV5_ENCODE: native block encoded");
        if let Err(error) = network.broadcast_gov5_block_reliable(gov5_rlp).await {
            error!(target: "n42::interop::h2v4", %hash, %error, "refusing to propose after gov5 block broadcast failed");
            return;
        }
    }

    if let Some((import_start, task)) = leader_validation {
        let outcome = match task.await {
            Ok(outcome) => outcome,
            Err(join_error) => {
                error!(target: "n42::interop::h2v4", %hash, block_number, %join_error, "Gov5 leader validation task failed");
                return;
            }
        };
        match outcome {
            Ok(status) if payload_was_fully_validated(&status.status) => {
                mark_eager_import_valid(block_guard.as_ref(), block_number);
                info!(
                    target: "n42::interop::h2v4",
                    %hash,
                    block_number,
                    elapsed_ms = import_start.elapsed().as_millis() as u64,
                    "validated normalized Gov5 leader payload before proposal release"
                );
                if eager_import_done_tx
                    // The staking scan of the serialized payload is not
                    // wired here; stay conservative and force the scan.
                    .send((
                        hash,
                        block_timestamp,
                        actual_parent,
                        block_number,
                        true,
                        None,
                    ))
                    .await
                    .is_err()
                {
                    debug!(target: "n42::cl::exec_bridge", %hash, "leader eager import completion receiver dropped");
                }
            }
            Ok(status) => {
                if let Some(ref cache) = exec_output_cache {
                    cache.evict(hash);
                }
                bad_blocks.insert_if_invalid(hash, &status.status, "gov5_leader_pre_proposal");
                error!(
                    target: "n42::interop::h2v4",
                    %hash,
                    block_number,
                    status = ?status.status,
                    "refusing to release Gov5 leader proposal before new_payload(Valid)"
                );
                return;
            }
            Err(error) => {
                if let Some(ref cache) = exec_output_cache {
                    cache.evict(hash);
                }
                error!(
                    target: "n42::interop::h2v4",
                    %hash,
                    block_number,
                    %error,
                    "refusing to release Gov5 leader proposal after new_payload failure"
                );
                return;
            }
        }
    }

    // 2. Trigger consensus voting immediately (non-blocking channel send)
    if block_ready_tx
        .send(super::PayloadBuildReady {
            context: build_context,
            block_hash: hash,
            kind: super::ReadyKind::Payload,
        })
        .await
        .is_err()
    {
        debug!(target: "n42::cl::exec_bridge", %hash, "block_ready receiver dropped");
    }

    // 3. Eager import: run new_payload + fcu while consensus votes in parallel.
    //    This is the key pipelining optimization — by the time finalize_committed_block
    //    runs after consensus commit, the block is likely already in reth (Case A).
    //
    //    Guard: suppress only heights already accepted as Valid. A Syncing or
    //    Accepted child must not prevent a late parent from reaching reth.
    if bad_blocks.should_skip(hash, "leader_eager_import_pre_submit") {
        return;
    }
    if let Some(validated) = eager_import_already_validated(block_guard.as_ref(), block_number) {
        debug!(target: "n42::cl::exec_bridge", %hash, block_number, validated, "leader eager import: skipping already validated block number");
        return;
    }
    // Leader eager import: only run new_payload (no FCU).
    // Inserts block into reth's engine tree so finalize_committed_block's FCU
    // can accept it instantly. We skip FCU to avoid changing canonical chain —
    // only finalize_committed_block (after consensus commit) should do FCU.
    let import_start = std::time::Instant::now();
    match el
        .new_payload_for(ExecutionPath::LIVE_SEQUENTIAL, execution_data)
        .await
    {
        // Only `Valid` marks the block eager-validated. `Accepted` (stored, not
        // executed) falls through to the stale arm so a later commit never
        // promotes an unexecuted block (F3).
        Ok(status) if payload_was_fully_validated(&status.status) => {
            mark_eager_import_valid(block_guard.as_ref(), block_number);
            let np_elapsed = import_start.elapsed().as_millis() as u64;
            info!(target: "n42::cl::exec_bridge", %hash, np_elapsed, "eager import: new_payload accepted (no FCU)");
            metrics::counter!("n42_eager_import_hits_total").increment(1);
            metrics::counter!(
                "n42_eager_import_outcomes_total",
                "role" => "leader", "outcome" => "hit"
            )
            .increment(1);
            if eager_import_done_tx
                .send((
                    hash,
                    block_timestamp,
                    actual_parent,
                    block_number,
                    has_staking_target,
                    None,
                ))
                .await
                .is_err()
            {
                debug!(target: "n42::cl::exec_bridge", %hash, "leader eager import completion receiver dropped");
            }
            if let Some((state_diff_ready_tx, task)) = state_diff_task {
                match task.await {
                    Ok(Some(state_diff)) => {
                        if state_diff_ready_tx.send((hash, state_diff)).await.is_err() {
                            debug!(target: "n42::cl::exec_bridge", %hash, "state-diff completion receiver dropped");
                        }
                    }
                    Ok(None) => {
                        metrics::counter!("n42_state_diff_precompute_missing_total", "role" => "leader").increment(1);
                    }
                    Err(error) => {
                        warn!(target: "n42::cl::exec_bridge", %hash, %error, "state-diff precompute task failed");
                    }
                }
            }
        }
        Ok(status) => {
            if let Some(ref cache) = exec_output_cache {
                cache.evict(hash);
            }
            bad_blocks.insert_if_invalid(hash, &status.status, "leader_eager_import");
            info!(target: "n42::cl::exec_bridge", %hash, status = ?status.status, elapsed_ms = import_start.elapsed().as_millis() as u64, "eager import: new_payload not accepted");
            metrics::counter!(
                "n42_eager_import_outcomes_total",
                "role" => "leader", "outcome" => "stale"
            )
            .increment(1);
        }
        Err(e) => {
            if let Some(ref cache) = exec_output_cache {
                cache.evict(hash);
            }
            info!(target: "n42::cl::exec_bridge", %hash, error = %e, "eager import: new_payload failed");
            metrics::counter!(
                "n42_eager_import_outcomes_total",
                "role" => "leader", "outcome" => "error"
            )
            .increment(1);
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn broadcast_block_data(
    network: Arc<dyn ConsensusNetwork>,
    leader_payload_tx: &mpsc::Sender<(B256, super::SharedBlockData)>,
    hash: B256,
    current_view: u64,
    payload_wire: &[u8],
    timestamp: u64,
    execution_output: Option<Vec<u8>>,
    leader_ready_unix_ms: u64,
    native_header_rlp: Option<Vec<u8>>,
    build_start: Instant,
    frame_refs: bool,
) {
    if payload_wire.is_empty() {
        return;
    }
    let payload_format = super::execution_payload_wire_format(payload_wire);
    let compress_start = std::time::Instant::now();
    // Frame references replace the transaction bytes on the wire when this
    // node holds the frames the block was built from. The full payload is then
    // kept uncompressed for this node's own cache and for serving sync.
    let frame_refs = frame_refs
        .then(|| {
            super::frame_ref::encode_frame_ref_payload(
                payload_wire,
                n42_tx_types::FrameStore::global(),
            )
        })
        .flatten();
    let compressed = match &frame_refs {
        Some(_) => payload_wire.to_vec(),
        None => super::compress_execution_payload(payload_wire),
    };
    let compress_ms = compress_start.elapsed().as_millis() as u64;
    if let Some(refs) = &frame_refs {
        metrics::counter!("n42_frame_ref_blocks_sent_total").increment(1);
        metrics::counter!("n42_frame_ref_loose_transactions_total").increment(refs.loose as u64);
        info!(
            target: "n42::cl::exec_bridge",
            %hash,
            frames = refs.frames,
            runs = refs.runs,
            referenced = refs.referenced,
            loose = refs.loose,
            wire_bytes = refs.bytes.len(),
            full_bytes = payload_wire.len(),
            encode_ms = compress_ms,
            "N42_FRAME_REF: block payload encoded as frame references"
        );
    }
    let raw_len = payload_wire.len();
    let compressed_len = compressed.len();
    let execution_output_len = execution_output.as_ref().map_or(0, Vec::len);
    let exec_kb = execution_output_len / 1024;
    info!(
        target: "n42::cl::exec_bridge",
        %hash,
        payload_format,
        raw_bytes = raw_len,
        compressed_bytes = compressed_len,
        raw_kb = raw_len / 1024,
        compressed_kb = compressed_len / 1024,
        exec_kb,
        ratio = format_args!("{:.1}%", compressed_len as f64 / raw_len.max(1) as f64 * 100.0),
        compress_ms,
        "N42_COMPRESS: payload compressed"
    );
    let mut broadcast = BlockDataBroadcast {
        block_hash: hash,
        view: current_view,
        payload_json: compressed,
        timestamp,
        execution_output,
        leader_ready_unix_ms,
        native_header_rlp,
    };
    let own_copy = match bincode::serialize(&broadcast) {
        Ok(enc) => Arc::new(enc),
        Err(e) => {
            error!(target: "n42::cl::exec_bridge", error = %e, "failed to serialize block data broadcast");
            return;
        }
    };
    // `encoded` is what peers receive; `own_copy` always holds the full payload.
    let encoded = match frame_refs {
        Some(refs) => {
            broadcast.payload_json = refs.bytes;
            match bincode::serialize(&broadcast) {
                Ok(enc) => Arc::new(enc),
                Err(e) => {
                    error!(target: "n42::cl::exec_bridge", error = %e, "failed to serialize frame-referenced block data");
                    return;
                }
            }
        }
        None => Arc::clone(&own_copy),
    };

    // This is the first and only point where the true on-wire size of a block is
    // known: after zstd, after bincode, and including `execution_output`, none of
    // which the payload builder can see. The builder budgets by transaction count
    // and gas, and neither is denominated in bytes, so nothing upstream prevents
    // a block from landing above the propagation ceiling.
    //
    // Both send paths below fail *quietly* when that happens — GossipSub logs a
    // warning and drops the publish, receivers `Reject` it. Validators then have
    // nothing to vote on, the view cannot reach quorum, and a restart rebuilds
    // the identical block from the identical mempool. So the chain stops with
    // only a warning to show for it. Record the overrun loudly and as a metric:
    // it is the difference between an afternoon of confused log-reading and one
    // glance at a dashboard.
    metrics::gauge!("n42_block_broadcast_bytes").set(encoded.len() as f64);
    let direct_only_requested = block_direct_only_enabled();
    let propagation_budget = if direct_only_requested {
        n42_network::MAX_BLOCK_DIRECT_SIZE
    } else {
        n42_network::MAX_BROADCAST_PAYLOAD_BYTES
    };
    if encoded.len() > propagation_budget {
        metrics::counter!("n42_block_broadcast_oversized_total").increment(1);
        error!(
            target: "n42::cl::exec_bridge",
            %hash,
            current_view,
            encoded_bytes = encoded.len(),
            budget_bytes = propagation_budget,
            gossip_limit_bytes = n42_network::MAX_GOSSIP_MESSAGE_SIZE,
            raw_kb = raw_len / 1024,
            compressed_kb = compressed_len / 1024,
            exec_kb,
            "block exceeds the propagation budget and will very likely not reach \
             validators through the configured propagation mode; lower \
             N42_MAX_TXS_PER_BLOCK or the block gas limit"
        );
    }

    let build_start_to_broadcast_ms = build_start.elapsed().as_millis() as u64;
    metrics::histogram!("n42_build_start_to_broadcast_ms")
        .record(build_start_to_broadcast_ms as f64);
    info!(
        target: "n42::cl::exec_bridge",
        %hash,
        current_view,
        payload_format,
        encoded_bytes = encoded.len(),
        encoded_kb = encoded.len() / 1024,
        raw_kb = raw_len / 1024,
        compressed_kb = compressed_len / 1024,
        exec_kb,
        compress_ms,
        build_start_to_broadcast_ms,
        "N42_CADENCE: build_start->broadcast"
    );

    // Leader direct push: send to all known validator peers via QUIC unicast.
    // This bypasses GossipSub mesh flooding for large payloads.
    // Direct request-response is valuable for large payloads, but sending every
    // tiny/empty block into the same QUIC lane can build a deep substream queue
    // before a burst starts. Keep small blocks on GossipSub and reserve direct
    // push capacity for payloads where bypassing mesh relay is material.
    let direct_min_bytes = block_direct_min_bytes();
    // A direct-only run must also carry small/empty block payloads directly;
    // otherwise disabling GossipSub would create a liveness hole between load
    // waves. Outside this explicit benchmark mode, keep the size threshold.
    let direct_eligible =
        block_direct_push_enabled() && (direct_only_requested || encoded.len() >= direct_min_bytes);
    let mut validator_peers = if direct_eligible {
        network.all_validator_peers()
    } else {
        Vec::new()
    };
    let requested_fanout = block_direct_fanout();
    if !validator_peers.is_empty() {
        // Keep a bounded direct fanout deterministic and fair across views.
        // Sending one full-size copy to every validator can leave six large
        // request-response streams contending on the leader while GossipSub is
        // already providing the reliable all-node path.
        validator_peers.sort_unstable_by_key(|(validator_index, _)| *validator_index);
        let fanout = requested_fanout.min(validator_peers.len());
        let rotation = (current_view as usize) % validator_peers.len();
        validator_peers.rotate_left(rotation);
        validator_peers.truncate(fanout);
    }
    if block_direct_push_enabled() && !direct_eligible {
        metrics::counter!("n42_block_direct_skipped_small").increment(1);
        tracing::debug!(
            target: "n42::cl::exec_bridge",
            %hash,
            bytes = encoded.len(),
            direct_min_bytes,
            "skipping direct push for small block payload"
        );
    }
    let direct_count = validator_peers.len();
    // Disable the heavy block-data GossipSub copy only when the requested
    // finite direct fanout is completely available. During startup or a peer
    // outage the normal fallback remains active instead of silently reducing
    // the recipients below the benchmark's configured validator set.
    let direct_only_active = direct_only_requested
        && requested_fanout > 0
        && requested_fanout != usize::MAX
        && direct_count == requested_fanout;
    let gossip_origin_copies = usize::from(!direct_only_active);
    let logical_origin_bytes = encoded
        .len()
        .saturating_mul(gossip_origin_copies.saturating_add(direct_count));
    info!(
        target: "n42::cl::exec_bridge",
        %hash,
        current_view,
        payload_format,
        payload_raw_bytes = raw_len,
        payload_compressed_bytes = compressed_len,
        execution_output_bytes = execution_output_len,
        envelope_bytes = encoded.len(),
        direct_copies = direct_count,
        gossip_origin_copies,
        direct_only_requested,
        direct_only_active,
        logical_origin_bytes,
        "N42_COMMUNICATION: leader block propagation accounting"
    );
    // Share one immutable payload across all direct requests. The old Vec clone
    // per peer multiplied a ~15 MiB block by the validator fanout before the
    // request-response codec performed its own serialization copy.
    let direct_encoded = (!validator_peers.is_empty()).then(|| Arc::clone(&encoded));
    if direct_encoded.is_some() {
        metrics::counter!(
            "n42_block_data_copy_avoided_bytes_total",
            "site" => "direct_origin"
        )
        .increment(encoded.len() as u64);
    }
    let send_start = std::time::Instant::now();
    for (idx, peer_id) in &validator_peers {
        if let Err(error) = network
            .send_block_direct_reliable(
                *peer_id,
                direct_encoded
                    .as_ref()
                    .expect("direct payload exists when peers are present")
                    .clone(),
            )
            .await
        {
            tracing::warn!(
                target: "n42::cl::exec_bridge",
                validator_index = idx,
                %peer_id,
                error = %error,
                "failed to send direct block payload to validator peer"
            );
        }
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

    // The normal mode retains GossipSub as a reliability fallback because
    // enqueue success on block_direct is not a remote delivery acknowledgement.
    // Explicit direct-only benchmarks may skip that full-payload duplicate only
    // after the complete configured direct fanout has been resolved above.
    // A delayed fallback remains available to non-direct-only experiments.
    let gossip_delay = block_gossip_fallback_delay();
    let gossip_delay_ms = gossip_delay.as_millis() as u64;
    let mut gossip_ms = 0;
    if direct_only_active {
        metrics::counter!("n42_block_gossip_fallback_skipped_direct_only_total").increment(1);
        info!(
            target: "n42::cl::exec_bridge",
            %hash,
            encoded_kb = encoded.len() / 1024,
            direct_peers = direct_count,
            "N42_DIRECT_ONLY: block-data GossipSub fallback disabled"
        );
    } else if gossip_delay.is_zero() {
        let gossip_start = std::time::Instant::now();
        if let Err(e) = network
            .announce_block_reliable(encoded.as_ref().clone())
            .await
        {
            warn!(target: "n42::cl::exec_bridge", error = %e, "failed to broadcast block data via gossipsub");
        }
        gossip_ms = gossip_start.elapsed().as_millis() as u64;
    } else {
        let gossip_network = network.clone();
        let gossip_encoded = encoded.as_ref().clone();
        tokio::spawn(async move {
            tokio::time::sleep(gossip_delay).await;
            let gossip_start = std::time::Instant::now();
            let gossip_encoded_kb = gossip_encoded.len() / 1024;
            if let Err(e) = gossip_network.announce_block_reliable(gossip_encoded).await {
                warn!(target: "n42::cl::exec_bridge", %hash, error = %e, "delayed block-data GossipSub fallback failed");
            }
            info!(
                target: "n42::cl::exec_bridge",
                %hash,
                encoded_kb = gossip_encoded_kb,
                gossip_delay_ms,
                gossip_ms = gossip_start.elapsed().as_millis() as u64,
                "N42_GOSSIP_FALLBACK: delayed block-data gossip enqueued"
            );
        });
    }
    info!(
        target: "n42::cl::exec_bridge",
        %hash,
        encoded_kb = encoded.len() / 1024,
        direct_peers = direct_count,
        send_ms,
        gossip_ms,
        gossip_delay_ms,
        direct_only_requested,
        direct_only_active,
        total_broadcast_ms = send_ms + gossip_ms,
        "N42_BROADCAST: block propagation dispatched"
    );
    info!(
        target: "n42::cl::timeout_diag",
        view = current_view,
        %hash,
        encoded_kb = encoded.len() / 1024,
        direct_peers = direct_count,
        send_ms,
        gossip_ms,
        gossip_delay_ms,
        direct_only_requested,
        direct_only_active,
        total_broadcast_ms = send_ms + gossip_ms,
        build_start_to_broadcast_ms,
        "N42_TIMEOUT_VIEW: leader_broadcast_complete"
    );

    if leader_payload_tx.send((hash, own_copy)).await.is_err() {
        debug!(target: "n42::cl::exec_bridge", %hash, "leader payload feedback receiver dropped");
    }
}

fn block_gossip_fallback_delay() -> std::time::Duration {
    static DELAY: std::sync::OnceLock<std::time::Duration> = std::sync::OnceLock::new();
    *DELAY.get_or_init(|| {
        let delay_ms = std::env::var("N42_BLOCK_GOSSIP_FALLBACK_DELAY_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or_default()
            .min(2_000);
        std::time::Duration::from_millis(delay_ms)
    })
}

fn block_direct_push_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        !matches!(
            std::env::var("N42_BLOCK_DIRECT_PUSH").ok().as_deref(),
            Some("0") | Some("false") | Some("off")
        )
    })
}

fn block_direct_only_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        matches!(
            std::env::var("N42_BLOCK_DIRECT_ONLY").ok().as_deref(),
            Some("1") | Some("true") | Some("TRUE") | Some("yes") | Some("YES")
        )
    })
}

fn block_direct_min_bytes() -> usize {
    static MIN_BYTES: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *MIN_BYTES.get_or_init(|| {
        std::env::var("N42_BLOCK_DIRECT_MIN_BYTES")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(64 * 1024)
            .min(n42_network::MAX_GOSSIP_MESSAGE_SIZE)
    })
}

fn block_direct_fanout() -> usize {
    static FANOUT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *FANOUT.get_or_init(|| {
        std::env::var("N42_BLOCK_DIRECT_FANOUT")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(usize::MAX)
    })
}

fn broadcast_blob_sidecars(
    network: &dyn ConsensusNetwork,
    blob_tx_hashes: Vec<B256>,
    hash: B256,
    current_view: u64,
    blob_store: Option<Arc<dyn BlobStorePort>>,
) {
    let blob_store = match blob_store {
        Some(bs) => bs,
        None => return,
    };

    if blob_tx_hashes.is_empty() {
        return;
    }

    match blob_store.get_all_encoded(blob_tx_hashes) {
        Ok(encoded_sidecars) if !encoded_sidecars.is_empty() => {
            // Receivers Reject blob-topic messages above
            // MAX_BLOB_GOSSIP_MESSAGE_SIZE, so a single all-or-nothing frame
            // would be published and then refused network-wide as soon as a
            // block carries more than ~7 sidecars — the same publish/receive
            // drift 44a01a4 removed for block broadcasts. Pack into as many
            // frames as the ceiling requires instead; receivers insert each
            // frame's sidecars independently, so splitting is transparent.
            let (frames, oversized) = pack_blob_sidecar_frames(
                encoded_sidecars,
                n42_network::MAX_BLOB_GOSSIP_MESSAGE_SIZE,
            );
            if oversized > 0 {
                error!(
                    target: "n42::cl::exec_bridge",
                    %hash,
                    oversized,
                    max_frame_bytes = n42_network::MAX_BLOB_GOSSIP_MESSAGE_SIZE,
                    "blob sidecar alone outgrows a gossip frame; receivers cannot obtain it"
                );
                metrics::counter!("n42_blob_sidecar_exceeds_frame_total")
                    .increment(oversized as u64);
            }
            let frame_count = frames.len();
            for (frame_idx, sidecars) in frames.into_iter().enumerate() {
                let blob_count = sidecars.len();
                let broadcast = BlobSidecarBroadcast {
                    block_hash: hash,
                    view: current_view,
                    sidecars,
                };

                match bincode::serialize(&broadcast) {
                    Ok(encoded) => {
                        debug!(
                            target: "n42::cl::exec_bridge",
                            %hash,
                            blob_count,
                            frame = frame_idx + 1,
                            frames = frame_count,
                            bytes = encoded.len(),
                            "broadcasting blob sidecars"
                        );
                        if let Err(e) = network.broadcast_blob_sidecar(encoded) {
                            warn!(target: "n42::cl::exec_bridge", error = %e, "failed to broadcast blob sidecars");
                        }
                    }
                    Err(error) => {
                        warn!(
                            target: "n42::cl::exec_bridge",
                            %hash,
                            error = %error,
                            "failed to serialize blob sidecar broadcast"
                        );
                    }
                }
            }
        }
        Ok(_) => {}
        Err(e) => {
            warn!(target: "n42::cl::exec_bridge", %hash, error = %e, "failed to get blob sidecars from store");
        }
    }
}

/// Bincode bytes each `BlobSidecarBroadcast::sidecars` entry adds beyond its
/// RLP payload: a `B256` tx hash serializes via `serialize_bytes` as 8-byte
/// length prefix + 32 bytes, and the RLP `Vec` adds its own 8-byte prefix
/// (bincode 1 fixint encoding, the wire format both broadcast paths use).
/// `exact_budget_fill_stays_single_frame` pins these against real bincode.
const BLOB_FRAME_ENTRY_OVERHEAD: usize = 48;

/// Bincode bytes of the fixed `BlobSidecarBroadcast` header: length-prefixed
/// 32-byte block hash (40) + 8-byte view + 8-byte `sidecars` length prefix.
const BLOB_FRAME_HEADER_OVERHEAD: usize = 56;

/// One frame's worth of `(tx_hash, sidecar_rlp)` entries.
type BlobSidecarFrame = Vec<(B256, Vec<u8>)>;

/// Packs sidecars into frames whose serialized size stays within
/// `max_frame_bytes`, the blob topic's receiver Reject threshold. Order is
/// preserved. Returns the frames plus the count of sidecars dropped because a
/// single entry alone cannot fit in a frame — those are unshippable and the
/// caller must surface them.
fn pack_blob_sidecar_frames(
    sidecars: BlobSidecarFrame,
    max_frame_bytes: usize,
) -> (Vec<BlobSidecarFrame>, usize) {
    let budget = max_frame_bytes.saturating_sub(BLOB_FRAME_HEADER_OVERHEAD);
    let mut frames = Vec::new();
    let mut current: BlobSidecarFrame = Vec::new();
    let mut current_bytes = 0usize;
    let mut oversized = 0usize;

    for (tx_hash, rlp) in sidecars {
        let entry_bytes = BLOB_FRAME_ENTRY_OVERHEAD + rlp.len();
        if entry_bytes > budget {
            oversized += 1;
            continue;
        }
        if current_bytes + entry_bytes > budget {
            frames.push(std::mem::take(&mut current));
            current_bytes = 0;
        }
        current_bytes += entry_bytes;
        current.push((tx_hash, rlp));
    }
    if !current.is_empty() {
        frames.push(current);
    }
    (frames, oversized)
}

#[cfg(test)]
mod blob_frame_tests {
    use super::*;

    fn sidecar(byte: u8, len: usize) -> (B256, Vec<u8>) {
        (B256::repeat_byte(byte), vec![byte; len])
    }

    /// The overhead constants must match what bincode actually emits — the
    /// whole point of packing is that every frame clears the receiver's
    /// Reject threshold.
    #[test]
    fn packed_frames_serialize_within_the_ceiling() {
        let max = n42_network::MAX_BLOB_GOSSIP_MESSAGE_SIZE;
        // 137 KiB ≈ one EIP-4844 blob + commitment + proof; 12 of them is a
        // 1.6 MiB broadcast that previously went out as one rejected frame.
        let sidecars: Vec<_> = (0..12u8).map(|i| sidecar(i, 137 * 1024)).collect();
        let (frames, oversized) = pack_blob_sidecar_frames(sidecars.clone(), max);

        assert_eq!(oversized, 0);
        assert!(frames.len() > 1, "12 sidecars cannot fit one frame");
        let repacked: Vec<_> = frames.iter().flatten().cloned().collect();
        assert_eq!(repacked, sidecars, "order and content preserved");
        for frame in frames {
            let encoded = bincode::serialize(&BlobSidecarBroadcast {
                block_hash: B256::repeat_byte(0xFF),
                view: u64::MAX,
                sidecars: frame,
            })
            .unwrap();
            assert!(encoded.len() <= max, "frame is {} bytes", encoded.len());
        }
    }

    #[test]
    fn exact_budget_fill_stays_single_frame() {
        let max = n42_network::MAX_BLOB_GOSSIP_MESSAGE_SIZE;
        let budget = max - BLOB_FRAME_HEADER_OVERHEAD;
        let (frames, oversized) =
            pack_blob_sidecar_frames(vec![sidecar(1, budget - BLOB_FRAME_ENTRY_OVERHEAD)], max);
        assert_eq!(oversized, 0);
        assert_eq!(frames.len(), 1);
        let encoded = bincode::serialize(&BlobSidecarBroadcast {
            block_hash: B256::ZERO,
            view: 0,
            sidecars: frames.into_iter().next().unwrap(),
        })
        .unwrap();
        assert_eq!(encoded.len(), max, "overhead constants match bincode");
    }

    #[test]
    fn unshippable_sidecar_is_dropped_and_counted() {
        let max = n42_network::MAX_BLOB_GOSSIP_MESSAGE_SIZE;
        let (frames, oversized) =
            pack_blob_sidecar_frames(vec![sidecar(1, 10), sidecar(2, max), sidecar(3, 10)], max);
        assert_eq!(oversized, 1);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].len(), 2, "the shippable neighbours still go out");
    }

    #[test]
    fn empty_input_produces_no_frames() {
        let (frames, oversized) = pack_blob_sidecar_frames(Vec::new(), 1024);
        assert!(frames.is_empty());
        assert_eq!(oversized, 0);
    }
}

#[cfg(test)]
mod payload_copy_tests {
    use super::copy_payload;
    use alloy_primitives::Bytes;
    use alloy_rpc_types_engine::{
        ExecutionData, ExecutionPayload, ExecutionPayloadSidecar, ExecutionPayloadV1,
    };

    /// A payload of `count` transactions as the builder hands them over: each
    /// one's bytes in an allocation of its own that nothing shares yet.
    fn payload(count: usize) -> ExecutionData {
        let transactions = (0..count)
            .map(|index| {
                let mut raw = vec![0x50u8; 146];
                raw[1..9].copy_from_slice(&(index as u64).to_be_bytes());
                Bytes::from(raw)
            })
            .collect();
        ExecutionData {
            payload: ExecutionPayload::V1(ExecutionPayloadV1 {
                parent_hash: Default::default(),
                fee_recipient: Default::default(),
                state_root: Default::default(),
                receipts_root: Default::default(),
                logs_bloom: Default::default(),
                prev_randao: Default::default(),
                block_number: 7,
                gas_limit: 9,
                gas_used: 3,
                timestamp: 11,
                extra_data: Bytes::from_static(b"N42H"),
                base_fee_per_gas: Default::default(),
                block_hash: Default::default(),
                transactions,
            }),
            sidecar: ExecutionPayloadSidecar::none(),
        }
    }

    #[test]
    fn a_copied_payload_equals_the_one_it_was_copied_from() {
        for count in [0, 3, 4096, 9000] {
            let mut data = payload(count);
            let reference = data.clone();
            let copy = copy_payload(&mut data);
            assert_eq!(copy.payload, reference.payload, "{count} transactions: the copy");
            assert_eq!(data.payload, reference.payload, "{count} transactions: the source");
        }
    }

    /// Prints what the first copies of a full native block's payload cost one
    /// transaction after another and side by side.
    #[test]
    #[ignore = "benchmark"]
    fn copying_a_full_block_payload() {
        let ms = |at: std::time::Instant| at.elapsed().as_secs_f64() * 1e3;
        let (mut first, mut second, mut beside, mut dropped) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for _ in 0..15 {
            let data = payload(95_238);
            let at = std::time::Instant::now();
            let one = data.clone();
            first.push(ms(at));
            let at = std::time::Instant::now();
            let two = data.clone();
            second.push(ms(at));
            let at = std::time::Instant::now();
            drop(one);
            dropped.push(ms(at));
            drop(two);
            let mut data = payload(95_238);
            let at = std::time::Instant::now();
            let copy = copy_payload(&mut data);
            beside.push(ms(at));
            assert_eq!(copy.payload.as_v1().transactions.len(), 95_238);
        }
        let median = |values: &mut Vec<f64>| {
            values.sort_by(f64::total_cmp);
            values[values.len() / 2]
        };
        println!(
            "PAYLOAD_COPY first_copy_ms={:.2} later_copy_ms={:.2} drop_ms={:.2} side_by_side_first_copy_ms={:.2}",
            median(&mut first),
            median(&mut second),
            median(&mut dropped),
            median(&mut beside)
        );
    }
}

#[cfg(test)]
mod eager_import_guard_tests {
    use super::{
        BuiltPayloadLinkError, compact_block_enabled, eager_import_already_validated,
        mark_eager_import_valid, payload_was_fully_validated,
        should_broadcast_execution_output_with, validate_built_payload_linkage,
    };
    use alloy_primitives::B256;
    use alloy_rpc_types_engine::PayloadStatusEnum;
    use std::sync::atomic::AtomicU64;

    #[test]
    fn out_of_order_syncing_child_does_not_suppress_missing_parents() {
        let guard = AtomicU64::new(95);

        // Block 101 reaches new_payload first and returns Syncing. The caller
        // deliberately does not mark that outcome, leaving all missing parents
        // eligible for submission.
        assert_eq!(eager_import_already_validated(&guard, 101), None);
        assert_eq!(eager_import_already_validated(&guard, 98), None);
        assert_eq!(eager_import_already_validated(&guard, 99), None);
        assert_eq!(eager_import_already_validated(&guard, 100), None);

        mark_eager_import_valid(&guard, 98);
        assert_eq!(eager_import_already_validated(&guard, 98), Some(98));
        assert_eq!(eager_import_already_validated(&guard, 99), None);

        mark_eager_import_valid(&guard, 100);
        assert_eq!(eager_import_already_validated(&guard, 99), Some(100));
        assert_eq!(eager_import_already_validated(&guard, 101), None);
    }

    #[test]
    fn normalized_gov5_payload_broadcasts_builder_output_only_when_shared() {
        // An H2-v4 follower executes the normalized payload itself unless the
        // operator opted into shared execution for co-located validators.
        assert!(!should_broadcast_execution_output_with(true, false));
        assert_eq!(
            should_broadcast_execution_output_with(true, true),
            compact_block_enabled()
        );
        assert_eq!(
            should_broadcast_execution_output_with(false, false),
            compact_block_enabled()
        );
    }

    #[test]
    fn only_engine_valid_advances_the_eager_validation_watermark() {
        for non_valid in [
            PayloadStatusEnum::Syncing,
            PayloadStatusEnum::Accepted,
            PayloadStatusEnum::Invalid {
                validation_error: "bad state root".into(),
            },
        ] {
            assert!(
                !payload_was_fully_validated(&non_valid),
                "{non_valid:?} must not mark a block fully validated"
            );

            // Model the caller's state transition: stale/syncing/invalid
            // responses leave the watermark untouched, so missing parents
            // remain eligible for import.
            let guard = AtomicU64::new(41);
            if payload_was_fully_validated(&non_valid) {
                mark_eager_import_valid(&guard, 42);
            }
            assert_eq!(eager_import_already_validated(&guard, 42), None);
        }

        assert!(payload_was_fully_validated(&PayloadStatusEnum::Valid));
        let guard = AtomicU64::new(41);
        if payload_was_fully_validated(&PayloadStatusEnum::Valid) {
            mark_eager_import_valid(&guard, 42);
        }
        assert_eq!(eager_import_already_validated(&guard, 42), Some(42));
    }

    #[test]
    fn built_payload_must_match_locked_parent_and_its_declared_hash() {
        let parent = B256::repeat_byte(0x11);
        let other_parent = B256::repeat_byte(0x12);
        let declared_hash = B256::repeat_byte(0x21);
        let payload_hash = B256::repeat_byte(0x22);

        assert_eq!(
            validate_built_payload_linkage(declared_hash, declared_hash, parent, parent),
            Ok(())
        );
        assert_eq!(
            validate_built_payload_linkage(declared_hash, declared_hash, other_parent, parent),
            Err(BuiltPayloadLinkError::Parent {
                actual: other_parent,
                required: parent,
            }),
            "parent mismatch is rejected before any hash comparison"
        );
        assert_eq!(
            validate_built_payload_linkage(declared_hash, payload_hash, parent, parent),
            Err(BuiltPayloadLinkError::Hash {
                declared: declared_hash,
                payload: payload_hash,
            })
        );
    }
}
