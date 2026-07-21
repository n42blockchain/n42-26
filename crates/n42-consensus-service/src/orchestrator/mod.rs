pub(crate) mod bad_block_cache;
mod consensus_loop;
mod execution_bridge;
pub use execution_bridge::compact_block_enabled;
mod state_mgmt;
mod view_jump_throttle;

use crate::blob_port::BlobStorePort;
use crate::consensus_state::{PoolDepthSnapshot, SharedConsensusState};
use crate::el::ExecutionLayer;
use crate::epoch_schedule::EpochSchedule;
use crate::exec_cache::ExecutionOutputCache;
use crate::net_port::ConsensusNetwork;
use crate::sinks::{StakingSink, StateSink, WithdrawalSource, ZkSink};
use alloy_primitives::{Address, B256};
use metrics::{counter, gauge, histogram};
use n42_consensus::{
    AuthenticatedConsensusMessage, ConsensusEngine, EngineOutput, FUTURE_VIEW_WINDOW, ValidatorSet,
};
use n42_network::{NetworkEvent, PeerId, SyncPayload};
use n42_primitives::QuorumCertificate;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::{debug, error, info, warn};

/// Batched network transaction import payload sent to the tx-pool bridge. Defined
/// locally (identical to `tx_bridge::TxImportBatch`) so the consensus service does
/// not depend on the reth-coupled `tx_bridge` module — both are `Vec<Vec<u8>>`, so
/// the channel ends stay compatible.
pub type TxImportBatch = Vec<Vec<u8>>;

/// zstd magic bytes: all zstd frames start with 0x28B52FFD.
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

/// TX forwarding batches are latency-bounded by a 50ms flush timer, so we can
/// use a larger batch target to reduce cross-task and cross-peer overhead.
const TX_FORWARD_BATCH_TARGET: usize = 512;

/// Maximum number of already-queued R1/R2 votes verified in one randomized
/// BLS multi-pairing. Draining is non-blocking, so this adds no timer latency.
const CONSENSUS_VOTE_BATCH_MAX: usize = 256;

/// Minimum gossip warm-up before leader proposal attempts.
const DEFAULT_LEADER_PROPOSE_WARMUP_MS: u64 = 500;

/// Re-check interval while waiting for validator quorum.
const LEADER_QUORUM_RETRY_MS: u64 = 500;

/// Default interval for re-sending the current view's locally signed R1/R2
/// vote when the collector may have missed the first delivery.
const DEFAULT_VOTE_RESEND_MS: u64 = 2_000;

/// A locally signed vote awaiting evidence that the view progressed. Re-sending
/// the exact bytes is safe because collectors deduplicate by `(view, voter)`.
#[derive(Clone)]
struct PendingVoteResend {
    view: u64,
    target: u32,
    message: n42_primitives::ConsensusMessage,
    next_at: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LeaderBuildWaitMode {
    /// The pending leader build should be executed directly once quorum is reached.
    Direct,
    /// The pending leader build should resume through `schedule_payload_build` once quorum is reached.
    Scheduled,
}

/// Compress payload JSON with zstd (level 3 — good speed/ratio tradeoff).
pub fn compress_payload(json: &[u8]) -> Vec<u8> {
    zstd::bulk::compress(json, 3).unwrap_or_else(|e| {
        tracing::warn!(target: "n42::cl", len = json.len(), error = %e, "zstd compression failed, sending uncompressed");
        json.to_vec()
    })
}

/// Decompress payload if zstd-compressed; pass through raw JSON unchanged.
/// Backward-compatible: old nodes send uncompressed JSON, new nodes send zstd.
pub fn decompress_payload(data: &[u8]) -> std::io::Result<Vec<u8>> {
    if data.len() >= 4 && data[..4] == ZSTD_MAGIC {
        zstd::bulk::decompress(data, 64 * 1024 * 1024) // 64 MB max
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    } else {
        Ok(data.to_vec())
    }
}

fn elapsed_since_unix_ms(start_ms: u64) -> Option<u64> {
    if start_ms == 0 {
        return None;
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis() as u64;
    Some(now_ms.saturating_sub(start_ms))
}

/// Block data broadcast via /n42/blocks/1 GossipSub topic.
///
/// NOTE: Serialized with bincode. Adding new fields requires all nodes to upgrade
/// simultaneously — bincode does not support missing trailing fields on deserialization.
/// New→old is fine (bincode ignores trailing bytes), but old→new will fail.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct BlockDataBroadcast {
    pub block_hash: B256,
    pub view: u64,
    /// Execution payload — zstd-compressed JSON (or raw JSON from older peers).
    /// Use `decompress_payload()` before `serde_json::from_slice()`.
    pub payload_json: Vec<u8>,
    /// Block timestamp (seconds since epoch). Stored directly to avoid JSON re-parsing.
    /// Defaults to 0 for backwards compatibility with older serialized broadcasts.
    #[serde(default)]
    pub timestamp: u64,
    /// Compact Block: zstd-compressed bincode of `CompactBlockExecution`.
    /// When present, followers load this into `payload_cache` to skip EVM re-execution.
    /// None for backwards compatibility with older peers.
    #[serde(default)]
    pub execution_output: Option<Vec<u8>>,
    /// Leader wall-clock timestamp in unix milliseconds when payload became ready
    /// for broadcast. Used only for cross-task timing diagnostics.
    #[serde(default)]
    pub leader_ready_unix_ms: u64,
}

/// Consensus context captured when a leader starts an asynchronous payload build.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PayloadBuildContext {
    view: u64,
    parent_hash: B256,
}

/// A resolved payload together with the context it was built under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PayloadBuildReady {
    context: PayloadBuildContext,
    block_hash: B256,
}

/// Serializable proxy for `(BlockExecutionOutput<Receipt>, Vec<Address>)`.
/// `BlockExecutionResult` from alloy-evm doesn't derive serde, so we flatten it.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct CompactBlockExecution {
    pub bundle_state: revm::database::states::BundleState,
    pub receipts: Vec<reth_ethereum_primitives::Receipt>,
    pub requests: alloy_eips::eip7685::Requests,
    pub gas_used: u64,
    pub blob_gas_used: u64,
    pub senders: Vec<Address>,
}

/// Blob sidecar broadcast via /n42/blobs/1 GossipSub topic.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct BlobSidecarBroadcast {
    pub block_hash: B256,
    pub view: u64,
    pub sidecars: Vec<(B256, Vec<u8>)>,
}

/// Per-block pipeline timing tracker.
///
/// The consensus slot interval acts as a "bus clock": within each tick,
/// build / import / consensus / communication stages run in parallel.
/// This struct records wall-clock timestamps for each stage so we can
/// quantify overlap and identify serialization bottlenecks.
///
/// ```text
/// Slot N clock tick ──────────────────────────────────────── slot_time
/// ├── [Build]           tx_pack + evm_exec + state_root
/// ├── [Broadcast]       block data + blob sidecars
/// ├── [Consensus]       voting rounds (overlaps with import)
/// ├── [Import]          new_payload + fcu (overlaps with consensus)
/// └── [Commit]          finalize
/// ```
#[derive(Debug, Clone)]
pub(crate) struct PipelineTiming {
    /// When we first learned about this block (build trigger or block_data received).
    pub(crate) created: Instant,
    /// Role of this node for the block.
    pub(crate) is_leader: bool,
    /// Leader: payload resolved & broadcast. Follower: block data received.
    pub(crate) build_complete: Option<Instant>,
    /// Eager import (new_payload + fcu) completed.
    pub(crate) import_complete: Option<Instant>,
    /// Consensus committed (Decide received or CommitQC formed).
    pub(crate) committed: Option<Instant>,
}

#[derive(Debug, Default)]
struct TimeoutViewDiag {
    build_start: Option<Instant>,
    build_pool_pending: usize,
    build_pool_queued: usize,
    block_data_received_count: u32,
    first_block_data_received: Option<Instant>,
    timeout_at: Option<Instant>,
    timeout_pool_pending: usize,
    timeout_pool_queued: usize,
}

impl PipelineTiming {
    fn new_follower() -> Self {
        Self {
            created: Instant::now(),
            is_leader: false,
            build_complete: None,
            import_complete: None,
            committed: None,
        }
    }

    /// Produces a one-line summary of pipeline stage durations (ms).
    fn summary(&self) -> String {
        let role = if self.is_leader { "L" } else { "F" };
        let build_ms = self
            .build_complete
            .map(|t| t.duration_since(self.created).as_millis() as u64);
        let import_ms = self
            .import_complete
            .map(|t| t.duration_since(self.created).as_millis() as u64);
        let commit_ms = self
            .committed
            .map(|t| t.duration_since(self.created).as_millis() as u64);
        // Overlap: how much of import happened during consensus (before commit)
        let import_before_commit = match (self.import_complete, self.committed) {
            (Some(imp), Some(com)) if imp <= com => {
                Some(com.duration_since(imp).as_millis() as u64)
            }
            _ => None,
        };
        format!(
            "role={role} build={build}ms import={import}ms commit={commit}ms import_headroom={headroom}ms",
            build = build_ms
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".into()),
            import = import_ms
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".into()),
            commit = commit_ms
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".into()),
            headroom = import_before_commit
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".into()),
        )
    }

    /// Emit per-stage metrics histograms.
    fn emit_metrics(&self) {
        if let Some(t) = self.build_complete {
            histogram!("n42_pipeline_build_ms")
                .record(t.duration_since(self.created).as_millis() as f64);
        }
        if let Some(t) = self.import_complete {
            histogram!("n42_pipeline_import_ms")
                .record(t.duration_since(self.created).as_millis() as f64);
        }
        if let Some(t) = self.committed {
            let total = t.duration_since(self.created).as_millis() as f64;
            histogram!("n42_pipeline_total_ms").record(total);
        }
        // Overlap ratio: what fraction of total time was "useful work" vs waiting
        if let (Some(build), Some(import), Some(commit)) =
            (self.build_complete, self.import_complete, self.committed)
        {
            let build_dur = build.duration_since(self.created).as_millis() as f64;
            let import_dur = import.duration_since(self.created).as_millis() as f64;
            let total = commit.duration_since(self.created).as_millis() as f64;
            if total > 0.0 {
                // Parallelism ratio: (build + import) / total.  >1.0 means good overlap.
                let parallelism = (build_dur + import_dur) / total;
                gauge!("n42_pipeline_parallelism").set(parallelism);
            }
        }
    }
}

/// Deferred finalization: Decide arrived before BlockData (f=0 race).
struct PendingFinalization {
    view: u64,
    block_hash: B256,
    #[allow(dead_code)]
    commit_qc: QuorumCertificate,
}

type FinalizeDone = (u64, B256, QuorumCertificate, bool);
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImportOutcome {
    Valid,
    Syncing,
    Invalid,
}
type ImportDone = (B256, u64, ImportOutcome, u64);
type SidecarDiffWork = (u64, B256, n42_execution::state_diff::StateDiff);

pub struct CommittedBlock {
    pub view: u64,
    pub block_hash: B256,
    pub commit_qc: QuorumCertificate,
    pub payload: Vec<u8>,
    pub validator_changes: Option<Vec<n42_primitives::consensus::ValidatorChange>>,
    /// Full execution lineage from the previously execution-validated head to
    /// this committed block. It may include a prepared ancestor that never
    /// obtained its own CommitQC before a timeout/restart.
    pub execution_lineage: Vec<SyncPayload>,
}

#[derive(Clone)]
struct RecentBlockData {
    view: u64,
    block_hash: B256,
    block_data: Vec<u8>,
}

/// Bridges the consensus engine with the P2P network layer and reth Engine API.
///
/// Runs as a background task via `tokio::select!` over network events, engine outputs,
/// pacemaker timeouts, and BlockReady signals.
pub struct ConsensusService {
    engine: ConsensusEngine,
    /// Network port (Caplin sentinel-client seam). One in-process adapter today
    /// (`NetworkHandle`); the trait object lets the orchestrator move into a
    /// service crate without depending on `n42-network` / libp2p internals.
    network: Arc<dyn ConsensusNetwork>,
    /// High-priority: consensus messages only (Vote, Proposal, PrepareQC, etc.)
    consensus_event_rx: Option<mpsc::Receiver<NetworkEvent>>,
    /// Lower-priority: data events (BlockData, TX, Sync, Peers)
    net_event_rx: mpsc::Receiver<NetworkEvent>,
    output_rx: mpsc::Receiver<EngineOutput>,
    /// The execution-layer seam (Caplin-style `ExecutionEngine`). `None` for
    /// tests / EL-less observer paths. One in-process adapter
    /// (`RethExecutionLayer`) wraps the reth engine + payload-builder handles.
    el: Option<Arc<dyn ExecutionLayer>>,
    consensus_state: Option<Arc<SharedConsensusState>>,
    /// Most recent locally execution-validated committed head. This is deliberately
    /// distinct from `committed_block_count`: HotStuff agreement may advance before
    /// reth has accepted the corresponding payload.
    head_block_hash: B256,
    /// Highest committed/sync view whose block reth confirmed as `Valid`.
    /// Guards async finalize/import completions from regressing the executed head.
    execution_validated_head_view: u64,
    /// Bounded record of speculative eager imports that reth already accepted.
    /// A matching commit may promote the block without waiting for a second EL result.
    eager_execution_validated: VecDeque<B256>,
    /// State-tree sidecar diffs staged at commit time, applied only once
    /// execution is CONFIRMED (advance_execution_validated_head) - never on a
    /// block reth could still reject.
    pending_sidecar_diffs: BTreeMap<u64, (B256, n42_execution::state_diff::StateDiff)>,
    /// Exact block hash reth confirmed for each staged sidecar view. A diff is
    /// never flushed merely because a later view advanced; its staged hash
    /// must match this execution-valid canonical binding first.
    execution_validated_sidecar_hashes: BTreeMap<u64, B256>,
    /// Committed views whose compact execution output was unavailable or
    /// malformed when the block committed. A gap is an ordering barrier:
    /// later diffs remain staged until late BlockData recovers the missing one.
    missing_sidecar_diffs: BTreeMap<u64, B256>,
    /// Confirmed sidecar diffs waiting for the single FIFO background worker.
    /// A shared queue is required because separate spawn_blocking calls can
    /// acquire the StateSink lock out of commit order.
    sidecar_apply_queue: Arc<std::sync::Mutex<VecDeque<SidecarDiffWork>>>,
    sidecar_apply_worker_running: Arc<std::sync::atomic::AtomicBool>,
    /// Highest view accepted by the sidecar worker, retained across worker
    /// lifetimes so a later queue cannot replay or regress an already-applied
    /// append-ordered diff.
    last_sidecar_applied_view: Arc<std::sync::atomic::AtomicU64>,
    last_commit_qc: Option<QuorumCertificate>,
    /// Cached `keccak256(commit_qc.aggregate_signature)` — avoids re-hashing on every payload build.
    prev_randao_cache: B256,
    block_ready_tx: mpsc::Sender<PayloadBuildReady>,
    block_ready_rx: mpsc::Receiver<PayloadBuildReady>,
    fee_recipient: Address,
    slot_time: Duration,
    next_build_at: Option<Instant>,
    next_slot_timestamp: Option<u64>,
    consecutive_empty_skips: u32,
    pending_block_data: BTreeMap<B256, Vec<u8>>,
    /// Raw proposal payloads retained across ordinary pending-cache clears so
    /// CommitQC descendants can prove and serve any prepared execution
    /// ancestors needed by a restarting reth tree.
    recent_block_data: VecDeque<RecentBlockData>,
    pending_executions: HashSet<B256>,
    pending_finalization: Option<PendingFinalization>,
    /// Blocks that returned `Syncing` from new_payload, queued for retry.
    /// Each entry is `(serialized_data, retry_count)` — dropped after 3 retries.
    syncing_blocks: VecDeque<(Vec<u8>, u32)>,
    /// Deterministic `new_payload(INVALID)` verdicts, bounded by an LRU policy.
    bad_blocks: bad_block_cache::BadBlockCache,
    tx_import_tx: Option<mpsc::Sender<TxImportBatch>>,
    tx_broadcast_rx: Option<mpsc::Receiver<Vec<u8>>>,
    committed_blocks: VecDeque<CommittedBlock>,
    connected_peers: HashSet<PeerId>,
    /// Pending leader-proposal quorum gate for a specific view.
    ///
    /// `LeaderBuildWaitMode::Direct` means the gate will call
    /// `do_trigger_payload_build` directly once quorum is reached.
    /// `LeaderBuildWaitMode::Scheduled` means the gate re-enters
    /// `schedule_payload_build` once quorum is reached.
    leader_build_waiting_view: Option<(u64, LeaderBuildWaitMode)>,
    /// Earliest instant at which the startup leader may propose for the
    /// associated view. Peer-connect events can satisfy the quorum gate before
    /// the configured warmup floor, so the floor must be tracked separately
    /// from the recheck timer.
    leader_build_not_before: Option<(u64, Instant)>,
    sync_in_flight: bool,
    sync_started_at: Option<Instant>,
    /// `[from_view, to_view]` of the most recent in-flight sync / catch-up
    /// request. Sync responses are validated against this range so a peer
    /// cannot inject blocks we never asked for (T2a). Cleared together with
    /// `sync_requested_peers` when that request group completes.
    sync_request_range: Option<(u64, u64)>,
    /// Peers to which the active sync range was actually sent. Responses from
    /// any other peer are unsolicited and must not clear or mutate active sync
    /// state. Catch-up fan-out keeps every successful recipient here until one
    /// returns useful data or all recipients finish.
    sync_requested_peers: HashSet<PeerId>,
    /// Round-robin cursor for recovery requests. The execution-validated view
    /// can remain pinned while consensus advances, so keying peer choice only
    /// on that view would retry the same possibly incomplete peer forever.
    sync_peer_cursor: usize,
    state_file: Option<PathBuf>,
    validator_set_for_sync: Option<ValidatorSet>,
    /// View at which a validator_changes block was applied during sync.
    /// Guards the epoch-advance check in the sync loop: we only fire advance_epoch
    /// at a boundary if the staging came from a block we actually replayed in sync
    /// (not from pre-existing live-consensus state), preventing premature advances.
    epoch_sync_staged_view: Option<u64>,
    mobile_packet_tx: Option<mpsc::Sender<(B256, u64)>>,
    leader_payload_rx: mpsc::Receiver<(B256, Vec<u8>)>,
    leader_payload_tx: mpsc::Sender<(B256, Vec<u8>)>,
    /// Receives notifications when a background `import_and_notify` task completes.
    /// Tuple: (block_hash, view, execution outcome, block timestamp).
    import_done_rx: mpsc::Receiver<ImportDone>,
    import_done_tx: mpsc::Sender<ImportDone>,
    /// Receives finalize-FCU completion from the optional async finalize path.
    /// Tuple: (view, block_hash, commit_qc, finalized).
    finalize_done_rx: mpsc::Receiver<FinalizeDone>,
    finalize_done_tx: mpsc::Sender<FinalizeDone>,
    blob_store: Option<Arc<dyn BlobStorePort>>,
    /// Compact-block execution-output cache behind the [`ExecutionOutputCache`]
    /// port. `None` ⇒ compact-block inject/take are no-ops (tests / EL-less).
    exec_output_cache: Option<Arc<dyn ExecutionOutputCache>>,
    /// True while a background import task is running.
    bg_import_in_flight: bool,
    /// Hashes already queued or in-flight for background import.
    /// Prevents duplicate Case B work for the same block.
    bg_import_hashes: HashSet<B256>,
    /// Queue of pending imports waiting for the current bg import to finish.
    /// Entries: (serialized_block_data, block_hash, view).
    bg_import_queue: VecDeque<(Vec<u8>, B256, u64)>,
    /// Speculative build: eager import tasks notify when a block is imported to reth.
    /// Allows the next leader to start building before consensus commits the current block.
    eager_import_done_tx: mpsc::Sender<(B256, u64)>,
    eager_import_done_rx: mpsc::Receiver<(B256, u64)>,
    /// Hash of the block whose speculative build is in progress.
    /// Set when build is triggered by eager import (before consensus commit).
    /// Cleared on ViewChanged or when finalize confirms the block.
    speculative_build_hash: Option<B256>,
    /// View and parent hash for which a payload build task is currently running.
    /// Prevents duplicate builds based on the same parent, which produce different
    /// blocks at the same height and overwhelm reth with conflicting `new_payload` calls
    /// — triggering pipeline sync and chain stalls.
    building_on_parent: Option<PayloadBuildContext>,
    /// When the current payload build was triggered. Used to measure actual build duration
    /// in PipelineTiming (created → build_complete). Reset when build finishes or is cancelled.
    build_triggered_at: Option<Instant>,
    /// Notifies the orchestrator when a payload resolve task finishes (success or failure).
    /// Used to clear `building_on_parent` so new builds are not permanently blocked
    /// when a resolve task takes longer than the pacemaker timeout.
    build_complete_tx: mpsc::Sender<PayloadBuildContext>,
    build_complete_rx: mpsc::Receiver<PayloadBuildContext>,
    /// Staking scan/persist behind the [`StakingSink`] port.
    staking_sink: Option<Arc<dyn StakingSink>>,
    /// Reward + matured-stake withdrawal computation behind the
    /// [`WithdrawalSource`] port (build-path query).
    withdrawal_source: Option<Arc<dyn WithdrawalSource>>,
    committed_block_count: u64,
    /// Tracks the timestamp of the last built/committed block to prevent
    /// "invalid timestamp" errors from the Engine API.  The Engine API requires
    /// `new_payload_attributes.timestamp > head_block.timestamp` (strictly greater).
    /// Without this guard, fast block production (slot_time=0 or f=0 single-node)
    /// can produce two blocks within the same wall-clock second, violating the rule.
    last_committed_timestamp: u64,
    /// Timestamp when the current view started (recorded on ViewChanged).
    /// Used to measure commit latency: time from view start to BlockCommitted.
    view_started_at: Option<tokio::time::Instant>,
    /// Epoch schedule loaded from `epoch_schedule.json`.
    /// Used to pre-stage the next epoch's validator set at each epoch transition.
    epoch_schedule: Option<EpochSchedule>,
    /// Buffer for batching tx forwards to the current leader.
    tx_forward_buffer: Vec<Vec<u8>>,
    /// Last R1/R2 vote sent in the current view. If direct delivery was lost and
    /// the view has not advanced, retry it periodically to the collector.
    pending_vote_resend: Option<PendingVoteResend>,
    /// Per-block pipeline timing tracker. Populated incrementally as events flow
    /// through the orchestrator. Logged and emitted as metrics at commit time.
    /// Bounded to 32 entries; older entries are evicted.
    pipeline_timings: HashMap<B256, PipelineTiming>,
    /// Last local consensus commit wall-clock. Used for inter-block cadence metrics.
    last_commit_instant: Option<Instant>,
    last_commit_view: Option<u64>,
    last_commit_hash: Option<B256>,
    /// Observation-only high-TPS timeout diagnostics keyed by consensus view.
    timeout_view_diags: BTreeMap<u64, TimeoutViewDiag>,
    /// Last committed parent for which commit->build_start has already been recorded.
    commit_to_build_recorded_parent: Option<B256>,
    /// Guards follower eager import: tracks the last block number sent to reth
    /// via new_payload. Prevents multiple eager imports for the same block number
    /// with different hashes, which triggers reth pipeline sync and chain stalls.
    eager_import_block_guard: Arc<std::sync::atomic::AtomicU64>,
    /// Fast propose mode: skip slot boundary alignment, build immediately after
    /// ViewChanged/BlockCommitted. Consensus voting naturally paces block production.
    /// Enabled by `N42_FAST_PROPOSE=1`. Default: false (grid-aligned slots).
    fast_propose: bool,
    /// Moves finalize forkchoiceUpdated off the consensus select-loop hot path.
    /// Enabled by `N42_ASYNC_FINALIZE_FCU=1`. Default: false.
    async_finalize_fcu: bool,
    /// Deferred state-root mode (env `N42_DEFER_STATE_ROOT`, read once by the node
    /// at construction). When set, finalized blocks log a deferred-state-root note.
    /// Cached as a `bool` so the orchestrator no longer calls the reth EVM helper.
    defer_state_root: bool,
    /// Sparse Binary Merkle Tree for parallel state commitment, behind the
    /// [`StateSink`] port. Updated asynchronously after each committed block.
    jmt: Option<Arc<dyn StateSink>>,
    /// Twig state tree for append-only state commitments, behind the
    /// [`StateSink`] port. Updated asynchronously after each committed block.
    twig: Option<Arc<dyn StateSink>>,
    /// ZK proof scheduler behind the [`ZkSink`] port: generates ZK proofs
    /// asynchronously as a sidecar. Enabled by `N42_ZK_PROOF=1`.
    zk_scheduler: Option<Arc<dyn ZkSink>>,
    /// Whether deterministic validator PeerIds may be derived when no explicit
    /// `p2p_peer_id` bindings are configured.
    allow_deterministic_validator_peers: bool,
    /// Admin command receiver for RPC-originated validator reconfig requests.
    admin_rx: Option<mpsc::Receiver<crate::consensus_state::AdminCommand>>,
    /// MDBX-backed store for per-block consensus evidence (QC + mobile attestation).
    /// Shares the JMT MDBX environment. Written after each committed block.
    evidence_store: Option<Arc<n42_jmt::EvidenceStore>>,
    /// Per-peer rate limiter for messages that would otherwise force the engine
    /// into the BLS-heavy QC view-jump path. See `view_jump_throttle.rs`.
    view_jump_throttle: view_jump_throttle::ViewJumpThrottle,
}

impl ConsensusService {
    fn vote_resend_delay() -> Duration {
        Duration::from_millis(
            std::env::var("N42_VOTE_RESEND_MS")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(DEFAULT_VOTE_RESEND_MS)
                .max(100),
        )
    }

    fn resend_pending_vote(&mut self) {
        let Some(pending) = self.pending_vote_resend.clone() else {
            return;
        };

        let current_view = self.engine.current_view();
        if pending.view != current_view {
            self.pending_vote_resend = None;
            return;
        }

        let mut delivered_directly = false;
        if let Some(peer_id) = self.network.validator_peer(pending.target) {
            match self.network.send_direct(peer_id, pending.message.clone()) {
                Ok(()) => delivered_directly = true,
                Err(error) => warn!(
                    target: "n42::cl::orchestrator",
                    view = pending.view,
                    target = pending.target,
                    %error,
                    "direct vote resend failed; falling back to gossip"
                ),
            }
        }

        if !delivered_directly && let Err(error) = self.network.broadcast_consensus(pending.message)
        {
            warn!(
                target: "n42::cl::orchestrator",
                view = pending.view,
                target = pending.target,
                %error,
                "fallback gossip vote resend failed"
            );
        }

        counter!("n42_consensus_vote_resends_total", "delivery" => if delivered_directly { "direct" } else { "gossip" })
            .increment(1);
        if let Some(next) = self.pending_vote_resend.as_mut() {
            next.next_at = Instant::now() + Self::vote_resend_delay();
        }
    }

    fn async_finalize_fcu_enabled() -> bool {
        std::env::var("N42_ASYNC_FINALIZE_FCU")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0)
            > 0
    }

    fn cache_pending_block_data(
        &mut self,
        hash: B256,
        data: Vec<u8>,
        protected_hashes: &[B256],
    ) -> bool {
        // A Decide-before-BlockData race may have left the sidecar paused at
        // this committed view. Recover the owned StateDiff directly from the
        // late bytes even if the bounded raw-data cache cannot retain them.
        self.try_recover_missing_sidecar_diff(hash, &data);
        self.remember_recent_block_data(hash, &data);
        if self.pending_block_data.contains_key(&hash) {
            return false;
        }

        if self.pending_block_data.len() >= execution_bridge::MAX_PENDING_BLOCK_DATA {
            let evict_key = self
                .pending_block_data
                .keys()
                .find(|candidate| !protected_hashes.contains(*candidate))
                .copied();

            let Some(evict_key) = evict_key else {
                counter!("n42_pending_block_data_full_protected_drop_total").increment(1);
                warn!(
                    target: "n42::cl::orchestrator",
                    %hash,
                    protected = protected_hashes.len(),
                    "pending block data cache full with no evictable entry, dropping block data"
                );
                return false;
            };
            self.pending_block_data.remove(&evict_key);
        }

        self.pending_block_data.insert(hash, data);
        true
    }

    fn remember_recent_block_data(&mut self, hash: B256, data: &[u8]) {
        if self
            .recent_block_data
            .iter()
            .any(|entry| entry.block_hash == hash)
        {
            return;
        }
        let Ok(broadcast) = bincode::deserialize::<BlockDataBroadcast>(data) else {
            return;
        };
        if broadcast.block_hash != hash {
            return;
        }
        if self.recent_block_data.len() >= execution_bridge::MAX_PENDING_BLOCK_DATA {
            self.recent_block_data.pop_front();
        }
        self.recent_block_data.push_back(RecentBlockData {
            view: broadcast.view,
            block_hash: hash,
            block_data: data.to_vec(),
        });
    }

    fn execution_parent_and_number(data: &[u8]) -> Option<(B256, u64)> {
        let broadcast = bincode::deserialize::<BlockDataBroadcast>(data).ok()?;
        let payload = decompress_payload(&broadcast.payload_json).ok()?;
        let execution: alloy_rpc_types_engine::ExecutionData =
            serde_json::from_slice(&payload).ok()?;
        (execution.block_hash() == broadcast.block_hash)
            .then(|| (execution.parent_hash(), execution.block_number()))
    }

    fn execution_parent_and_number_from_payload(payload_json: &[u8]) -> Option<(B256, u64)> {
        let payload = decompress_payload(payload_json).ok()?;
        let execution: alloy_rpc_types_engine::ExecutionData =
            serde_json::from_slice(&payload).ok()?;
        Some((execution.parent_hash(), execution.block_number()))
    }

    /// Returns the raw execution chain from the current validated head through
    /// `block_hash`. An empty result means the bounded proposal cache cannot
    /// prove a complete parent-linked lineage and callers must stay conservative.
    fn recent_execution_lineage(&self, block_hash: B256) -> Vec<RecentBlockData> {
        let mut current = block_hash;
        let mut reversed = Vec::new();
        let mut seen = HashSet::new();

        while current != self.head_block_hash
            && reversed.len() < execution_bridge::MAX_PENDING_BLOCK_DATA
        {
            if !seen.insert(current) {
                return Vec::new();
            }
            let Some(entry) = self
                .recent_block_data
                .iter()
                .find(|entry| entry.block_hash == current)
                .cloned()
            else {
                return Vec::new();
            };
            let Some((parent, _)) = Self::execution_parent_and_number(&entry.block_data) else {
                return Vec::new();
            };
            reversed.push(entry);
            current = parent;
        }

        if current != self.head_block_hash {
            return Vec::new();
        }
        reversed.reverse();
        reversed
    }

    fn try_recover_missing_sidecar_diff(&mut self, hash: B256, data: &[u8]) {
        if self.jmt.is_none() && self.twig.is_none() {
            return;
        }
        let Some(view) = self
            .missing_sidecar_diffs
            .iter()
            .find_map(|(view, missing_hash)| (*missing_hash == hash).then_some(*view))
        else {
            return;
        };
        let Some(diff) = Self::extract_state_diff_for_state_tree(hash, data) else {
            return;
        };

        self.missing_sidecar_diffs.remove(&view);
        self.pending_sidecar_diffs.insert(view, (hash, diff));
        counter!("n42_sidecar_diff_gap_recovered_total").increment(1);
        info!(
            target: "n42::twig",
            view,
            %hash,
            "recovered missing committed sidecar diff from late block data"
        );
        if view <= self.execution_validated_head_view {
            self.enqueue_confirmed_sidecar_state_diffs(self.execution_validated_head_view);
        }
    }

    /// Drop a staged diff whose corresponding payload failed execution
    /// validation. Keeping peer-supplied bytes after a non-`Valid` result could
    /// let a later honest payload with the same declared hash flush the forged
    /// diff into QMDB/Twig.
    fn discard_unvalidated_sidecar_diff(&mut self, view: u64, hash: B256) {
        let matches = self
            .pending_sidecar_diffs
            .get(&view)
            .is_some_and(|(staged_hash, _)| *staged_hash == hash);
        let removed = matches && self.pending_sidecar_diffs.remove(&view).is_some();
        if removed {
            self.missing_sidecar_diffs.insert(view, hash);
            counter!("n42_sidecar_unvalidated_diffs_discarded_total").increment(1);
            warn!(
                target: "n42::twig",
                view,
                %hash,
                "discarded sidecar diff after payload failed execution validation"
            );
        }
    }

    fn leader_propose_warmup_delay() -> Duration {
        std::env::var("N42_STARTUP_DELAY_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|v| *v > 0)
            .map(Duration::from_millis)
            .unwrap_or_else(|| Duration::from_millis(DEFAULT_LEADER_PROPOSE_WARMUP_MS))
    }

    fn leader_quorum_recheck_delay() -> Duration {
        Duration::from_millis(LEADER_QUORUM_RETRY_MS)
    }

    fn connected_validator_peer_count(&self) -> usize {
        let connected_peers: HashSet<_> = self.connected_peers.iter().copied().collect();
        self.network
            .all_validator_peers()
            .into_iter()
            .map(|(_, peer_id)| peer_id)
            .filter(|peer_id| connected_peers.contains(peer_id))
            .count()
    }

    fn quorum_peers_needed(&self) -> usize {
        // The leader contributes its own vote, so it needs n-f-1 connected
        // validator peers before building. This also makes f=0 multi-validator
        // sets wait for every other validator.
        self.engine.quorum_size().saturating_sub(1)
    }

    fn has_quorum_peers(&self) -> bool {
        self.connected_validator_peer_count() >= self.quorum_peers_needed()
    }

    fn pending_leader_build_mode_for_current_view(&self) -> Option<LeaderBuildWaitMode> {
        let current_view = self.engine.current_view();
        self.leader_build_waiting_view
            .and_then(|(view, mode)| (view == current_view).then_some(mode))
    }

    fn begin_leader_build_wait(&mut self, mode: LeaderBuildWaitMode, slot_timestamp: Option<u64>) {
        self.leader_build_waiting_view = Some((self.engine.current_view(), mode));
        self.next_slot_timestamp = slot_timestamp;
    }

    fn clear_leader_build_wait(&mut self) {
        self.leader_build_waiting_view = None;
        self.leader_build_not_before = None;
    }

    fn schedule_leader_build_recheck(&mut self, delay: Duration) {
        self.next_build_at = Some(Instant::now() + delay);
    }

    fn arm_leader_build_timer_as_direct_wait(&mut self, slot_timestamp: Option<u64>) {
        if self.pending_leader_build_mode_for_current_view().is_none() {
            self.begin_leader_build_wait(LeaderBuildWaitMode::Direct, slot_timestamp);
        } else if let Some(slot_timestamp) = slot_timestamp {
            self.next_slot_timestamp = Some(slot_timestamp);
        }
    }

    async fn evaluate_leader_build_wait(&mut self, slot_timestamp: Option<u64>) {
        let Some(mode) = self.pending_leader_build_mode_for_current_view() else {
            return;
        };

        if !self.engine.is_current_leader() {
            self.clear_leader_build_wait();
            return;
        }

        let current_view = self.engine.current_view();
        if let Some((floor_view, not_before)) = self.leader_build_not_before {
            if floor_view != current_view {
                self.leader_build_not_before = None;
            } else if let Some(remaining) = not_before.checked_duration_since(Instant::now()) {
                debug!(
                    target: "n42::cl::orchestrator",
                    view = current_view,
                    remaining_ms = remaining.as_millis() as u64,
                    "leader build waiting for warmup floor"
                );
                self.schedule_leader_build_recheck(remaining);
                return;
            } else {
                self.leader_build_not_before = None;
            }
        }

        if let Some(slot_timestamp) = slot_timestamp {
            self.next_slot_timestamp = Some(slot_timestamp);
        }

        let connected_validator_peers = self.connected_validator_peer_count();
        let needed_quorum_peers = self.quorum_peers_needed();
        let has_quorum_peers = self.has_quorum_peers();
        debug_assert_eq!(
            has_quorum_peers,
            connected_validator_peers >= needed_quorum_peers
        );
        if !has_quorum_peers {
            debug!(
                target: "n42::cl::orchestrator",
                view = self.engine.current_view(),
                ?mode,
                connected_validator_peers,
                needed_quorum_peers,
                "leader build waiting for validator peer quorum"
            );
            self.schedule_leader_build_recheck(Self::leader_quorum_recheck_delay());
            return;
        }

        info!(
            target: "n42::cl::orchestrator",
            view = self.engine.current_view(),
            ?mode,
            connected_validator_peers,
            needed_quorum_peers,
            "validator peer quorum reached for leader build"
        );

        self.clear_leader_build_wait();
        match mode {
            LeaderBuildWaitMode::Direct => {
                let slot_timestamp = self.next_slot_timestamp.take();
                self.do_trigger_payload_build(slot_timestamp).await;
            }
            LeaderBuildWaitMode::Scheduled => {
                self.schedule_payload_build().await;
            }
        }
    }

    /// Accepts a resolved payload only under the LockedQC parent/view captured
    /// when its build began. A view change must never wrap an old child of B in
    /// a new Proposal justified by LockedQC(A).
    fn handle_payload_build_ready(&mut self, ready: PayloadBuildReady) -> bool {
        let current_context = self.required_payload_build_context();
        let owns_guard = self.building_on_parent == Some(ready.context);
        if owns_guard {
            self.building_on_parent = None;
        }

        if current_context != Some(ready.context) {
            warn!(
                target: "n42::cl::orchestrator",
                built_view = ready.context.view,
                built_parent = %ready.context.parent_hash,
                current_view = self.engine.current_view(),
                required_parent = ?current_context.map(|context| context.parent_hash),
                block_hash = %ready.block_hash,
                "discarding payload resolved for stale view or non-LockedQC parent"
            );
            counter!("n42_stale_payload_builds_total").increment(1);
            if owns_guard {
                self.build_triggered_at = None;
            }
            if self.building_on_parent.is_none() {
                self.schedule_build_retry();
            }
            return false;
        }

        let now = Instant::now();
        let timing = PipelineTiming {
            created: self.build_triggered_at.take().unwrap_or(now),
            is_leader: true,
            build_complete: Some(now),
            import_complete: None,
            committed: None,
        };
        self.record_pipeline_timing(ready.block_hash, timing);

        info!(target: "n42::cl::orchestrator", hash = %ready.block_hash,
            view = ready.context.view, parent = %ready.context.parent_hash,
            "payload built on LockedQC parent, feeding BlockReady to consensus");
        if let Err(error) = self
            .engine
            .process_event(n42_consensus::ConsensusEvent::BlockReady(
                ready.block_hash,
                None,
            ))
        {
            error!(target: "n42::cl::orchestrator", %error, "error processing BlockReady event");
            return false;
        }
        true
    }

    /// Clears only the guard owned by the task that completed. An older
    /// resolve task may finish after a new-view build has already started.
    fn handle_payload_build_complete(&mut self, completed: PayloadBuildContext) -> bool {
        if self.building_on_parent != Some(completed) {
            debug!(target: "n42::cl::orchestrator", ?completed,
                active = ?self.building_on_parent,
                "ignoring stale payload-build completion");
            return false;
        }

        debug!(target: "n42::cl::orchestrator", ?completed,
            "build task completed, clearing its building_on_parent guard");
        self.building_on_parent = None;
        self.build_triggered_at = None;
        true
    }

    fn drain_leader_payload_rx(&mut self, protected_hashes: &[B256]) {
        while let Ok((hash, data)) = self.leader_payload_rx.try_recv() {
            let decoded = bincode::deserialize::<BlockDataBroadcast>(&data).ok();
            self.cache_pending_block_data(hash, data, protected_hashes);
            let execution_lineage = self
                .recent_execution_lineage(hash)
                .into_iter()
                .map(|entry| SyncPayload {
                    view: entry.view,
                    block_hash: entry.block_hash,
                    block_data: entry.block_data,
                })
                .collect::<Vec<_>>();
            if let Some(broadcast) = decoded {
                if broadcast.timestamp > 0 {
                    self.last_committed_timestamp =
                        self.last_committed_timestamp.max(broadcast.timestamp);
                }
                if let Some(block) = self
                    .committed_blocks
                    .iter_mut()
                    .rev()
                    .find(|block| block.block_hash == hash)
                {
                    if block.payload.is_empty() {
                        block.payload = broadcast.payload_json;
                        debug!(
                            target: "n42::cl::consensus_loop",
                            %hash,
                            "backfilled committed payload while draining leader feedback"
                        );
                    }
                    if block.execution_lineage.is_empty() && execution_lineage.len() > 1 {
                        block.execution_lineage = execution_lineage;
                    }
                }
            }
        }
    }

    fn diag_leader_index_for_view(&self, view: u64) -> u32 {
        if view == self.engine.current_view() {
            return self.engine.current_leader_index();
        }
        let validator_count = self.engine.validator_count().max(1) as u64;
        (view % validator_count) as u32
    }

    fn timeout_diag_entry(&mut self, view: u64) -> &mut TimeoutViewDiag {
        const MAX_TIMEOUT_DIAG_VIEWS: usize = 256;
        while self.timeout_view_diags.len() >= MAX_TIMEOUT_DIAG_VIEWS {
            let Some(oldest) = self.timeout_view_diags.keys().next().copied() else {
                break;
            };
            self.timeout_view_diags.remove(&oldest);
        }
        self.timeout_view_diags.entry(view).or_default()
    }

    fn record_timeout_diag_build_start(
        &mut self,
        view: u64,
        parent: B256,
        pool_pending: usize,
        pool_queued: usize,
        build_start: Instant,
    ) {
        let leader_idx = self.diag_leader_index_for_view(view);
        let my_index = self.engine.my_index();
        let prev_view = view.saturating_sub(1);
        let prev_timeout_to_build_start_ms = self
            .timeout_view_diags
            .get(&prev_view)
            .and_then(|diag| diag.timeout_at)
            .map(|timeout_at| {
                build_start
                    .saturating_duration_since(timeout_at)
                    .as_millis() as u64
            });
        let prev_view_had_timeout = prev_timeout_to_build_start_ms.is_some();

        let diag = self.timeout_diag_entry(view);
        diag.build_start = Some(build_start);
        diag.build_pool_pending = pool_pending;
        diag.build_pool_queued = pool_queued;

        info!(
            target: "n42::cl::timeout_diag",
            view,
            leader_idx,
            my_index,
            %parent,
            pool_pending,
            pool_queued,
            prev_view,
            prev_view_had_timeout,
            prev_timeout_to_build_start_ms = prev_timeout_to_build_start_ms.unwrap_or(0),
            "N42_TIMEOUT_VIEW: leader_build_start"
        );
    }

    fn record_timeout_diag_block_data_received(
        &mut self,
        view: u64,
        hash: B256,
        bytes: usize,
        leader_ready_unix_ms: u64,
        duplicate: bool,
    ) {
        let now = Instant::now();
        let current_view = self.engine.current_view();
        let leader_idx = self.diag_leader_index_for_view(view);
        let my_index = self.engine.my_index();
        let since_leader_ready_ms = elapsed_since_unix_ms(leader_ready_unix_ms).unwrap_or(0);

        let diag = self.timeout_diag_entry(view);
        diag.block_data_received_count = diag.block_data_received_count.saturating_add(1);
        if diag.first_block_data_received.is_none() {
            diag.first_block_data_received = Some(now);
        }
        let received_after_timeout_ms = diag
            .timeout_at
            .map(|timeout_at| now.saturating_duration_since(timeout_at).as_millis() as u64);
        let timeout_seen = received_after_timeout_ms.is_some();

        info!(
            target: "n42::cl::timeout_diag",
            view,
            current_view,
            leader_idx,
            my_index,
            %hash,
            bytes,
            duplicate,
            block_data_received_count = diag.block_data_received_count,
            has_leader_ready_ts = leader_ready_unix_ms > 0,
            since_leader_ready_ms,
            timeout_seen,
            received_after_timeout_ms = received_after_timeout_ms.unwrap_or(0),
            "N42_TIMEOUT_VIEW: block_data_received"
        );
    }

    fn record_timeout_diag_timeout_triggered(&mut self, view: u64) {
        let now = Instant::now();
        let pool_depth = self.pool_depth_snapshot();
        let leader_idx = self.engine.current_leader_index();
        let my_index = self.engine.my_index();
        let elapsed_since_last_commit_ms = self
            .last_commit_instant
            .map(|last_commit| now.saturating_duration_since(last_commit).as_millis() as u64);
        let view_elapsed_ms = self
            .view_started_at
            .map(|started| started.elapsed().as_millis() as u64);

        let building_on_parent = self.building_on_parent;
        let next_build_scheduled = self.next_build_at.is_some();
        let speculative_build_hash = self.speculative_build_hash;
        let bg_import_in_flight = self.bg_import_in_flight;
        let bg_import_queue_len = self.bg_import_queue.len();
        let pending_block_data_len = self.pending_block_data.len();
        let pending_executions_len = self.pending_executions.len();
        let phase = format!("{:?}", self.engine.current_phase());
        let is_leader = self.engine.is_current_leader();
        let last_commit_view = self.last_commit_view.unwrap_or_default();
        let last_commit_hash = self.last_commit_hash;

        let diag = self.timeout_diag_entry(view);
        let build_started = diag.build_start.is_some();
        let build_start_age_ms = diag
            .build_start
            .map(|build_start| now.saturating_duration_since(build_start).as_millis() as u64);
        let block_data_received_count = diag.block_data_received_count;
        let block_data_before_timeout = diag.first_block_data_received.is_some();
        diag.timeout_at.get_or_insert(now);
        diag.timeout_pool_pending = pool_depth.pending;
        diag.timeout_pool_queued = pool_depth.queued;

        info!(
            target: "n42::cl::timeout_diag",
            view,
            leader_idx,
            my_index,
            is_leader,
            phase = %phase,
            pool_pending = pool_depth.pending,
            pool_queued = pool_depth.queued,
            last_commit_view,
            last_commit_hash = ?last_commit_hash,
            elapsed_since_last_commit_ms = elapsed_since_last_commit_ms.unwrap_or(0),
            has_last_commit = elapsed_since_last_commit_ms.is_some(),
            view_elapsed_ms = view_elapsed_ms.unwrap_or(0),
            has_view_start = view_elapsed_ms.is_some(),
            build_started,
            build_start_age_ms = build_start_age_ms.unwrap_or(0),
            block_data_received_count,
            block_data_before_timeout,
            pending_block_data_len,
            pending_executions_len,
            building_on_parent = ?building_on_parent,
            next_build_scheduled,
            speculative_build_hash = ?speculative_build_hash,
            bg_import_in_flight,
            bg_import_queue_len,
            "N42_TIMEOUT_VIEW: timeout_triggered"
        );
    }

    fn record_timeout_diag_view_changed(&mut self, new_view: u64) {
        let now = Instant::now();
        let timed_out_view = new_view.saturating_sub(1);
        let Some(diag) = self.timeout_view_diags.get(&timed_out_view) else {
            return;
        };
        let Some(timeout_at) = diag.timeout_at else {
            return;
        };
        let pool_depth = self.pool_depth_snapshot();
        let timeout_to_view_change_ms =
            now.saturating_duration_since(timeout_at).as_millis() as u64;
        let prev_leader_idx = self.diag_leader_index_for_view(timed_out_view);
        let next_leader_idx = self.diag_leader_index_for_view(new_view);

        info!(
            target: "n42::cl::timeout_diag",
            timed_out_view,
            new_view,
            prev_leader_idx,
            next_leader_idx,
            my_index = self.engine.my_index(),
            timeout_to_view_change_ms,
            timeout_pool_pending = diag.timeout_pool_pending,
            timeout_pool_queued = diag.timeout_pool_queued,
            pool_pending = pool_depth.pending,
            pool_queued = pool_depth.queued,
            build_started = diag.build_start.is_some(),
            block_data_received_count = diag.block_data_received_count,
            "N42_TIMEOUT_VIEW: view_change_after_timeout"
        );
    }

    pub fn new(
        engine: ConsensusEngine,
        network: Arc<dyn ConsensusNetwork>,
        net_event_rx: mpsc::Receiver<NetworkEvent>,
        output_rx: mpsc::Receiver<EngineOutput>,
    ) -> Self {
        // Tests use this constructor; create a dummy consensus channel.
        let (block_ready_tx, block_ready_rx) = mpsc::channel(256);
        let (leader_payload_tx, leader_payload_rx) = mpsc::channel(64);
        let (import_done_tx, import_done_rx) = mpsc::channel(256);
        let (finalize_done_tx, finalize_done_rx) = mpsc::channel(256);
        let (eager_import_done_tx, eager_import_done_rx) = mpsc::channel(256);
        let (build_complete_tx, build_complete_rx) = mpsc::channel(256);
        Self {
            engine,
            network,
            consensus_event_rx: None,
            net_event_rx,
            output_rx,
            el: None,
            consensus_state: None,
            head_block_hash: B256::ZERO,
            execution_validated_head_view: 0,
            eager_execution_validated: VecDeque::new(),
            pending_sidecar_diffs: BTreeMap::new(),
            execution_validated_sidecar_hashes: BTreeMap::new(),
            missing_sidecar_diffs: BTreeMap::new(),
            sidecar_apply_queue: Arc::new(std::sync::Mutex::new(VecDeque::new())),
            sidecar_apply_worker_running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            last_sidecar_applied_view: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            last_commit_qc: None,
            prev_randao_cache: B256::ZERO,
            block_ready_tx,
            block_ready_rx,
            fee_recipient: Address::ZERO,
            slot_time: Duration::ZERO,
            next_build_at: None,
            next_slot_timestamp: None,
            consecutive_empty_skips: 0,
            pending_block_data: BTreeMap::new(),
            recent_block_data: VecDeque::new(),
            pending_executions: HashSet::new(),
            pending_finalization: None,
            syncing_blocks: VecDeque::new(),
            bad_blocks: bad_block_cache::BadBlockCache::default(),
            tx_import_tx: None,
            tx_broadcast_rx: None,
            committed_blocks: VecDeque::new(),
            connected_peers: HashSet::new(),
            leader_build_waiting_view: None,
            leader_build_not_before: None,
            sync_in_flight: false,
            sync_started_at: None,
            sync_request_range: None,
            sync_requested_peers: HashSet::new(),
            sync_peer_cursor: 0,
            state_file: None,
            validator_set_for_sync: None,
            epoch_sync_staged_view: None,
            mobile_packet_tx: None,
            leader_payload_rx,
            leader_payload_tx,
            import_done_rx,
            import_done_tx,
            finalize_done_rx,
            finalize_done_tx,
            blob_store: None,
            exec_output_cache: None,
            bg_import_in_flight: false,
            bg_import_hashes: HashSet::new(),
            bg_import_queue: VecDeque::new(),
            eager_import_done_tx,
            eager_import_done_rx,
            speculative_build_hash: None,
            building_on_parent: None,
            build_triggered_at: None,
            build_complete_tx,
            build_complete_rx,
            staking_sink: None,
            withdrawal_source: None,
            committed_block_count: 0,
            last_committed_timestamp: 0,
            view_started_at: None,
            epoch_schedule: None,
            tx_forward_buffer: Vec::new(),
            pending_vote_resend: None,
            pipeline_timings: HashMap::new(),
            last_commit_instant: None,
            last_commit_view: None,
            last_commit_hash: None,
            timeout_view_diags: BTreeMap::new(),
            commit_to_build_recorded_parent: None,
            eager_import_block_guard: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            fast_propose: std::env::var("N42_FAST_PROPOSE")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0)
                > 0,
            async_finalize_fcu: Self::async_finalize_fcu_enabled(),
            defer_state_root: false,
            jmt: None,
            twig: None,
            zk_scheduler: None,
            allow_deterministic_validator_peers: true,
            admin_rx: None,
            evidence_store: None,
            view_jump_throttle: view_jump_throttle::ViewJumpThrottle::default(),
        }
    }

    pub(super) async fn enqueue_mobile_packet(
        &self,
        block_hash: B256,
        view: u64,
        reason: &'static str,
    ) {
        let Some(tx) = self.mobile_packet_tx.as_ref().cloned() else {
            return;
        };

        match tx.try_send((block_hash, view)) {
            Ok(()) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                warn!(
                    target: "n42::cl::mobile",
                    %block_hash,
                    view,
                    reason,
                    "mobile packet channel closed"
                );
            }
            Err(tokio::sync::mpsc::error::TrySendError::Full(packet)) => {
                warn!(
                    target: "n42::cl::mobile",
                    %block_hash,
                    view,
                    reason,
                    "mobile packet channel full, waiting to enqueue"
                );
                if let Err(error) = tx.send(packet).await {
                    warn!(
                        target: "n42::cl::mobile",
                        %block_hash,
                        view,
                        reason,
                        error = %error,
                        "mobile packet enqueue failed after backpressure wait"
                    );
                }
            }
        }
    }

    pub(super) async fn enqueue_tx_import(&self, data: Vec<u8>, reason: &'static str) {
        self.enqueue_tx_import_batch(vec![data], reason).await;
    }

    pub(super) async fn enqueue_tx_import_batch(&self, batch: TxImportBatch, reason: &'static str) {
        let Some(tx) = self.tx_import_tx.as_ref().cloned() else {
            return;
        };

        if batch.is_empty() {
            return;
        }

        match tx.try_send(batch) {
            Ok(()) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                warn!(target: "n42::cl::tx", reason, "tx import channel closed");
            }
            Err(tokio::sync::mpsc::error::TrySendError::Full(_batch)) => {
                // Drop the batch instead of blocking the consensus loop.
                // Under high load, waiting for the TX import channel to drain
                // causes event starvation and invalid-ancestor cascades.
                warn!(
                    target: "n42::cl::tx",
                    reason,
                    "tx import channel full, dropping batch to avoid blocking consensus loop"
                );
                counter!("n42_tx_import_drops_total").increment(1);
            }
        }
    }

    pub(super) fn pool_depth_snapshot(&self) -> PoolDepthSnapshot {
        self.consensus_state
            .as_ref()
            .map(|state| state.load_pool_depth())
            .unwrap_or_default()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_execution_layer(
        engine: ConsensusEngine,
        network: Arc<dyn ConsensusNetwork>,
        consensus_event_rx: mpsc::Receiver<NetworkEvent>,
        net_event_rx: mpsc::Receiver<NetworkEvent>,
        output_rx: mpsc::Receiver<EngineOutput>,
        el: Arc<dyn ExecutionLayer>,
        consensus_state: Arc<SharedConsensusState>,
        head_block_hash: B256,
        fee_recipient: Address,
    ) -> Self {
        let (block_ready_tx, block_ready_rx) = mpsc::channel(256);
        let (leader_payload_tx, leader_payload_rx) = mpsc::channel(64);
        let (import_done_tx, import_done_rx) = mpsc::channel(256);
        let (finalize_done_tx, finalize_done_rx) = mpsc::channel(256);
        let (eager_import_done_tx, eager_import_done_rx) = mpsc::channel(256);
        let (build_complete_tx, build_complete_rx) = mpsc::channel(256);

        let slot_time_ms: u64 = std::env::var("N42_BLOCK_INTERVAL_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let slot_time = Duration::from_millis(slot_time_ms);

        let fast_propose = std::env::var("N42_FAST_PROPOSE")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0)
            > 0;
        let async_finalize_fcu = Self::async_finalize_fcu_enabled();

        if slot_time_ms > 0 {
            info!(target: "n42::cl::orchestrator", slot_time_ms, fast_propose, async_finalize_fcu, "slot timing configured");
        }

        Self {
            engine,
            network,
            consensus_event_rx: Some(consensus_event_rx),
            net_event_rx,
            output_rx,
            el: Some(el),
            consensus_state: Some(consensus_state),
            head_block_hash,
            execution_validated_head_view: 0,
            eager_execution_validated: VecDeque::new(),
            pending_sidecar_diffs: BTreeMap::new(),
            execution_validated_sidecar_hashes: BTreeMap::new(),
            missing_sidecar_diffs: BTreeMap::new(),
            sidecar_apply_queue: Arc::new(std::sync::Mutex::new(VecDeque::new())),
            sidecar_apply_worker_running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            last_sidecar_applied_view: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            last_commit_qc: None,
            prev_randao_cache: B256::ZERO,
            block_ready_tx,
            block_ready_rx,
            fee_recipient,
            slot_time,
            next_build_at: None,
            next_slot_timestamp: None,
            consecutive_empty_skips: 0,
            pending_block_data: BTreeMap::new(),
            recent_block_data: VecDeque::new(),
            pending_executions: HashSet::new(),
            pending_finalization: None,
            syncing_blocks: VecDeque::new(),
            bad_blocks: bad_block_cache::BadBlockCache::default(),
            tx_import_tx: None,
            tx_broadcast_rx: None,
            committed_blocks: VecDeque::new(),
            connected_peers: HashSet::new(),
            leader_build_waiting_view: None,
            leader_build_not_before: None,
            sync_in_flight: false,
            sync_started_at: None,
            sync_request_range: None,
            sync_requested_peers: HashSet::new(),
            sync_peer_cursor: 0,
            state_file: None,
            validator_set_for_sync: None,
            epoch_sync_staged_view: None,
            mobile_packet_tx: None,
            leader_payload_rx,
            leader_payload_tx,
            import_done_rx,
            import_done_tx,
            finalize_done_rx,
            finalize_done_tx,
            blob_store: None,
            exec_output_cache: None,
            bg_import_in_flight: false,
            bg_import_hashes: HashSet::new(),
            bg_import_queue: VecDeque::new(),
            eager_import_done_tx,
            eager_import_done_rx,
            speculative_build_hash: None,
            building_on_parent: None,
            build_triggered_at: None,
            build_complete_tx,
            build_complete_rx,
            staking_sink: None,
            withdrawal_source: None,
            committed_block_count: 0,
            last_committed_timestamp: 0,
            view_started_at: None,
            epoch_schedule: None,
            tx_forward_buffer: Vec::new(),
            pending_vote_resend: None,
            pipeline_timings: HashMap::new(),
            last_commit_instant: None,
            last_commit_view: None,
            last_commit_hash: None,
            timeout_view_diags: BTreeMap::new(),
            commit_to_build_recorded_parent: None,
            eager_import_block_guard: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            fast_propose,
            async_finalize_fcu,
            defer_state_root: false,
            jmt: None,
            twig: None,
            zk_scheduler: None,
            allow_deterministic_validator_peers: true,
            admin_rx: None,
            evidence_store: None,
            view_jump_throttle: view_jump_throttle::ViewJumpThrottle::default(),
        }
    }

    pub fn with_exec_output_cache(mut self, cache: Arc<dyn ExecutionOutputCache>) -> Self {
        self.exec_output_cache = Some(cache);
        self
    }

    pub fn with_blob_store(mut self, blob_store: Arc<dyn BlobStorePort>) -> Self {
        self.blob_store = Some(blob_store);
        self
    }

    /// Caches the deferred-state-root mode (read from `N42_DEFER_STATE_ROOT` by the
    /// node) so the orchestrator does not call the reth EVM helper directly.
    pub fn with_defer_state_root(mut self, defer: bool) -> Self {
        self.defer_state_root = defer;
        self
    }

    pub fn with_state_persistence(mut self, path: PathBuf) -> Self {
        self.state_file = Some(path);
        self
    }

    pub fn with_validator_set(mut self, vs: ValidatorSet) -> Self {
        self.validator_set_for_sync = Some(vs);
        self
    }

    pub fn with_mobile_packet_tx(mut self, tx: mpsc::Sender<(B256, u64)>) -> Self {
        self.mobile_packet_tx = Some(tx);
        self
    }

    /// Attaches the reward+staking withdrawal source (build-path query). The
    /// node builds the concrete adapter (`NodeWithdrawalSource`) and passes the
    /// trait object so this crate stays free of the reward/staking concretes.
    pub fn with_withdrawal_source(mut self, source: Arc<dyn WithdrawalSource>) -> Self {
        self.withdrawal_source = Some(source);
        self
    }

    /// Attaches the staking scan/persist sink. The node builds the concrete
    /// adapter (`ManagerStakingSink`) and passes the trait object.
    pub fn with_staking_sink(mut self, sink: Arc<dyn StakingSink>) -> Self {
        self.staking_sink = Some(sink);
        self
    }

    /// Restores the committed block count from a persisted snapshot.
    /// Without this, the counter starts at 0 after restart, which can cause
    /// the `MobileRewardManager` to re-trigger epoch boundary rewards.
    pub fn with_committed_block_count(mut self, count: u64) -> Self {
        self.committed_block_count = count;
        self
    }

    /// Attaches an epoch schedule for dynamic validator set rotation.
    ///
    /// When set, the orchestrator consults the schedule at each `EpochTransition`
    /// event and automatically stages the next epoch's validator set.
    pub fn with_jmt(mut self, jmt: Arc<dyn StateSink>) -> Self {
        self.jmt = Some(jmt);
        self
    }

    pub fn with_twig(mut self, twig: Arc<dyn StateSink>) -> Self {
        self.twig = Some(twig);
        self
    }

    /// Attaches a ZK proof scheduler for asynchronous proof generation.
    ///
    /// When set, the orchestrator triggers ZK proof generation after each
    /// committed block (at the configured interval). Proofs are generated
    /// in `spawn_blocking` and never block the consensus path.
    pub fn with_zk_scheduler(mut self, sink: Arc<dyn ZkSink>) -> Self {
        self.zk_scheduler = Some(sink);
        self
    }

    pub fn with_epoch_schedule(mut self, schedule: EpochSchedule) -> Self {
        // The transition handler stages epoch N+1 after entering N, but a
        // freshly started/recovered service has not emitted such an event yet.
        // Pre-stage an exact next-epoch schedule entry here so the first
        // configured rotation is not skipped.
        let next_epoch = self.engine.epoch_manager().current_epoch() + 1;
        if !self.engine.epoch_manager().has_staged_next()
            && let Some((validators, fault_tolerance)) = schedule.get_for_epoch(next_epoch)
        {
            match self
                .engine
                .epoch_manager_mut()
                .stage_next_epoch(validators, fault_tolerance)
            {
                Ok(()) => info!(
                    target: "n42::cl::orchestrator",
                    next_epoch,
                    validator_count = validators.len(),
                    fault_tolerance,
                    "pre-staged next epoch validator set from schedule"
                ),
                Err(error) => warn!(
                    target: "n42::cl::orchestrator",
                    next_epoch,
                    %error,
                    "failed to pre-stage next epoch validator set from schedule"
                ),
            }
        }
        self.epoch_schedule = Some(schedule);
        self
    }

    pub fn with_allow_deterministic_validator_peers(mut self, allow: bool) -> Self {
        self.allow_deterministic_validator_peers = allow;
        self
    }

    /// Restores `prev_randao` derivation state from a recovered CommitQC (crash recovery).
    /// Without this, the first payload after restart would use `B256::ZERO` as `prev_randao`.
    pub fn with_recovered_commit_qc(mut self, qc: QuorumCertificate) -> Self {
        self.prev_randao_cache = alloy_primitives::keccak256(qc.aggregate_signature.to_bytes());
        self.last_commit_qc = Some(qc);
        self
    }

    /// Restores the execution-validity guard clock from a v4 snapshot (T1).
    ///
    /// The guard (`execution_validated_head_view`) is process-local while the
    /// consensus view stream and reth's canonical head are persistent. Left at
    /// 0 across restart it disables the stale/same-view refusals for the first
    /// confirmation and pins the post-restart catch-up floor at 0 (re-audit
    /// F1/F2). Startup proves the view belongs to the canonical hash before it
    /// calls this builder; `head_block_hash` itself already came directly from
    /// reth and is never replaced from the snapshot.
    pub fn with_recovered_execution_validated_head(mut self, view: u64) -> Self {
        self.execution_validated_head_view = view;
        self
    }

    /// Installs the admin command receiver for RPC-originated validator reconfig requests.
    pub fn with_admin_rx(
        mut self,
        rx: mpsc::Receiver<crate::consensus_state::AdminCommand>,
    ) -> Self {
        self.admin_rx = Some(rx);
        self
    }

    /// Attaches the MDBX-backed evidence store for persisting per-block consensus evidence.
    pub fn with_evidence_store(mut self, store: Arc<n42_jmt::EvidenceStore>) -> Self {
        self.evidence_store = Some(store);
        self
    }

    pub fn with_tx_pool_bridge(
        mut self,
        tx_import_tx: mpsc::Sender<TxImportBatch>,
        tx_broadcast_rx: mpsc::Receiver<Vec<u8>>,
    ) -> Self {
        self.tx_import_tx = Some(tx_import_tx);
        self.tx_broadcast_rx = Some(tx_broadcast_rx);
        self
    }

    /// Runs the orchestrator event loop. Never returns under normal operation.
    pub async fn run(mut self) {
        info!(
            target: "n42::cl::orchestrator",
            version = env!("CARGO_PKG_VERSION"),
            consensus = "HotStuff-2",
            validators = self.engine.validator_count(),
            view = self.engine.current_view(),
            head = %self.head_block_hash,
            "N42 consensus layer initialized"
        );

        self.initialize_startup_schedule().await;

        // Set validator context for Rotor relay forwarding in network layer
        self.network
            .set_validator_context(self.engine.my_index(), self.engine.validator_count())
            .await;

        loop {
            // Bind this Sleep to the view/deadline from which it was created. The
            // event loop recreates and drops the timer after every selected branch;
            // the checks in the timeout branch are defense-in-depth against a stale
            // timer if that structure changes in the future.
            let timeout_view = self.engine.current_view();
            let timeout = self.engine.pacemaker().timeout_sleep();
            tokio::pin!(timeout);

            let next_build_deadline = self.next_build_at;
            let build_timer = async {
                match next_build_deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending().await,
                }
            };
            tokio::pin!(build_timer);

            // TX forward flush timer: flush partial batches every 50ms.
            let tx_buf_has_data = !self.tx_forward_buffer.is_empty();
            let tx_flush_timer = async {
                if tx_buf_has_data {
                    tokio::time::sleep(Duration::from_millis(50)).await
                } else {
                    std::future::pending::<()>().await
                }
            };
            tokio::pin!(tx_flush_timer);

            let vote_resend_deadline = self
                .pending_vote_resend
                .as_ref()
                .map(|pending| pending.next_at);
            let vote_resend_timer = async {
                match vote_resend_deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending().await,
                }
            };
            tokio::pin!(vote_resend_timer);

            // Biased select: consensus-critical channels are checked FIRST.
            // Without biased, tokio randomly permutes branch order on each poll,
            // causing consensus votes to compete with high-frequency TX events.
            // Under 20K+ TPS, this random scheduling delays R1_collect by 500-1000ms.
            tokio::select! {
                biased;

                // === Priority 1: Safety-critical ===
                _ = &mut timeout => {
                    let view = self.engine.current_view();
                    if view != timeout_view || !self.engine.pacemaker().is_timed_out() {
                        debug!(target: "n42::cl::orchestrator", timeout_view, current_view = view,
                            "ignoring stale pacemaker timer");
                        continue;
                    }
                    counter!("n42_view_timeouts_total").increment(1);
                    self.record_timeout_diag_timeout_triggered(view);
                    warn!(target: "n42::cl::orchestrator", view, "pacemaker timeout, initiating view change");
                    if let Err(e) = self.engine.on_timeout() {
                        error!(target: "n42::cl::orchestrator", view, error = %e, "error handling timeout");
                    }
                    // Recovery: if we're still the leader after a repeat timeout and
                    // no build is in progress, schedule a build retry.
                    // This handles the case where FCU returned SYNCING earlier and
                    // the leader couldn't propose, causing a permanent stall.
                    if self.engine.is_current_leader()
                        && self.next_build_at.is_none()
                        && self.speculative_build_hash.is_none()
                    {
                        info!(target: "n42::cl::orchestrator", view, "leader timeout recovery: scheduling build retry");
                        self.schedule_build_retry();
                    }
                }

                // === Priority 2: Consensus engine outputs (Vote broadcast, ViewChanged, Decide) ===
                output = self.output_rx.recv() => {
                    match output {
                        Some(engine_output) => self.handle_engine_output(engine_output).await,
                        None => {
                            info!(target: "n42::cl::orchestrator", "engine output channel closed, shutting down orchestrator");
                            break;
                        }
                    }
                }

                // Keep the current view's locally signed R1/R2 vote alive when
                // a collector's direct or gossip receive path was transiently lost.
                _ = &mut vote_resend_timer => {
                    self.resend_pending_vote();
                }

                // === Priority 3: Async finalize-FCU completion ===
                finalize_done = async {
                    if self.finalize_done_rx.is_closed() && self.finalize_done_rx.is_empty() {
                        std::future::pending::<Option<FinalizeDone>>().await
                    } else {
                        self.finalize_done_rx.recv().await
                    }
                } => {
                    if let Some((view, block_hash, commit_qc, finalized)) = finalize_done {
                        self.handle_finalize_done(view, block_hash, commit_qc, finalized).await;
                        self.save_consensus_state();
                    }
                }

                // === Priority 4: Consensus network events (high-priority, dedicated channel) ===
                // Consensus messages (votes, proposals, QCs) are routed to a separate channel
                // so they are never queued behind high-volume TX/BlockData events.
                event = async {
                    match self.consensus_event_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    match event {
                        Some(ev) => {
                            let mut events = Vec::with_capacity(CONSENSUS_VOTE_BATCH_MAX);
                            events.push(ev);
                            if let Some(rx) = self.consensus_event_rx.as_mut() {
                                while events.len() < CONSENSUS_VOTE_BATCH_MAX {
                                    match rx.try_recv() {
                                        Ok(event) => events.push(event),
                                        Err(_) => break,
                                    }
                                }
                            }
                            self.handle_consensus_event_batch(events).await;
                        }
                        None => {
                            // Consensus channel closed is not fatal — may be observer mode.
                            self.consensus_event_rx = None;
                            debug!(target: "n42::cl::orchestrator", "consensus event channel closed");
                        }
                    }
                }

                // === Priority 5: Block build completion ===
                payload_ready = async {
                    if self.block_ready_rx.is_closed() && self.block_ready_rx.is_empty() {
                        std::future::pending::<Option<PayloadBuildReady>>().await
                    } else {
                        self.block_ready_rx.recv().await
                    }
                } => {
                    if let Some(ready) = payload_ready {
                        self.handle_payload_build_ready(ready);
                    }
                }

                // === Priority 6: Data network events (TX forward, block data, sync, peers) ===
                // Lower priority than consensus — under high TPS these fire thousands of
                // times per second but should never delay vote/proposal processing.
                event = self.net_event_rx.recv() => {
                    match event {
                        Some(ev) => self.handle_network_event(ev).await,
                        None => {
                            info!(target: "n42::cl::orchestrator", "network event channel closed, shutting down orchestrator");
                            break;
                        }
                    }
                }

                // === Priority 7: Import and build lifecycle ===
                import_result = async {
                    if self.import_done_rx.is_closed() && self.import_done_rx.is_empty() {
                        std::future::pending::<Option<ImportDone>>().await
                    } else {
                        self.import_done_rx.recv().await
                    }
                } => {
                    if let Some((hash, view, outcome, block_ts)) = import_result {
                        self.handle_import_done(hash, view, outcome, block_ts).await;
                        // Background import completion can advance the
                        // execution-validity guard after the commit-time
                        // snapshot was written. Persist the exact new pair now.
                        self.save_consensus_state();
                    }
                }

                eager_done = async {
                    if self.eager_import_done_rx.is_closed() && self.eager_import_done_rx.is_empty() {
                        std::future::pending::<Option<(B256, u64)>>().await
                    } else {
                        self.eager_import_done_rx.recv().await
                    }
                } => {
                    if let Some((hash, block_ts)) = eager_done {
                        // Pipeline: import complete — record timing.
                        if let Some(timing) = self.pipeline_timings.get_mut(&hash) {
                            timing.import_complete = Some(Instant::now());
                        }
                        self.handle_eager_import_done(hash, block_ts).await;
                        // A late eager completion may close a committed Case C
                        // and advance the guard outside handle_block_committed.
                        self.save_consensus_state();
                    }
                }

                payload_data = async {
                    if self.leader_payload_rx.is_closed() && self.leader_payload_rx.is_empty() {
                        std::future::pending::<Option<(B256, Vec<u8>)>>().await
                    } else {
                        self.leader_payload_rx.recv().await
                    }
                } => {
                    if let Some((hash, data)) = payload_data {
                        self.handle_leader_payload_feedback(hash, data).await;
                    }
                }

                completed_build = async {
                    if self.build_complete_rx.is_closed() && self.build_complete_rx.is_empty() {
                        std::future::pending::<Option<PayloadBuildContext>>().await
                    } else {
                        self.build_complete_rx.recv().await
                    }
                } => {
                    // Payload resolve task finished (success or failure).
                    // A stale task must never clear a newer view's build guard.
                    if let Some(completed) = completed_build {
                        self.handle_payload_build_complete(completed);
                    }
                }

                // === Priority 8: Slot timing ===
                _ = &mut build_timer => {
                    let slot_ts = self.next_slot_timestamp.take();
                    self.next_build_at = None;

                    if slot_ts.is_none() {
                        // Retry or startup delay: verify we're still the leader before building.
                        if !self.engine.is_current_leader() {
                            debug!(target: "n42::cl::orchestrator", "build timer fired but no longer leader, skipping");
                            continue;
                        }
                        let view = self.engine.current_view();
                        if self.speculative_build_hash.is_some() {
                            debug!(target: "n42::cl::orchestrator", view, "build timer fired but speculative build in progress, skipping");
                            continue;
                        }
                        self.arm_leader_build_timer_as_direct_wait(None);
                        // Check if this is a startup recheck or a regular re-check:
                        // on initial view, we still gate strictly on quorum peers before
                        // attempting first proposal.
                        if view <= 1 {
                            self.engine.pacemaker_mut().reset_for_view(view, 0);
                        }
                        if view <= 1 {
                            info!(
                                target: "n42::cl::orchestrator",
                                "leader build timer reached; checking validator peer quorum"
                            );
                        } else {
                            info!(
                                target: "n42::cl::orchestrator",
                                view,
                                "leader build timer reached; re-checking validator peer quorum"
                            );
                        }
                    } else {
                        info!(target: "n42::cl::orchestrator", slot_timestamp = ?slot_ts, "slot boundary reached, triggering payload build");
                        self.arm_leader_build_timer_as_direct_wait(slot_ts);
                    }

                    self.evaluate_leader_build_wait(slot_ts).await;
                }

                // === Priority 9: TX forwarding (lowest priority) ===
                // Under high load, TX events fire thousands of times per second.
                // Deferring them ensures consensus votes are never delayed by TX buffering.
                tx_data = async {
                    match self.tx_broadcast_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    // TX Forward to Leader: instead of GossipSub broadcast, buffer txs
                    // and forward to the current leader in batches.
                    if let Some(data) = tx_data {
                        self.tx_forward_buffer.push(data);
                    }
                    // Batch-drain up to the configured target before forwarding/importing.
                    if let Some(rx) = self.tx_broadcast_rx.as_mut() {
                        for _ in 1..TX_FORWARD_BATCH_TARGET {
                            match rx.try_recv() {
                                Ok(data) => { self.tx_forward_buffer.push(data); }
                                Err(_) => break,
                            }
                        }
                    }
                    // Flush if buffer is large enough.
                    if self.tx_forward_buffer.len() >= TX_FORWARD_BATCH_TARGET {
                        self.flush_tx_forward_buffer().await;
                    }
                }

                _ = &mut tx_flush_timer => {
                    self.flush_tx_forward_buffer().await;
                }

                // === Priority 10: Admin commands (validator reconfig from RPC) ===
                msg = async {
                    match self.admin_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    if let Some(cmd) = msg {
                        self.handle_admin_command(cmd);
                    }
                }

            }

            // Eagerly drain all pending consensus engine outputs after each iteration.
            // The consensus engine can emit multiple outputs per event (e.g., Vote +
            // ViewChanged). Processing them immediately avoids waiting for the next
            // select! iteration, which may be delayed by lower-priority branches.
            while let Ok(engine_output) = self.output_rx.try_recv() {
                self.handle_engine_output(engine_output).await;
            }
        }

        info!(target: "n42::cl::orchestrator", view = self.engine.current_view(), "orchestrator shutting down, persisting final state");
        self.save_shutdown_state();
    }

    async fn initialize_startup_schedule(&mut self) {
        if !self.engine.is_current_leader() || self.el.is_none() {
            return;
        }

        let warmup_delay = Self::leader_propose_warmup_delay();
        let warmup_delay = if self.quorum_peers_needed() == 0 {
            Duration::ZERO
        } else {
            warmup_delay
        };
        self.begin_leader_build_wait(LeaderBuildWaitMode::Direct, None);
        if warmup_delay.is_zero() {
            info!(
                target: "n42::cl::orchestrator",
                validators = self.engine.validator_count(),
                "leader for view 1, attempting immediate proposal after quorum check"
            );
            self.evaluate_leader_build_wait(None).await;
            return;
        }

        info!(
            target: "n42::cl::orchestrator",
            delay_ms = warmup_delay.as_millis() as u64,
            validators = self.engine.validator_count(),
            needed_quorum_peers = self.quorum_peers_needed(),
            "leader for view 1, waiting for validator quorum (warmup floor)"
        );
        self.leader_build_not_before =
            Some((self.engine.current_view(), Instant::now() + warmup_delay));
        self.schedule_leader_build_recheck(warmup_delay);
        self.engine.pacemaker_mut().extend_deadline(warmup_delay);
    }

    /// Records a pipeline timing entry for a block, with bounded map size.
    fn record_pipeline_timing(&mut self, hash: B256, timing: PipelineTiming) {
        const MAX_PIPELINE_ENTRIES: usize = 32;
        if self.pipeline_timings.len() >= MAX_PIPELINE_ENTRIES {
            // Evict oldest entry (smallest `created` timestamp).
            if let Some(oldest) = self
                .pipeline_timings
                .iter()
                .min_by_key(|(_, t)| t.created)
                .map(|(k, _)| *k)
            {
                self.pipeline_timings.remove(&oldest);
            }
        }
        self.pipeline_timings.insert(hash, timing);
    }

    fn handle_admin_command(&mut self, cmd: crate::consensus_state::AdminCommand) {
        use crate::consensus_state::AdminCommand;
        match cmd {
            AdminCommand::AddValidator { info, reply } => {
                let result = self
                    .engine
                    .propose_add_validator(info)
                    .map_err(|e| e.to_string());
                let _ = reply.send(result);
            }
            AdminCommand::RemoveValidator { address, reply } => {
                let result = self
                    .engine
                    .propose_remove_validator(address)
                    .map_err(|e| e.to_string());
                let _ = reply.send(result);
            }
        }
        // Refresh epoch status so RPC immediately reflects pending changes.
        self.refresh_epoch_status();
    }

    /// Publishes the current epoch/pending/staged state to `SharedConsensusState`
    /// so that `n42_validatorSet` RPC returns up-to-date transition info.
    fn refresh_epoch_status(&self) {
        if let Some(ref state) = self.consensus_state {
            let em = self.engine.epoch_manager();
            let next_count = em.peek_next_set().map_or(0, |s| s.len() as usize);
            state.update_epoch_status(crate::consensus_state::EpochStatus {
                current_epoch: em.current_epoch(),
                pending_changes: em.pending_change_count(),
                staged_next_epoch: em.has_staged_next(),
                next_epoch_validator_count: next_count,
            });
        }
    }

    fn trim_tx_forward_buffer(&mut self, leader_idx: u32, reason: &'static str) {
        if self.tx_forward_buffer.len() > 4096 {
            let excess = self.tx_forward_buffer.len() - 2048;
            self.tx_forward_buffer.drain(..excess);
            counter!("n42_tx_forward_dropped").increment(excess as u64);
            warn!(
                target: "n42::cl::orchestrator",
                leader_idx,
                dropped = excess,
                buffer_len = self.tx_forward_buffer.len(),
                reason,
                "dropped oldest txs from forward buffer"
            );
        }
    }

    /// Broadcasts a consensus message using Rotor relay: send directly to relay nodes,
    /// then always broadcast via GossipSub as safety net.
    fn broadcast_via_rotor(&mut self, msg: n42_primitives::ConsensusMessage) {
        use n42_consensus::rotor::cached_relay_assignment;

        let view = self.engine.current_view();
        let validator_count = self.engine.epoch_manager().current_validator_set().len();
        let leader = self.engine.current_leader_index();
        let assignment = cached_relay_assignment(view, validator_count, leader, 3);

        let mut direct_ok = 0u32;
        let mut direct_fail = 0u32;
        for (relay_idx, _targets) in &assignment.relays {
            if let Some(peer_id) = self.network.validator_peer(*relay_idx) {
                // Fire-and-forget: do NOT await relay sends. Blocking the consensus
                // loop on network I/O causes event starvation under 48K+ load.
                match self.network.send_direct(peer_id, msg.clone()) {
                    Ok(()) => direct_ok += 1,
                    Err(e) => {
                        warn!(
                            target: "n42::cl::rotor",
                            relay = relay_idx,
                            error = %e,
                            "rotor: relay direct send failed"
                        );
                        direct_fail += 1;
                    }
                }
            } else {
                direct_fail += 1;
            }
        }

        // Always broadcast via GossipSub as safety net (non-blocking)
        if let Err(e) = self.network.broadcast_consensus(msg) {
            error!(target: "n42::cl::rotor", error = %e, "rotor: gossipsub fallback failed");
        }

        if direct_ok > 0 {
            counter!("n42_rotor_direct_sends").increment(direct_ok as u64);
        }
        if direct_fail > 0 {
            counter!("n42_rotor_fallback_used").increment(direct_fail as u64);
        }
    }

    fn restore_failed_tx_forward_batch(&mut self, txs: Vec<Vec<u8>>, leader_idx: u32) {
        self.tx_forward_buffer = txs;
        self.trim_tx_forward_buffer(leader_idx, "forward send failure");
    }

    /// Flush buffered txs to the current leader via the tx_forward protocol.
    async fn flush_tx_forward_buffer(&mut self) {
        if self.tx_forward_buffer.is_empty() {
            return;
        }

        // N42_DISABLE_TX_FORWARD=1: keep txs in local pool only (for sync ingest benchmarks).
        // Txs are still accepted into the local pool but never forwarded to leader.
        static DISABLE_TX_FORWARD: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let disabled = *DISABLE_TX_FORWARD.get_or_init(|| {
            std::env::var("N42_DISABLE_TX_FORWARD")
                .map(|v| v == "1")
                .unwrap_or(false)
        });
        if disabled {
            // Feed all buffered txs into local pool and return (no forwarding).
            let txs = std::mem::take(&mut self.tx_forward_buffer);
            if self.tx_import_tx.is_some() {
                self.enqueue_tx_import_batch(txs, "tx forward disabled")
                    .await;
            }
            return;
        }

        let leader_idx = self.engine.current_leader_index();
        let validator_peers = self.network.all_validator_peers();

        // If we are the leader, feed txs directly into the local pool.
        if self.engine.is_current_leader() {
            let txs = std::mem::take(&mut self.tx_forward_buffer);
            let count = txs.len();
            if self.tx_import_tx.is_some() {
                self.enqueue_tx_import_batch(txs, "leader local tx import")
                    .await;
            }
            counter!("n42_tx_forward_local").increment(count as u64);
            return;
        }

        // Find the leader's PeerId.
        let leader_peer = validator_peers
            .iter()
            .find(|(idx, _)| *idx == leader_idx)
            .map(|(_, pid)| *pid);

        match leader_peer {
            Some(peer) => {
                let txs = std::mem::take(&mut self.tx_forward_buffer);
                let count = txs.len();
                debug!(target: "n42::cl::orchestrator", count, leader_idx, %peer, "flushing tx forward buffer to leader");
                match self.network.forward_tx_batch(peer, txs.clone()) {
                    Ok(()) => {
                        counter!("n42_tx_forward_batches").increment(1);
                        counter!("n42_tx_forward_txs").increment(count as u64);
                    }
                    Err(error) => {
                        self.restore_failed_tx_forward_batch(txs, leader_idx);
                        warn!(
                            target: "n42::cl::orchestrator",
                            leader_idx,
                            %peer,
                            count,
                            error = %error,
                            "failed to forward tx batch to leader"
                        );
                    }
                }
            }
            None => {
                // Leader not connected yet — fall back to keeping in buffer.
                // If buffer grows too large, drop oldest to prevent memory bloat.
                let buf_len = self.tx_forward_buffer.len();
                warn!(target: "n42::cl::orchestrator", leader_idx, buf_len, peers = validator_peers.len(), "leader peer not found for tx forward");
                self.trim_tx_forward_buffer(leader_idx, "leader peer not found");
            }
        }
    }

    async fn handle_consensus_event_batch(&mut self, events: Vec<NetworkEvent>) {
        let current_view = self.engine.current_view();
        let mut positions = Vec::new();
        let mut messages = Vec::new();
        for (position, event) in events.iter().enumerate() {
            let NetworkEvent::ConsensusMessage { message, .. } = event else {
                continue;
            };
            let eligible = matches!(
                message.as_ref(),
                n42_primitives::ConsensusMessage::Vote(vote) if vote.view == current_view
            ) || matches!(
                message.as_ref(),
                n42_primitives::ConsensusMessage::CommitVote(vote) if vote.view == current_view
            );
            if eligible {
                positions.push(position);
                messages.push(message.as_ref().clone());
            }
        }

        if messages.len() < 2 {
            for event in events {
                self.handle_network_event(event).await;
            }
            return;
        }

        let verify_started = std::time::Instant::now();
        let authenticated = self.engine.authenticate_vote_batch(messages);
        histogram!("n42_consensus_vote_batch_size").record(authenticated.len() as f64);
        histogram!("n42_consensus_vote_batch_verify_ms")
            .record(verify_started.elapsed().as_secs_f64() * 1000.0);

        let mut hints: Vec<Option<Option<AuthenticatedConsensusMessage>>> =
            (0..events.len()).map(|_| None).collect();
        for (position, authenticated) in positions.into_iter().zip(authenticated) {
            hints[position] = Some(authenticated);
        }

        for (event, hint) in events.into_iter().zip(hints) {
            self.handle_network_event_with_auth(event, hint).await;
        }
    }

    async fn handle_network_event(&mut self, event: NetworkEvent) {
        self.handle_network_event_with_auth(event, None).await;
    }

    async fn handle_network_event_with_auth(
        &mut self,
        event: NetworkEvent,
        preauthenticated: Option<Option<AuthenticatedConsensusMessage>>,
    ) {
        match event {
            NetworkEvent::ConsensusMessage { source, message } => {
                counter!("n42_consensus_messages_received").increment(1);
                use n42_primitives::ConsensusMessage as CM;
                let msg_type = match message.as_ref() {
                    CM::Proposal(_) => "Proposal",
                    CM::Vote(_) => "Vote",
                    CM::CommitVote(_) => "CommitVote",
                    CM::Timeout(_) => "Timeout",
                    CM::NewView(_) => "NewView",
                    CM::Decide(_) => "Decide",
                    _ => "Other",
                };

                // Rate-limit messages that would force the engine into the BLS-heavy
                // QC view-jump path. We bypass Decide / NewView (they own their view
                // logic and are cheaper to verify) and Timeout within the buffer
                // window. Anything else more than FUTURE_VIEW_WINDOW ahead must
                // consume a token from this peer's bucket or be dropped.
                let needs_view_jump = !matches!(message.as_ref(), CM::Decide(_) | CM::NewView(_))
                    && message.view()
                        > self
                            .engine
                            .current_view()
                            .saturating_add(FUTURE_VIEW_WINDOW);
                if needs_view_jump && !self.view_jump_throttle.try_consume(source) {
                    counter!("n42_view_jump_throttled_total").increment(1);
                    debug!(
                        target: "n42::cl::orchestrator",
                        %source,
                        msg_type,
                        msg_view = message.view(),
                        current_view = self.engine.current_view(),
                        "view-jump message rate-limited"
                    );
                    return;
                }

                let authenticated = match preauthenticated {
                    Some(Some(authenticated)) => Some(authenticated),
                    // CommitVote's signature domain includes proposal-derived
                    // validator_changes_hash. Batch verification happens before
                    // earlier events in this drain are dispatched, so a failed
                    // precheck can become valid after an earlier Proposal fills
                    // the cache. Re-run the canonical individual check in that
                    // case; Vote has no mutable domain and can be dropped here.
                    Some(None) if matches!(message.as_ref(), CM::CommitVote(_)) => {
                        counter!("n42_consensus_commit_vote_batch_domain_fallback_total")
                            .increment(1);
                        None
                    }
                    Some(None) => {
                        counter!("n42_consensus_vote_batch_invalid_total").increment(1);
                        debug!(
                            target: "n42::cl::orchestrator",
                            %source,
                            msg_type,
                            "dropping vote rejected by batch signature verification"
                        );
                        return;
                    }
                    None => None,
                };
                let validator_index = authenticated
                    .as_ref()
                    .map(AuthenticatedConsensusMessage::signer)
                    .or_else(|| self.engine.authenticated_signer(message.as_ref()));
                if let Some(validator_index) = validator_index
                    && let Err(error) = self
                        .network
                        .authenticate_validator_peer_reliable(source, validator_index)
                        .await
                {
                    debug!(
                        target: "n42::cl::orchestrator",
                        %source,
                        validator_index,
                        error = %error,
                        "failed to promote authenticated validator peer"
                    );
                }
                // Save the message view before process_event consumes it via *message.
                let msg_view = message.view();
                debug!(target: "n42::cl::orchestrator", msg_type, view = self.engine.current_view(), "processing consensus message");
                let result = match authenticated {
                    Some(authenticated) => self.engine.process_authenticated_message(authenticated),
                    None => self
                        .engine
                        .process_event(n42_consensus::ConsensusEvent::Message(*message)),
                };
                match result {
                    Ok(()) => {}
                    Err(e) => {
                        if matches!(e, n42_consensus::N42ConsensusError::SafetyViolation { .. }) {
                            debug!(target: "n42::cl::orchestrator", error = %e, "benign safety check (QC ordering race)");
                        } else {
                            warn!(target: "n42::cl::orchestrator", error = %e, "error processing consensus message");
                            // Bitmap length mismatch means the peer used a different epoch's
                            // validator set to sign the QC/TC — our epoch state has diverged.
                            // Trigger sync so we can recover the missing epoch transition.
                            let is_bitmap_mismatch = matches!(
                                &e,
                                n42_consensus::N42ConsensusError::InvalidQC { reason, .. }
                                    if reason.contains("bitmap length mismatch")
                            ) || matches!(
                                &e,
                                n42_consensus::N42ConsensusError::InvalidTC { reason, .. }
                                    if reason.contains("bitmap length mismatch")
                            );
                            if is_bitmap_mismatch {
                                let local_view = self.engine.current_view();
                                warn!(
                                    target: "n42::cl::orchestrator",
                                    current_view = local_view,
                                    msg_view,
                                    "epoch state divergence detected (bitmap mismatch) — triggering sync"
                                );
                                // Sync from two epochs back up to the message's view so we
                                // capture the validator-change block that caused the mismatch.
                                // Using msg_view (not local_view) as the target is critical:
                                // when local_view=0 and msg_view=45, initiate_sync(0,0) would
                                // produce an invalid from_view=1 > to_view=0 request that the
                                // peer rejects silently, leaving the node permanently stuck.
                                let epoch_len = self.engine.epoch_manager().epoch_length().max(1);
                                let from = local_view.saturating_sub(epoch_len * 2);
                                let target = msg_view.max(local_view);
                                self.initiate_sync(from, target);
                            }
                        }
                    }
                }
            }
            NetworkEvent::PeerConnected(peer_id) => {
                info!(target: "n42::cl::orchestrator", %peer_id, "consensus peer connected");
                self.connected_peers.insert(peer_id);
                gauge!("n42_connected_peers").set(self.connected_peers.len() as f64);

                if self.pending_leader_build_mode_for_current_view().is_some() {
                    let slot_timestamp = self.next_slot_timestamp;
                    self.evaluate_leader_build_wait(slot_timestamp).await;
                }
            }
            NetworkEvent::PeerDisconnected(peer_id) => {
                warn!(target: "n42::cl::orchestrator", %peer_id, "consensus peer disconnected");
                self.connected_peers.remove(&peer_id);
                self.view_jump_throttle.forget(&peer_id);
                gauge!("n42_connected_peers").set(self.connected_peers.len() as f64);

                if self.pending_leader_build_mode_for_current_view().is_some() {
                    self.schedule_leader_build_recheck(Self::leader_quorum_recheck_delay());
                }
            }
            NetworkEvent::BlockAnnouncement { source, data } => {
                tracing::debug!(target: "n42::cl::orchestrator", %source, bytes = data.len(), "received block data broadcast");
                self.handle_block_data(data).await;
            }
            NetworkEvent::TransactionReceived { source: _, data } => {
                self.enqueue_tx_import(data, "p2p transaction received")
                    .await;
            }
            NetworkEvent::TxForwardReceived { source: _, txs } => {
                // Leader receives forwarded txs from validators — feed into local pool.
                let count = txs.len();
                self.enqueue_tx_import_batch(txs, "validator tx forward received")
                    .await;
                counter!("n42_tx_forward_imported").increment(count as u64);
            }
            NetworkEvent::TxForwardCreditUpdate { .. } => {
                // Reserved for future credit-based flow control.
            }
            NetworkEvent::SyncRequest {
                peer,
                request_id,
                request,
            } => {
                self.handle_sync_request(peer, request_id, request).await;
            }
            NetworkEvent::SyncResponse { peer, response } => {
                self.handle_sync_response(peer, response).await;
            }
            NetworkEvent::SyncRequestFailed { peer, error } => {
                warn!(target: "n42::cl::orchestrator", %peer, %error, "sync request failed");
                if self.sync_requested_peers.remove(&peer) && self.sync_requested_peers.is_empty() {
                    self.sync_in_flight = false;
                    self.sync_started_at = None;
                    self.sync_request_range = None;
                }
            }
            NetworkEvent::BlobSidecarReceived { source: _, data } => {
                self.handle_blob_sidecar(data);
            }
            _ => {
                // Verification receipts are handled by dedicated subsystems.
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, B256, Bloom, Bytes, U256};
    use alloy_rpc_types_engine::{
        ExecutionData, ExecutionPayload, ExecutionPayloadSidecar, ExecutionPayloadV1,
        ForkchoiceState, ForkchoiceUpdated, PayloadAttributes, PayloadId, PayloadStatus,
        PayloadStatusEnum,
    };
    use libp2p::identity::Keypair;
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};
    use n42_chainspec::ValidatorInfo;
    use n42_consensus::{ConsensusEngine, EpochManager, ValidatorSet};
    use n42_network::{NetworkCommand, NetworkHandle};
    use n42_primitives::consensus::ValidatorChange;
    use n42_primitives::{BlsSecretKey, ConsensusMessage, QuorumCertificate, TimeoutMessage, Vote};
    use std::collections::{HashMap, HashSet};
    use std::sync::{Arc, Mutex, RwLock};
    use std::time::Duration;

    #[derive(Clone, Default)]
    struct MockConsensusNetwork {
        peers: Arc<RwLock<HashMap<u32, n42_network::PeerId>>>,
        broadcasts: Arc<Mutex<Vec<n42_primitives::ConsensusMessage>>>,
        direct_messages: Arc<Mutex<Vec<(n42_network::PeerId, n42_primitives::ConsensusMessage)>>>,
    }

    impl MockConsensusNetwork {
        fn with_peers(peers: Vec<(u32, n42_network::PeerId)>) -> Self {
            let peer_map = peers.into_iter().collect::<HashMap<_, _>>();
            Self {
                peers: Arc::new(RwLock::new(peer_map)),
                ..Self::default()
            }
        }

        fn broadcasts(&self) -> Vec<n42_primitives::ConsensusMessage> {
            self.broadcasts
                .lock()
                .expect("mock broadcasts lock")
                .clone()
        }

        fn direct_messages(&self) -> Vec<(n42_network::PeerId, n42_primitives::ConsensusMessage)> {
            self.direct_messages
                .lock()
                .expect("mock direct messages lock")
                .clone()
        }
    }

    #[derive(Clone)]
    struct MockExecutionLayer {
        new_payload_statuses: Arc<Mutex<VecDeque<PayloadStatusEnum>>>,
        fcu_statuses: Arc<Mutex<VecDeque<PayloadStatusEnum>>>,
        build_parents: Arc<Mutex<Vec<B256>>>,
        build_forkchoices: Arc<Mutex<Vec<(B256, B256, B256)>>>,
        /// Every attribute-less forkchoiceUpdated head - the canonical-writer
        /// audit trail (import paths must NEVER appear here).
        fcu_heads: Arc<Mutex<Vec<B256>>>,
        /// Every new_payload block hash.
        new_payload_hashes: Arc<Mutex<Vec<B256>>>,
    }

    impl MockExecutionLayer {
        fn new(new_payload_status: PayloadStatusEnum, fcu_status: PayloadStatusEnum) -> Self {
            Self {
                new_payload_statuses: Arc::new(Mutex::new(VecDeque::from([new_payload_status]))),
                fcu_statuses: Arc::new(Mutex::new(VecDeque::from([fcu_status]))),
                build_parents: Arc::new(Mutex::new(Vec::new())),
                build_forkchoices: Arc::new(Mutex::new(Vec::new())),
                fcu_heads: Arc::new(Mutex::new(Vec::new())),
                new_payload_hashes: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn with_fcu_statuses(
            new_payload_status: PayloadStatusEnum,
            fcu_statuses: impl IntoIterator<Item = PayloadStatusEnum>,
        ) -> Self {
            let fcu_statuses = fcu_statuses.into_iter().collect::<VecDeque<_>>();
            assert!(
                !fcu_statuses.is_empty(),
                "at least one FCU status is required"
            );
            Self {
                new_payload_statuses: Arc::new(Mutex::new(VecDeque::from([new_payload_status]))),
                fcu_statuses: Arc::new(Mutex::new(fcu_statuses)),
                build_parents: Arc::new(Mutex::new(Vec::new())),
                build_forkchoices: Arc::new(Mutex::new(Vec::new())),
                fcu_heads: Arc::new(Mutex::new(Vec::new())),
                new_payload_hashes: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn with_new_payload_statuses(
            new_payload_statuses: impl IntoIterator<Item = PayloadStatusEnum>,
            fcu_status: PayloadStatusEnum,
        ) -> Self {
            let new_payload_statuses = new_payload_statuses.into_iter().collect::<VecDeque<_>>();
            assert!(
                !new_payload_statuses.is_empty(),
                "at least one new-payload status is required"
            );
            Self {
                new_payload_statuses: Arc::new(Mutex::new(new_payload_statuses)),
                fcu_statuses: Arc::new(Mutex::new(VecDeque::from([fcu_status]))),
                build_parents: Arc::new(Mutex::new(Vec::new())),
                build_forkchoices: Arc::new(Mutex::new(Vec::new())),
                fcu_heads: Arc::new(Mutex::new(Vec::new())),
                new_payload_hashes: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn fcu_heads(&self) -> Vec<B256> {
            self.fcu_heads
                .lock()
                .expect("mock execution-layer lock")
                .clone()
        }

        fn new_payload_hashes(&self) -> Vec<B256> {
            self.new_payload_hashes
                .lock()
                .expect("mock execution-layer lock")
                .clone()
        }

        fn build_parents(&self) -> Vec<B256> {
            self.build_parents
                .lock()
                .expect("mock execution-layer lock")
                .clone()
        }

        fn build_forkchoices(&self) -> Vec<(B256, B256, B256)> {
            self.build_forkchoices
                .lock()
                .expect("mock execution-layer lock")
                .clone()
        }
    }

    #[async_trait::async_trait]
    impl ExecutionLayer for MockExecutionLayer {
        async fn new_payload(
            &self,
            payload: ExecutionData,
        ) -> Result<PayloadStatus, crate::el::ElError> {
            self.new_payload_hashes
                .lock()
                .expect("mock execution-layer lock")
                .push(payload.payload.block_hash());
            let mut statuses = self
                .new_payload_statuses
                .lock()
                .expect("mock execution-layer lock");
            let status = if statuses.len() > 1 {
                statuses.pop_front().expect("non-empty status queue")
            } else {
                statuses.front().expect("non-empty status queue").clone()
            };
            Ok(PayloadStatus::from_status(status))
        }

        async fn fork_choice_updated(
            &self,
            state: ForkchoiceState,
        ) -> Result<ForkchoiceUpdated, crate::el::ElError> {
            self.fcu_heads
                .lock()
                .expect("mock execution-layer lock")
                .push(state.head_block_hash);
            let mut statuses = self.fcu_statuses.lock().expect("mock execution-layer lock");
            let status = if statuses.len() > 1 {
                statuses.pop_front().expect("non-empty FCU status queue")
            } else {
                statuses
                    .front()
                    .expect("non-empty FCU status queue")
                    .clone()
            };
            Ok(ForkchoiceUpdated::from_status(status))
        }

        async fn fork_choice_updated_with_attrs(
            &self,
            state: ForkchoiceState,
            _attrs: PayloadAttributes,
        ) -> Result<ForkchoiceUpdated, crate::el::ElError> {
            self.build_forkchoices
                .lock()
                .expect("mock execution-layer lock")
                .push((
                    state.head_block_hash,
                    state.safe_block_hash,
                    state.finalized_block_hash,
                ));
            self.build_parents
                .lock()
                .expect("mock execution-layer lock")
                .push(state.head_block_hash);
            Ok(ForkchoiceUpdated::from_status(PayloadStatusEnum::Valid))
        }

        async fn resolve_payload(
            &self,
            _id: PayloadId,
            _kind: crate::el::ResolveKind,
        ) -> Option<Result<crate::el::BuiltBlock, crate::el::ElError>> {
            None
        }
    }

    #[derive(Default)]
    struct MockExecutionOutputCache {
        active: Mutex<HashSet<B256>>,
        injected: Mutex<Vec<B256>>,
        evicted: Mutex<Vec<B256>>,
    }

    impl MockExecutionOutputCache {
        fn contains(&self, hash: B256) -> bool {
            self.active.lock().expect("mock cache lock").contains(&hash)
        }

        fn evicted(&self) -> Vec<B256> {
            self.evicted.lock().expect("mock cache lock").clone()
        }
    }

    impl crate::exec_cache::ExecutionOutputCache for MockExecutionOutputCache {
        fn take_serialized(&self, _hash: B256) -> Option<Vec<u8>> {
            None
        }

        fn inject(&self, hash: B256, _compressed: &[u8], _source: &'static str) -> bool {
            self.active.lock().expect("mock cache lock").insert(hash);
            self.injected.lock().expect("mock cache lock").push(hash);
            true
        }

        fn evict(&self, hash: B256) {
            self.active.lock().expect("mock cache lock").remove(&hash);
            self.evicted.lock().expect("mock cache lock").push(hash);
        }
    }

    fn test_execution_data_at_number(
        parent_hash: B256,
        block_hash: B256,
        block_number: u64,
    ) -> ExecutionData {
        ExecutionData::new(
            ExecutionPayload::V1(ExecutionPayloadV1 {
                parent_hash,
                fee_recipient: Address::ZERO,
                state_root: B256::ZERO,
                receipts_root: B256::ZERO,
                logs_bloom: Bloom::ZERO,
                prev_randao: B256::ZERO,
                block_number,
                gas_limit: 30_000_000,
                gas_used: 0,
                timestamp: 1,
                extra_data: Bytes::new(),
                base_fee_per_gas: U256::from(1),
                block_hash,
                transactions: Vec::new(),
            }),
            ExecutionPayloadSidecar::none(),
        )
    }

    fn test_execution_data(parent_hash: B256, block_hash: B256) -> ExecutionData {
        test_execution_data_at_number(parent_hash, block_hash, 1)
    }

    fn test_block_data(parent_hash: B256, block_hash: B256, view: u64) -> Vec<u8> {
        let payload_json = serde_json::to_vec(&test_execution_data(parent_hash, block_hash))
            .expect("serialize test execution data");
        bincode::serialize(&BlockDataBroadcast {
            block_hash,
            view,
            payload_json,
            timestamp: 1,
            execution_output: None,
            leader_ready_unix_ms: 0,
        })
        .expect("serialize test block broadcast")
    }

    fn test_block_data_at_number(
        parent_hash: B256,
        block_hash: B256,
        view: u64,
        block_number: u64,
    ) -> Vec<u8> {
        let payload_json = serde_json::to_vec(&test_execution_data_at_number(
            parent_hash,
            block_hash,
            block_number,
        ))
        .expect("serialize test execution data");
        bincode::serialize(&BlockDataBroadcast {
            block_hash,
            view,
            payload_json,
            timestamp: 1,
            execution_output: None,
            leader_ready_unix_ms: 0,
        })
        .expect("serialize test block broadcast")
    }

    fn test_commit_qc(view: u64, block_hash: B256) -> QuorumCertificate {
        let key = test_key(0x11);
        let message =
            n42_consensus::protocol::quorum::commit_signing_message(view, &block_hash, &B256::ZERO);
        let signature = key.sign(&message);
        let aggregate_signature = n42_primitives::bls::AggregateSignature::aggregate(&[&signature])
            .expect("single test signature aggregates");
        let mut signers = QuorumCertificate::genesis().signers;
        signers.push(true);
        QuorumCertificate {
            view,
            block_hash,
            aggregate_signature,
            signers,
        }
    }

    fn test_block_data_with_empty_execution(
        parent_hash: B256,
        block_hash: B256,
        view: u64,
    ) -> Vec<u8> {
        test_block_data_with_empty_execution_at_number(parent_hash, block_hash, view, 1)
    }

    fn test_block_data_with_empty_execution_at_number(
        parent_hash: B256,
        block_hash: B256,
        view: u64,
        block_number: u64,
    ) -> Vec<u8> {
        let payload_json = serde_json::to_vec(&test_execution_data_at_number(
            parent_hash,
            block_hash,
            block_number,
        ))
        .expect("serialize test execution data");
        let compact = CompactBlockExecution {
            bundle_state: Default::default(),
            receipts: Vec::new(),
            requests: Default::default(),
            gas_used: 0,
            blob_gas_used: 0,
            senders: Vec::new(),
        };
        let execution_json = serde_json::to_vec(&compact).expect("serialize compact execution");
        let execution_output =
            zstd::bulk::compress(&execution_json, 3).expect("compress compact execution");
        bincode::serialize(&BlockDataBroadcast {
            block_hash,
            view,
            payload_json,
            timestamp: 1,
            execution_output: Some(execution_output),
            leader_ready_unix_ms: 0,
        })
        .expect("serialize test block broadcast")
    }

    #[async_trait::async_trait]
    impl ConsensusNetwork for MockConsensusNetwork {
        fn broadcast_consensus(
            &self,
            msg: n42_primitives::ConsensusMessage,
        ) -> Result<(), n42_network::NetworkError> {
            self.broadcasts
                .lock()
                .expect("mock broadcasts lock")
                .push(msg);
            Ok(())
        }

        fn validator_peer(&self, index: u32) -> Option<n42_network::PeerId> {
            self.peers
                .read()
                .expect("mock peers lock")
                .get(&index)
                .copied()
        }

        fn send_direct(
            &self,
            peer: n42_network::PeerId,
            msg: n42_primitives::ConsensusMessage,
        ) -> Result<(), n42_network::NetworkError> {
            self.direct_messages
                .lock()
                .expect("mock direct messages lock")
                .push((peer, msg));
            Ok(())
        }

        fn all_validator_peers(&self) -> Vec<(u32, n42_network::PeerId)> {
            self.peers
                .read()
                .expect("mock peers lock")
                .iter()
                .map(|(index, peer_id)| (*index, *peer_id))
                .collect()
        }

        fn forward_tx_batch(
            &self,
            _peer: n42_network::PeerId,
            _txs: Vec<Vec<u8>>,
        ) -> Result<(), n42_network::NetworkError> {
            Ok(())
        }

        fn request_sync(
            &self,
            _peer: n42_network::PeerId,
            _request: n42_network::BlockSyncRequest,
        ) -> Result<(), n42_network::NetworkError> {
            Ok(())
        }

        fn broadcast_blob_sidecar(&self, _data: Vec<u8>) -> Result<(), n42_network::NetworkError> {
            Ok(())
        }

        async fn announce_block_reliable(
            &self,
            _data: Vec<u8>,
        ) -> Result<(), n42_network::NetworkError> {
            Ok(())
        }

        async fn send_block_direct_reliable(
            &self,
            _peer: n42_network::PeerId,
            _data: Vec<u8>,
        ) -> Result<(), n42_network::NetworkError> {
            Ok(())
        }

        async fn send_sync_response_reliable(
            &self,
            _request_id: u64,
            _response: n42_network::BlockSyncResponse,
        ) -> Result<(), n42_network::NetworkError> {
            Ok(())
        }

        async fn authenticate_validator_peer_reliable(
            &self,
            _peer_id: n42_network::PeerId,
            _validator_index: u32,
        ) -> Result<(), n42_network::NetworkError> {
            Ok(())
        }

        async fn replace_expected_validator_peers_reliable(
            &self,
            peers: HashMap<u32, n42_network::PeerId>,
        ) -> Result<(), n42_network::NetworkError> {
            *self.peers.write().expect("mock peers lock") = peers;
            Ok(())
        }

        async fn set_validator_context(&self, _my_index: u32, _validator_count: u32) {}
    }

    fn test_key(seed: u8) -> BlsSecretKey {
        BlsSecretKey::key_gen(&[seed; 32]).expect("deterministic test key should be valid")
    }

    fn make_test_engine_with_validator_count(
        count: u8,
        my_index: u8,
    ) -> (ConsensusEngine, mpsc::Receiver<EngineOutput>) {
        let fault_tolerance = u32::from(count.saturating_sub(1)) / 3;
        make_test_engine_with_fault_tolerance(count, my_index, fault_tolerance)
    }

    fn make_test_engine_with_fault_tolerance(
        count: u8,
        my_index: u8,
        fault_tolerance: u32,
    ) -> (ConsensusEngine, mpsc::Receiver<EngineOutput>) {
        assert!(my_index < count);
        let keys: Vec<_> = (0..count).map(test_key).collect();
        let validators = keys
            .iter()
            .enumerate()
            .map(|(idx, key)| ValidatorInfo {
                address: Address::with_last_byte(idx as u8),
                bls_public_key: key.public_key(),
                p2p_peer_id: None,
            })
            .collect::<Vec<_>>();
        let (output_tx, output_rx) = mpsc::channel(1024);

        let engine = ConsensusEngine::new(
            my_index as u32,
            keys[my_index as usize].clone(),
            ValidatorSet::new(&validators, fault_tolerance),
            60000,
            120000,
            output_tx,
        );
        (engine, output_rx)
    }

    fn make_test_engine() -> (ConsensusEngine, mpsc::Receiver<EngineOutput>) {
        let sk = test_key(0x11);
        let pk = sk.public_key();

        let validator_info = ValidatorInfo {
            address: Address::with_last_byte(1),
            bls_public_key: pk,
            p2p_peer_id: None,
        };

        let vs = ValidatorSet::new(&[validator_info], 0);
        let (output_tx, output_rx) = mpsc::channel(1024);

        let engine = ConsensusEngine::new(0, sk, vs, 60000, 120000, output_tx);
        (engine, output_rx)
    }

    fn make_test_engine_locked_on(
        locked_hash: B256,
    ) -> (ConsensusEngine, mpsc::Receiver<EngineOutput>) {
        let sk = test_key(0x21);
        let validator_info = ValidatorInfo {
            address: Address::with_last_byte(1),
            bls_public_key: sk.public_key(),
            p2p_peer_id: None,
        };
        let validator_set = ValidatorSet::new(&[validator_info], 0);
        let (output_tx, output_rx) = mpsc::channel(1024);
        let mut locked_qc = QuorumCertificate::genesis();
        locked_qc.view = 1;
        locked_qc.block_hash = locked_hash;
        let engine = ConsensusEngine::with_recovered_state(
            0,
            sk,
            EpochManager::new(validator_set),
            60_000,
            120_000,
            output_tx,
            2,
            locked_qc,
            QuorumCertificate::genesis(),
            0,
            0,
        );
        (engine, output_rx)
    }

    /// Returns (handle, normal_cmd_rx, priority_cmd_rx).
    fn make_test_network() -> (
        NetworkHandle,
        mpsc::Receiver<NetworkCommand>,
        mpsc::Receiver<NetworkCommand>,
    ) {
        let (cmd_tx, cmd_rx) = mpsc::channel(8);
        let (ptx, prx) = mpsc::channel(8);
        (NetworkHandle::new(cmd_tx, ptx), cmd_rx, prx)
    }

    #[test]
    fn test_orchestrator_construction() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let _ = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx);
    }

    #[test]
    fn test_engine_initial_state() {
        let (engine, _output_rx) = make_test_engine();
        assert_eq!(engine.current_view(), 1, "initial view should be 1");
        assert!(
            engine.is_current_leader(),
            "single validator should be leader"
        );
    }

    #[tokio::test]
    async fn test_handle_engine_output_broadcast() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, mut cmd_rx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx);

        let sk = test_key(0x12);
        let sig = sk.sign(b"test");
        let vote = Vote {
            view: 1,
            block_hash: B256::repeat_byte(0xAA),
            voter: 0,
            signature: sig,
        };

        orch.handle_engine_output(EngineOutput::BroadcastMessage(ConsensusMessage::Vote(vote)))
            .await;

        let cmd = cmd_rx.try_recv().expect("should receive a command");
        assert!(
            matches!(cmd, NetworkCommand::BroadcastConsensus(_)),
            "should be a BroadcastConsensus command"
        );
    }

    #[tokio::test]
    async fn test_timeout_rebroadcast_directly_reaches_reconnected_validators() {
        let (engine, output_rx) = make_test_engine_with_validator_count(3, 0);
        let peers = (0..3u32)
            .map(|idx| {
                let keypair = Keypair::generate_ed25519();
                (idx, keypair.public().to_peer_id())
            })
            .collect::<Vec<_>>();
        let network = Arc::new(MockConsensusNetwork::with_peers(peers.clone()));
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let mut orch = ConsensusService::new(engine, network.clone(), net_event_rx, output_rx);

        let view = 1;
        let timeout = TimeoutMessage {
            view,
            high_qc: QuorumCertificate::genesis(),
            sender: 0,
            signature: test_key(0).sign(&n42_consensus::protocol::quorum::timeout_signing_message(
                view,
            )),
        };

        orch.handle_engine_output(EngineOutput::BroadcastMessage(ConsensusMessage::Timeout(
            timeout,
        )))
        .await;

        let direct = network.direct_messages();
        assert_eq!(
            direct.len(),
            2,
            "timeout must use direct delivery to every peer except self"
        );
        assert!(
            direct
                .iter()
                .all(|(_, msg)| matches!(msg, ConsensusMessage::Timeout(_)))
        );
        assert!(direct.iter().all(|(peer, _)| *peer != peers[0].1));
        assert_eq!(
            network.broadcasts().len(),
            1,
            "GossipSub remains the fanout fallback"
        );
    }

    #[tokio::test]
    async fn test_consensus_event_batch_forms_qc_and_drops_bad_vote() {
        let (mut engine, output_rx) = make_test_engine_with_validator_count(4, 1);
        let block_hash = B256::repeat_byte(0xB7);
        engine
            .process_event(n42_consensus::ConsensusEvent::BlockReady(block_hash, None))
            .expect("leader should initialize the vote collector");

        let peers = (0..4u32)
            .map(|idx| {
                let keypair = Keypair::generate_ed25519();
                (idx, keypair.public().to_peer_id())
            })
            .collect::<Vec<_>>();
        let network = Arc::new(MockConsensusNetwork::with_peers(peers.clone()));
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let mut orch = ConsensusService::new(engine, network, net_event_rx, output_rx);
        let signing_message = n42_consensus::protocol::quorum::signing_message(1, &block_hash);

        let events = [(0u32, 0u8), (2, 2), (3, 0)]
            .into_iter()
            .map(|(voter, signing_key)| NetworkEvent::ConsensusMessage {
                source: peers[voter as usize].1,
                message: Box::new(ConsensusMessage::Vote(Vote {
                    view: 1,
                    block_hash,
                    voter,
                    signature: test_key(signing_key).sign(&signing_message),
                })),
            })
            .collect();

        orch.handle_consensus_event_batch(events).await;

        assert!(
            std::iter::from_fn(|| orch.output_rx.try_recv().ok()).any(|output| matches!(
                output,
                EngineOutput::BroadcastMessage(ConsensusMessage::PrepareQC(_))
            ))
        );
    }

    #[tokio::test]
    async fn test_handle_engine_output_send_to_validator() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, mut cmd_rx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx);

        let sk = test_key(0x13);
        let sig = sk.sign(b"test");
        let vote = Vote {
            view: 1,
            block_hash: B256::repeat_byte(0xBB),
            voter: 0,
            signature: sig,
        };

        orch.handle_engine_output(EngineOutput::SendToValidator(
            0,
            ConsensusMessage::Vote(vote),
        ))
        .await;

        let cmd = cmd_rx.try_recv().expect("should receive a command");
        assert!(
            matches!(cmd, NetworkCommand::BroadcastConsensus(_)),
            "SendToValidator should fallback to BroadcastConsensus"
        );
    }

    #[tokio::test]
    async fn test_current_view_vote_is_resent_directly_until_view_changes() {
        let (engine, output_rx) = make_test_engine_with_validator_count(3, 0);
        let collector = n42_network::PeerId::random();
        let network = MockConsensusNetwork::with_peers(vec![(1, collector)]);
        let network_probe = network.clone();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx);

        let vote = Vote {
            view: 1,
            block_hash: B256::repeat_byte(0xBC),
            voter: 0,
            signature: test_key(0).sign(b"vote-resend"),
        };
        orch.handle_engine_output(EngineOutput::SendToValidator(
            1,
            ConsensusMessage::Vote(vote),
        ))
        .await;

        assert_eq!(network_probe.direct_messages().len(), 1);
        assert_eq!(network_probe.broadcasts().len(), 1);
        orch.resend_pending_vote();
        assert_eq!(
            network_probe.direct_messages().len(),
            2,
            "the collector should receive the exact signed vote again"
        );
        assert_eq!(
            network_probe.broadcasts().len(),
            1,
            "successful direct resend should not add gossip traffic"
        );

        orch.pending_vote_resend
            .as_mut()
            .expect("vote should remain pending")
            .view = 0;
        orch.resend_pending_vote();
        assert!(orch.pending_vote_resend.is_none());
        assert_eq!(network_probe.direct_messages().len(), 2);
    }

    #[tokio::test]
    async fn test_handle_engine_output_execute_block() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx);
        orch.handle_engine_output(EngineOutput::ExecuteBlock(B256::repeat_byte(0xCC)))
            .await;
    }

    #[tokio::test]
    async fn test_commit_without_execution_confirmation_does_not_advance_head() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx);

        let commit_qc = QuorumCertificate::genesis();
        orch.handle_engine_output(EngineOutput::BlockCommitted {
            view: 1,
            block_hash: B256::repeat_byte(0xDD),
            commit_qc,
            validator_changes: None,
        })
        .await;

        assert_eq!(
            orch.head_block_hash,
            B256::ZERO,
            "agreement alone must not advance the execution head"
        );
        assert_eq!(orch.committed_block_count, 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn committed_invalid_payload_preserves_validated_head_and_build_parent() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);

        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let previous_head = B256::repeat_byte(0x11);
        let bad_hash = B256::repeat_byte(0xDD);
        let view = 1;
        let mock_el = Arc::new(MockExecutionLayer::new(
            PayloadStatusEnum::Invalid {
                validation_error: "stale nonce".to_string(),
            },
            PayloadStatusEnum::Syncing,
        ));
        let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx);
        orch.el = Some(mock_el.clone());
        orch.head_block_hash = previous_head;
        orch.async_finalize_fcu = false;
        orch.pending_block_data
            .insert(bad_hash, test_block_data(previous_head, bad_hash, view));

        orch.handle_engine_output(EngineOutput::BlockCommitted {
            view,
            block_hash: bad_hash,
            commit_qc: QuorumCertificate::genesis(),
            validator_changes: None,
        })
        .await;

        assert_eq!(orch.committed_block_count, 1, "QC agreement still advances");
        assert_eq!(orch.head_block_hash, previous_head);

        let (hash, imported_view, outcome, block_ts) =
            tokio::time::timeout(Duration::from_secs(2), orch.import_done_rx.recv())
                .await
                .expect("background import should finish")
                .expect("background import channel should remain open");
        assert_eq!((hash, imported_view), (bad_hash, view));
        assert_eq!(
            outcome,
            ImportOutcome::Invalid,
            "mock new_payload Invalid must reject the block"
        );
        orch.handle_import_done(hash, imported_view, outcome, block_ts)
            .await;

        assert_eq!(
            orch.head_block_hash, previous_head,
            "committed invalid block must not pin the local execution head"
        );
        let metric_value =
            snapshotter
                .snapshot()
                .into_vec()
                .into_iter()
                .find_map(|(key, _, _, value)| {
                    (key.key().name() == "n42_committed_block_unexecutable_total").then_some(value)
                });
        assert_eq!(metric_value, Some(DebugValue::Counter(1)));

        orch.do_trigger_payload_build(None).await;
        assert_eq!(mock_el.build_parents(), vec![previous_head]);
        assert!(
            !mock_el.build_parents().contains(&bad_hash),
            "no payload build may use the unexecutable committed hash as parent"
        );
    }

    /// S3 acceptance: when consensus is locked on A but reth's last validated
    /// head is the same-height sibling B, the leader must request a payload on
    /// A. A sibling B must not be advertised as A's safe/finalized ancestor.
    #[tokio::test]
    async fn leader_build_uses_locked_qc_parent_over_local_execution_sibling() {
        let locked_parent = B256::repeat_byte(0xA1);
        let execution_sibling = B256::repeat_byte(0xB2);
        let (engine, output_rx) = make_test_engine_locked_on(locked_parent);
        let (network, _cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let mock_el = Arc::new(MockExecutionLayer::new(
            PayloadStatusEnum::Valid,
            PayloadStatusEnum::Valid,
        ));
        let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx);
        orch.el = Some(mock_el.clone());
        orch.head_block_hash = execution_sibling;

        orch.do_trigger_payload_build(None).await;

        assert_eq!(mock_el.build_parents(), vec![locked_parent]);
        assert_eq!(
            mock_el.build_forkchoices(),
            vec![(locked_parent, B256::ZERO, B256::ZERO)],
            "LockedQC must drive FCU head without claiming sibling B as its safe/finalized ancestor"
        );
    }

    #[test]
    fn payload_ready_is_bound_to_captured_locked_qc_context() {
        let locked_parent = B256::repeat_byte(0xA1);
        let wrong_parent = B256::repeat_byte(0xB2);
        let child = B256::repeat_byte(0xC3);
        let (engine, output_rx) = make_test_engine_locked_on(locked_parent);
        let (network, _cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx);
        let stale_context = PayloadBuildContext {
            view: 2,
            parent_hash: wrong_parent,
        };
        orch.building_on_parent = Some(stale_context);
        orch.build_triggered_at = Some(Instant::now());

        assert!(
            !orch.handle_payload_build_ready(PayloadBuildReady {
                context: stale_context,
                block_hash: child,
            }),
            "a payload from a sibling parent must not enter consensus"
        );
        assert!(
            orch.output_rx.try_recv().is_err(),
            "no proposal may be emitted"
        );
        assert!(
            orch.next_build_at.is_some(),
            "the leader must retry on LockedQC"
        );

        let locked_context = PayloadBuildContext {
            view: 2,
            parent_hash: locked_parent,
        };
        orch.building_on_parent = Some(locked_context);
        orch.build_triggered_at = Some(Instant::now());
        assert!(orch.handle_payload_build_ready(PayloadBuildReady {
            context: locked_context,
            block_hash: child,
        }));

        let proposal = match orch.output_rx.try_recv().expect("proposal output") {
            EngineOutput::BroadcastMessage(ConsensusMessage::Proposal(proposal)) => proposal,
            output => panic!("expected Proposal, got {output:?}"),
        };
        assert_eq!(proposal.view, locked_context.view);
        assert_eq!(proposal.block_hash, child);
        assert_eq!(proposal.justify_qc.block_hash, locked_parent);
    }

    #[test]
    fn stale_build_completion_cannot_clear_new_view_guard() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx);
        let old_context = PayloadBuildContext {
            view: 7,
            parent_hash: B256::repeat_byte(0x71),
        };
        let new_context = PayloadBuildContext {
            view: 8,
            parent_hash: B256::repeat_byte(0x82),
        };
        orch.building_on_parent = Some(new_context);
        orch.build_triggered_at = Some(Instant::now());

        assert!(!orch.handle_payload_build_complete(old_context));
        assert_eq!(orch.building_on_parent, Some(new_context));
        assert!(orch.build_triggered_at.is_some());
        assert!(orch.handle_payload_build_complete(new_context));
        assert!(orch.building_on_parent.is_none());
        assert!(orch.build_triggered_at.is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn committed_syncing_payload_requests_missing_execution_ancestors() {
        let (engine, output_rx) = make_test_engine();
        let (network, mut cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let peer = n42_network::PeerId::random();
        let previous_head = B256::repeat_byte(0x11);
        let waiting_hash = B256::repeat_byte(0x22);
        let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx);
        orch.connected_peers.insert(peer);
        orch.head_block_hash = previous_head;
        orch.committed_blocks.push_back(CommittedBlock {
            view: 1,
            block_hash: waiting_hash,
            commit_qc: QuorumCertificate::genesis(),
            payload: Vec::new(),
            validator_changes: None,
            execution_lineage: Vec::new(),
        });
        orch.pending_sidecar_diffs.insert(
            1,
            (
                waiting_hash,
                n42_execution::state_diff::StateDiff::default(),
            ),
        );

        orch.handle_import_done(waiting_hash, 1, ImportOutcome::Syncing, 0)
            .await;

        assert_eq!(orch.head_block_hash, previous_head);
        assert!(
            orch.pending_sidecar_diffs.contains_key(&1),
            "a retryable missing-parent result must preserve the staged sidecar diff"
        );
        let command = tokio::time::timeout(Duration::from_secs(1), cmd_rx.recv())
            .await
            .expect("catch-up sync command should be sent")
            .expect("network command channel should remain open");
        match command {
            NetworkCommand::RequestSync {
                peer: target,
                request,
            } => {
                assert_eq!(target, peer);
                assert_eq!(request.from_view, 1);
                assert_eq!(request.to_view, 1);
                assert_eq!(request.local_committed_view, 0);
            }
            other => panic!("unexpected network command: {other:?}"),
        }
    }

    #[tokio::test]
    async fn committed_valid_fcu_advances_execution_head() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let previous_head = B256::repeat_byte(0x22);
        let valid_hash = B256::repeat_byte(0xEE);
        let mock_el = Arc::new(MockExecutionLayer::new(
            PayloadStatusEnum::Valid,
            PayloadStatusEnum::Valid,
        ));
        let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx);
        orch.el = Some(mock_el);
        orch.head_block_hash = previous_head;
        orch.async_finalize_fcu = false;

        orch.handle_engine_output(EngineOutput::BlockCommitted {
            view: 1,
            block_hash: valid_hash,
            commit_qc: QuorumCertificate::genesis(),
            validator_changes: None,
        })
        .await;

        assert_eq!(orch.committed_block_count, 1);
        assert_eq!(orch.head_block_hash, valid_hash);
        assert_eq!(orch.execution_validated_head_view, 1);
    }

    #[tokio::test]
    async fn eager_valid_before_commit_advances_head_only_after_commit() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let previous_head = B256::repeat_byte(0x33);
        let eager_hash = B256::repeat_byte(0xEF);
        let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx);
        orch.head_block_hash = previous_head;

        orch.handle_eager_import_done(eager_hash, 1).await;
        assert_eq!(
            orch.head_block_hash, previous_head,
            "speculative eager validity must not move head before commit"
        );

        orch.handle_engine_output(EngineOutput::BlockCommitted {
            view: 1,
            block_hash: eager_hash,
            commit_qc: QuorumCertificate::genesis(),
            validator_changes: None,
        })
        .await;

        assert_eq!(orch.head_block_hash, eager_hash);
        assert_eq!(orch.execution_validated_head_view, 1);
        assert!(orch.eager_execution_validated.is_empty());
    }

    #[tokio::test]
    async fn queued_eager_validity_drained_during_finalize_advances_head() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let previous_head = B256::repeat_byte(0x34);
        let eager_hash = B256::repeat_byte(0xF0);
        let mock_el = Arc::new(MockExecutionLayer::with_fcu_statuses(
            PayloadStatusEnum::Valid,
            [PayloadStatusEnum::Syncing, PayloadStatusEnum::Valid],
        ));
        let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx);
        orch.el = Some(mock_el);
        orch.head_block_hash = previous_head;
        orch.async_finalize_fcu = false;
        orch.eager_import_done_tx
            .send((eager_hash, 1))
            .await
            .expect("queue eager completion");

        orch.handle_engine_output(EngineOutput::BlockCommitted {
            view: 1,
            block_hash: eager_hash,
            commit_qc: QuorumCertificate::genesis(),
            validator_changes: None,
        })
        .await;

        assert_eq!(orch.head_block_hash, eager_hash);
        assert_eq!(orch.execution_validated_head_view, 1);
        assert!(orch.eager_execution_validated.is_empty());
    }

    #[test]
    fn stale_execution_completion_cannot_regress_head() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx);
        let newer_hash = B256::repeat_byte(0x44);

        orch.advance_execution_validated_head(2, newer_hash, "test newer completion");
        orch.advance_execution_validated_head(1, B256::repeat_byte(0x45), "test stale completion");

        assert_eq!(orch.head_block_hash, newer_hash);
        assert_eq!(orch.execution_validated_head_view, 2);
    }

    #[tokio::test]
    async fn test_late_recovered_validator_changes_patch_sync_cache() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx);

        let block_hash = B256::repeat_byte(0xA5);
        orch.store_committed_block(15, block_hash, QuorumCertificate::genesis(), None);

        orch.handle_engine_output(EngineOutput::CommittedBlockValidatorChangesRecovered {
            view: 15,
            block_hash,
            validator_changes: vec![ValidatorChange::Add {
                address: Address::with_last_byte(0x04),
                bls_public_key: test_key(0x44).public_key(),
                p2p_peer_id: None,
            }],
        })
        .await;

        assert_eq!(orch.committed_blocks.len(), 1);
        assert!(orch.committed_blocks[0].validator_changes.is_some());
        assert_eq!(
            orch.committed_blocks[0]
                .validator_changes
                .as_ref()
                .expect("validator changes should be patched")
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn test_handle_engine_output_view_changed() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx);
        orch.handle_engine_output(EngineOutput::ViewChanged { new_view: 5 })
            .await;
    }

    #[test]
    fn attaching_epoch_schedule_pre_stages_first_rotation() {
        let current_key = test_key(0x31);
        let current = ValidatorInfo {
            address: Address::with_last_byte(0x31),
            bls_public_key: current_key.public_key(),
            p2p_peer_id: None,
        };
        let next_validators = vec![
            current.clone(),
            ValidatorInfo {
                address: Address::with_last_byte(0x32),
                bls_public_key: test_key(0x32).public_key(),
                p2p_peer_id: None,
            },
        ];
        let dir = tempfile::tempdir().unwrap();
        let schedule_path = dir.path().join("epoch_schedule.json");
        let json = serde_json::json!([{
            "start_epoch": 1,
            "validators": next_validators,
            "fault_tolerance": 0,
        }]);
        std::fs::write(&schedule_path, serde_json::to_vec(&json).unwrap()).unwrap();
        let schedule = EpochSchedule::load(&schedule_path)
            .unwrap()
            .expect("schedule should load");

        let (output_tx, output_rx) = mpsc::channel(64);
        let engine = ConsensusEngine::with_epoch_manager(
            0,
            current_key,
            EpochManager::with_epoch_length(ValidatorSet::new(&[current], 0), 30),
            60_000,
            120_000,
            output_tx,
        );
        let (network, _cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx)
            .with_epoch_schedule(schedule);

        assert!(orch.engine.epoch_manager().has_staged_next());
        assert_eq!(
            orch.engine
                .epoch_manager()
                .peek_next_set()
                .expect("epoch 1 should be staged")
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn test_epoch_transition_refreshes_expected_validator_peers() {
        let (engine, output_rx) = make_test_engine();
        let (network, mut cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx);

        let peer0 = Keypair::generate_ed25519().public().to_peer_id();
        let peer1 = Keypair::generate_ed25519().public().to_peer_id();
        let next_validators = vec![
            ValidatorInfo {
                address: Address::with_last_byte(0x10),
                bls_public_key: test_key(0x20).public_key(),
                p2p_peer_id: Some(peer0.to_string()),
            },
            ValidatorInfo {
                address: Address::with_last_byte(0x11),
                bls_public_key: test_key(0x21).public_key(),
                p2p_peer_id: Some(peer1.to_string()),
            },
        ];
        orch.engine
            .epoch_manager_mut()
            .stage_next_epoch(&next_validators, 0)
            .unwrap();
        assert!(orch.engine.epoch_manager_mut().advance_epoch());

        orch.handle_engine_output(EngineOutput::EpochTransition {
            new_epoch: 1,
            validator_count: 2,
        })
        .await;

        match cmd_rx.try_recv().expect("expected network refresh command") {
            NetworkCommand::ReplaceExpectedValidatorPeers { peers } => {
                assert_eq!(peers.get(&0), Some(&peer0));
                assert_eq!(peers.get(&1), Some(&peer1));
            }
            other => panic!("expected ReplaceExpectedValidatorPeers, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_epoch_transition_clears_expected_validator_peers_when_strict_binding_missing() {
        let (engine, output_rx) = make_test_engine();
        let (network, mut cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx)
            .with_allow_deterministic_validator_peers(false);

        let next_validators = vec![
            ValidatorInfo {
                address: Address::with_last_byte(0x20),
                bls_public_key: test_key(0x22).public_key(),
                p2p_peer_id: None,
            },
            ValidatorInfo {
                address: Address::with_last_byte(0x21),
                bls_public_key: test_key(0x23).public_key(),
                p2p_peer_id: None,
            },
        ];
        orch.engine
            .epoch_manager_mut()
            .stage_next_epoch(&next_validators, 0)
            .unwrap();
        assert!(orch.engine.epoch_manager_mut().advance_epoch());

        orch.handle_engine_output(EngineOutput::EpochTransition {
            new_epoch: 1,
            validator_count: 2,
        })
        .await;

        match cmd_rx.try_recv().expect("expected network refresh command") {
            NetworkCommand::ReplaceExpectedValidatorPeers { peers } => {
                assert!(peers.is_empty());
            }
            other => panic!("expected ReplaceExpectedValidatorPeers, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_orchestrator_exits_on_net_event_channel_close() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, _prx) = make_test_network();
        let (net_event_tx, net_event_rx) = mpsc::channel::<NetworkEvent>(8192);
        let orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx);
        drop(net_event_tx);

        let result = tokio::time::timeout(Duration::from_secs(5), orch.run()).await;
        assert!(
            result.is_ok(),
            "orchestrator should exit when network event channel is closed"
        );
    }

    #[tokio::test]
    async fn test_orchestrator_processes_peer_events() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, _prx) = make_test_network();
        let (net_event_tx, net_event_rx) = mpsc::channel::<NetworkEvent>(8192);
        let orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx);

        let peer_id = libp2p::PeerId::random();
        net_event_tx
            .try_send(NetworkEvent::PeerConnected(peer_id))
            .unwrap();
        drop(net_event_tx);

        let result = tokio::time::timeout(Duration::from_secs(5), orch.run()).await;
        assert!(
            result.is_ok(),
            "orchestrator should exit after processing events"
        );
    }

    fn make_test_orchestrator_with_state(
        state: Option<Arc<SharedConsensusState>>,
    ) -> ConsensusService {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx);
        orch.consensus_state = state;
        orch
    }

    #[test]
    fn test_cache_pending_block_data_preserves_protected_hash_when_full() {
        let mut orch = make_test_orchestrator_with_state(None);
        let protected_hash = B256::repeat_byte(0x00);
        for byte in 0..super::execution_bridge::MAX_PENDING_BLOCK_DATA {
            let hash = B256::repeat_byte(byte as u8);
            orch.pending_block_data.insert(hash, vec![byte as u8]);
        }

        let incoming_hash = B256::repeat_byte(0xff);
        assert!(orch.cache_pending_block_data(incoming_hash, vec![0xff], &[protected_hash]));

        assert_eq!(
            orch.pending_block_data.len(),
            super::execution_bridge::MAX_PENDING_BLOCK_DATA
        );
        assert!(orch.pending_block_data.contains_key(&protected_hash));
        assert!(orch.pending_block_data.contains_key(&incoming_hash));
        assert!(
            !orch
                .pending_block_data
                .contains_key(&B256::repeat_byte(0x01))
        );
    }

    #[test]
    fn test_leader_payload_drain_preserves_finalizing_hash_when_full() {
        let mut orch = make_test_orchestrator_with_state(None);
        for byte in 1..=super::execution_bridge::MAX_PENDING_BLOCK_DATA {
            let hash = B256::repeat_byte(byte as u8);
            orch.pending_block_data.insert(hash, vec![byte as u8]);
        }

        let finalizing_hash = B256::repeat_byte(0x00);
        let later_hash = B256::repeat_byte(0xff);
        orch.leader_payload_tx
            .try_send((finalizing_hash, b"target".to_vec()))
            .expect("leader payload channel should accept target");
        orch.leader_payload_tx
            .try_send((later_hash, b"later".to_vec()))
            .expect("leader payload channel should accept later payload");

        orch.drain_leader_payload_rx(&[finalizing_hash]);

        assert_eq!(
            orch.pending_block_data
                .get(&finalizing_hash)
                .map(Vec::as_slice),
            Some(&b"target"[..])
        );
        assert_eq!(
            orch.pending_block_data.len(),
            super::execution_bridge::MAX_PENDING_BLOCK_DATA
        );
        assert!(orch.pending_block_data.contains_key(&later_hash));
        assert!(
            !orch
                .pending_block_data
                .contains_key(&B256::repeat_byte(0x01))
        );
    }

    #[test]
    fn leader_payload_drain_backfills_committed_sync_payload() {
        let mut orch = make_test_orchestrator_with_state(None);
        let parent_hash = B256::repeat_byte(0x31);
        let block_hash = B256::repeat_byte(0x32);
        let data = test_block_data(parent_hash, block_hash, 2);
        let expected_payload = bincode::deserialize::<BlockDataBroadcast>(&data)
            .expect("test broadcast decodes")
            .payload_json;
        orch.committed_blocks.push_back(CommittedBlock {
            view: 2,
            block_hash,
            commit_qc: QuorumCertificate::genesis(),
            payload: Vec::new(),
            validator_changes: None,
            execution_lineage: Vec::new(),
        });
        orch.leader_payload_tx
            .try_send((block_hash, data))
            .expect("leader payload channel should accept block");

        orch.drain_leader_payload_rx(&[block_hash]);

        assert_eq!(orch.committed_blocks[0].payload, expected_payload);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn committed_child_uses_execution_number_when_ancestor_payload_is_missing() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let durable_head = B256::repeat_byte(0x53);
        let missing_parent = B256::repeat_byte(0x54);
        let committed_hash = B256::repeat_byte(0x55);
        let data = test_block_data_at_number(missing_parent, committed_hash, 145, 145);
        let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx);
        orch.head_block_hash = durable_head;
        orch.committed_block_count = 143;
        assert!(orch.cache_pending_block_data(committed_hash, data, &[committed_hash]));

        orch.handle_block_committed(
            145,
            committed_hash,
            test_commit_qc(145, committed_hash),
            None,
        )
        .await;

        assert_eq!(
            orch.committed_block_count, 145,
            "the child payload reveals the real execution height even before its parent is recovered"
        );
        assert_eq!(orch.head_block_hash, durable_head);
        assert_eq!(orch.committed_blocks.len(), 1);
        assert_eq!(
            orch.committed_blocks[0].execution_lineage.len(),
            0,
            "an incomplete raw ancestry is not retained as a trusted recovery lineage"
        );
    }

    #[tokio::test]
    async fn test_block_committed_updates_shared_state() {
        let vs = ValidatorSet::new(&[], 0);
        let state = Arc::new(SharedConsensusState::new(vs));
        let mut orch = make_test_orchestrator_with_state(Some(state.clone()));

        assert!(
            state.load_committed_qc().is_none(),
            "should start with no QC"
        );

        let commit_qc = QuorumCertificate::genesis();
        orch.handle_engine_output(EngineOutput::BlockCommitted {
            view: 1,
            block_hash: B256::repeat_byte(0xEE),
            commit_qc,
            validator_changes: None,
        })
        .await;

        assert!(
            state.load_committed_qc().is_some(),
            "should have QC after commit"
        );
    }

    #[tokio::test]
    async fn test_block_ready_channel() {
        let mut orch = make_test_orchestrator_with_state(None);
        let block_ready_tx = orch.block_ready_tx.clone();

        let test_hash = B256::repeat_byte(0xFF);
        let ready = PayloadBuildReady {
            context: PayloadBuildContext {
                view: 7,
                parent_hash: B256::repeat_byte(0xEE),
            },
            block_hash: test_hash,
        };
        block_ready_tx.send(ready).await.unwrap();

        let received = orch.block_ready_rx.try_recv();
        assert!(received.is_ok(), "should receive BlockReady context");
        assert_eq!(received.unwrap(), ready);
    }

    #[test]
    fn test_restore_failed_tx_forward_batch_preserves_recent_suffix() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx);

        let txs: Vec<Vec<u8>> = (0..5000)
            .map(|i| vec![(i & 0xff) as u8, ((i >> 8) & 0xff) as u8])
            .collect();
        let expected_tail = txs[2952..].to_vec();

        orch.restore_failed_tx_forward_batch(txs, 1);

        assert_eq!(orch.tx_forward_buffer.len(), 2048);
        assert_eq!(orch.tx_forward_buffer, expected_tail);
    }

    #[tokio::test]
    async fn test_sync_timeout_resets_in_flight() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx);

        orch.connected_peers.insert(libp2p::PeerId::random());
        orch.sync_in_flight = true;
        orch.sync_started_at =
            Some(Instant::now() - state_mgmt::sync_request_timeout() - Duration::from_secs(1));

        orch.initiate_sync(1, 10);

        assert!(orch.sync_in_flight, "new sync request should be in flight");
        let started = orch.sync_started_at.expect("sync_started_at should be set");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "sync_started_at should be recent after timeout reset"
        );
    }

    #[tokio::test]
    async fn expired_state_sync_yields_to_execution_catchup_fanout() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx);
        let peers = [libp2p::PeerId::random(), libp2p::PeerId::random()];
        orch.connected_peers.extend(peers);

        orch.sync_in_flight = true;
        orch.sync_started_at =
            Some(Instant::now() - state_mgmt::sync_request_timeout() - Duration::from_secs(1));
        orch.sync_request_range = Some((88, 89));
        orch.sync_requested_peers.insert(peers[0]);

        orch.initiate_execution_catchup_sync(86, 116);

        assert!(orch.sync_in_flight, "catch-up request should be in flight");
        assert_eq!(orch.sync_request_range, Some((87, 116)));
        assert_eq!(
            orch.sync_requested_peers,
            peers.into_iter().collect(),
            "execution catch-up must replace the stale request and fan out"
        );
        let started = orch.sync_started_at.expect("sync_started_at should be set");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "sync_started_at should belong to the replacement request"
        );
    }

    #[test]
    fn test_prev_randao_from_commit_qc() {
        use alloy_primitives::keccak256;

        // No commit QC → B256::ZERO
        let none_qc: Option<&QuorumCertificate> = None;
        let randao = none_qc
            .map(|qc| keccak256(qc.aggregate_signature.to_bytes()))
            .unwrap_or(B256::ZERO);
        assert_eq!(randao, B256::ZERO);

        // With commit QC → deterministic non-zero
        let qc = QuorumCertificate::genesis();
        let r1 = keccak256(qc.aggregate_signature.to_bytes());
        let r2 = keccak256(qc.aggregate_signature.to_bytes());
        assert_eq!(r1, r2, "must be deterministic");
    }

    #[test]
    fn test_with_recovered_commit_qc_restores_prev_randao() {
        use alloy_primitives::keccak256;

        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx);

        // Before recovery: prev_randao_cache is ZERO
        assert_eq!(orch.prev_randao_cache, B256::ZERO);

        // After recovery: prev_randao_cache matches QC hash
        let qc = QuorumCertificate::genesis();
        let expected = keccak256(qc.aggregate_signature.to_bytes());
        let orch = orch.with_recovered_commit_qc(qc);
        assert_eq!(orch.prev_randao_cache, expected);
        assert!(orch.last_commit_qc.is_some());
    }

    #[test]
    fn test_connected_validator_peer_count() {
        let (engine, output_rx) = make_test_engine_with_validator_count(2, 0);
        let mock_peers = vec![
            (0u32, Keypair::generate_ed25519().public().to_peer_id()),
            (1u32, Keypair::generate_ed25519().public().to_peer_id()),
        ];
        let network = Arc::new(MockConsensusNetwork::with_peers(mock_peers.clone()));
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let mut orch = ConsensusService::new(engine, network, net_event_rx, output_rx);

        let non_validator = Keypair::generate_ed25519().public().to_peer_id();
        orch.connected_peers.insert(non_validator);
        orch.connected_peers.insert(mock_peers[0].1);
        orch.connected_peers.insert(mock_peers[1].1);

        assert_eq!(orch.connected_validator_peer_count(), 2);
    }

    #[test]
    fn test_has_quorum_peers() {
        let (engine, output_rx) = make_test_engine_with_validator_count(7, 1);
        let mock_peers = (0..7)
            .map(|idx| {
                (
                    idx as u32,
                    Keypair::generate_ed25519().public().to_peer_id(),
                )
            })
            .collect::<Vec<_>>();
        let network = Arc::new(MockConsensusNetwork::with_peers(mock_peers.clone()));
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let mut orch = ConsensusService::new(engine, network, net_event_rx, output_rx);

        orch.connected_peers.insert(mock_peers[0].1);
        orch.connected_peers.insert(mock_peers[1].1);
        orch.connected_peers.insert(mock_peers[2].1);

        assert!(!orch.has_quorum_peers());
        orch.connected_peers.insert(mock_peers[4].1);
        assert!(orch.has_quorum_peers());
    }

    #[test]
    fn test_quorum_peers_needed_uses_engine_quorum() {
        let (engine, output_rx) = make_test_engine_with_fault_tolerance(7, 1, 1);
        let mock_peers = (0..7)
            .map(|idx| {
                (
                    idx as u32,
                    Keypair::generate_ed25519().public().to_peer_id(),
                )
            })
            .collect::<Vec<_>>();
        let network = Arc::new(MockConsensusNetwork::with_peers(mock_peers.clone()));
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let mut orch = ConsensusService::new(engine, network, net_event_rx, output_rx);

        assert_eq!(orch.quorum_peers_needed(), 5);
        for (_, peer_id) in mock_peers.iter().take(4) {
            orch.connected_peers.insert(*peer_id);
        }
        assert!(!orch.has_quorum_peers());
        orch.connected_peers.insert(mock_peers[4].1);
        assert!(orch.has_quorum_peers());
    }

    #[test]
    fn test_quorum_peers_needed_single_validator() {
        let (engine, output_rx) = make_test_engine_with_validator_count(1, 0);
        let network = Arc::new(MockConsensusNetwork::default());
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let orch = ConsensusService::new(engine, network, net_event_rx, output_rx);
        assert_eq!(orch.quorum_peers_needed(), 0);
        assert!(orch.has_quorum_peers());
    }

    #[test]
    fn test_quorum_peers_needed_zero_fault_multi_validator() {
        let (engine, output_rx) = make_test_engine_with_fault_tolerance(3, 1, 0);
        let network = Arc::new(MockConsensusNetwork::default());
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let orch = ConsensusService::new(engine, network, net_event_rx, output_rx);
        assert_eq!(orch.quorum_peers_needed(), 2);
        assert!(!orch.has_quorum_peers());
    }

    #[tokio::test]
    async fn test_startup_leader_gate_waits_for_validator_quorum() {
        let (engine, output_rx) = make_test_engine_with_validator_count(7, 1);
        let mock_peers = (0..7)
            .map(|idx| {
                (
                    idx as u32,
                    Keypair::generate_ed25519().public().to_peer_id(),
                )
            })
            .collect::<Vec<_>>();
        let network = Arc::new(MockConsensusNetwork::with_peers(mock_peers.clone()));
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let mut orch = ConsensusService::new(engine, network, net_event_rx, output_rx);

        orch.begin_leader_build_wait(LeaderBuildWaitMode::Direct, None);

        for idx in 0..3 {
            orch.handle_network_event(NetworkEvent::PeerConnected(mock_peers[idx as usize].1))
                .await;
            assert!(orch.leader_build_waiting_view.is_some());
        }
        orch.handle_network_event(NetworkEvent::PeerConnected(mock_peers[3].1))
            .await;
        assert!(orch.leader_build_waiting_view.is_none());
    }

    #[tokio::test]
    async fn test_startup_leader_gate_honors_warmup_floor_after_quorum() {
        let (engine, output_rx) = make_test_engine_with_validator_count(7, 1);
        let mock_peers = (0..7)
            .map(|idx| {
                (
                    idx as u32,
                    Keypair::generate_ed25519().public().to_peer_id(),
                )
            })
            .collect::<Vec<_>>();
        let network = Arc::new(MockConsensusNetwork::with_peers(mock_peers.clone()));
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let mut orch = ConsensusService::new(engine, network, net_event_rx, output_rx);
        let view = orch.engine.current_view();

        orch.begin_leader_build_wait(LeaderBuildWaitMode::Direct, None);
        orch.leader_build_not_before = Some((view, Instant::now() + Duration::from_secs(60)));

        for (_, peer_id) in mock_peers.iter().take(4) {
            orch.handle_network_event(NetworkEvent::PeerConnected(*peer_id))
                .await;
        }

        assert_eq!(
            orch.leader_build_waiting_view,
            Some((view, LeaderBuildWaitMode::Direct))
        );
        assert!(orch.leader_build_not_before.is_some());
        assert!(orch.next_build_at.is_some());

        orch.leader_build_not_before = Some((view, Instant::now()));
        orch.evaluate_leader_build_wait(None).await;

        assert!(orch.leader_build_waiting_view.is_none());
        assert!(orch.leader_build_not_before.is_none());
    }

    #[test]
    fn test_leader_build_timer_rearms_direct_wait_after_scheduled_gate_clears() {
        let (engine, output_rx) = make_test_engine_with_validator_count(7, 1);
        let network = Arc::new(MockConsensusNetwork::default());
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let mut orch = ConsensusService::new(engine, network, net_event_rx, output_rx);

        assert!(orch.leader_build_waiting_view.is_none());

        orch.arm_leader_build_timer_as_direct_wait(Some(12345));

        assert_eq!(
            orch.leader_build_waiting_view,
            Some((orch.engine.current_view(), LeaderBuildWaitMode::Direct))
        );
        assert_eq!(orch.next_slot_timestamp, Some(12345));
    }

    #[test]
    fn test_leader_build_timer_preserves_existing_wait_mode() {
        let (engine, output_rx) = make_test_engine_with_validator_count(7, 1);
        let network = Arc::new(MockConsensusNetwork::default());
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let mut orch = ConsensusService::new(engine, network, net_event_rx, output_rx);

        orch.begin_leader_build_wait(LeaderBuildWaitMode::Scheduled, None);
        orch.arm_leader_build_timer_as_direct_wait(None);

        assert_eq!(
            orch.leader_build_waiting_view,
            Some((orch.engine.current_view(), LeaderBuildWaitMode::Scheduled))
        );
    }

    /// Counting StateSink: records every applied diff (Task 2 spy).
    struct SpyStateSink {
        applies: Arc<Mutex<Vec<usize>>>, // account counts per apply
    }

    impl SpyStateSink {
        fn new() -> (Arc<Self>, Arc<Mutex<Vec<usize>>>) {
            let applies = Arc::new(Mutex::new(Vec::new()));
            (
                Arc::new(Self {
                    applies: Arc::clone(&applies),
                }),
                applies,
            )
        }
    }

    impl crate::sinks::StateSink for SpyStateSink {
        fn apply_diff(
            &self,
            _block_hash: B256,
            diff: &n42_execution::state_diff::StateDiff,
        ) -> Result<(u64, B256), String> {
            self.applies.lock().expect("spy sink lock").push(diff.len());
            Ok((1, B256::repeat_byte(0xAB)))
        }
    }

    async fn drain_spawned_tasks() {
        // The sidecar worker runs the sink on spawn_blocking; yield until
        // the pool has run it.
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    /// Task 2: a committed block whose execution reth REJECTS must never reach
    /// the sidecar state trees; the rejected diff is discarded, and the sink
    /// records zero applies.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sidecar_diff_withheld_when_execution_rejected() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let previous_head = B256::repeat_byte(0x11);
        let bad_hash = B256::repeat_byte(0xDD);
        let mock_el = Arc::new(MockExecutionLayer::new(
            PayloadStatusEnum::Invalid {
                validation_error: "stale nonce".to_string(),
            },
            PayloadStatusEnum::Syncing,
        ));
        let (sink, applies) = SpyStateSink::new();
        let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx);
        orch.el = Some(mock_el);
        orch.jmt = Some(sink);
        orch.head_block_hash = previous_head;
        orch.async_finalize_fcu = false;
        orch.pending_block_data
            .insert(bad_hash, test_block_data(previous_head, bad_hash, 1));
        // The broadcast fixture carries no execution output, so stage the diff
        // directly - the staging/flush timing is what this test pins down.
        orch.pending_sidecar_diffs.insert(
            1,
            (bad_hash, n42_execution::state_diff::StateDiff::default()),
        );

        orch.handle_engine_output(EngineOutput::BlockCommitted {
            view: 1,
            block_hash: bad_hash,
            commit_qc: QuorumCertificate::genesis(),
            validator_changes: None,
        })
        .await;
        let (hash, view, outcome, ts) =
            tokio::time::timeout(Duration::from_secs(2), orch.import_done_rx.recv())
                .await
                .expect("bg import finishes")
                .expect("channel open");
        assert_eq!(outcome, ImportOutcome::Invalid);
        orch.handle_import_done(hash, view, outcome, ts).await;
        drain_spawned_tasks().await;

        assert!(
            applies.lock().unwrap().is_empty(),
            "sidecar sink must not see a diff for an execution-rejected block"
        );
        assert!(
            orch.pending_sidecar_diffs.is_empty(),
            "a rejected block's staged diff must be discarded"
        );
    }

    /// Task 2 happy path: once execution IS confirmed (head advances to the
    /// block), the staged diff is applied exactly once.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sidecar_diff_applied_once_after_confirmation() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let good_hash = B256::repeat_byte(0x22);
        let (sink, applies) = SpyStateSink::new();
        let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx);
        orch.jmt = Some(sink);
        orch.pending_sidecar_diffs.insert(
            1,
            (good_hash, n42_execution::state_diff::StateDiff::default()),
        );

        orch.advance_execution_validated_head(1, good_hash, "test confirmation");
        drain_spawned_tasks().await;
        assert_eq!(
            applies.lock().unwrap().len(),
            1,
            "confirmed block applies its staged diff exactly once"
        );
        assert!(orch.pending_sidecar_diffs.is_empty());

        // A second confirmation of the same hash is a no-op (diff consumed).
        orch.advance_execution_validated_head(1, good_hash, "duplicate confirmation");
        drain_spawned_tasks().await;
        assert_eq!(applies.lock().unwrap().len(), 1);
    }

    /// F8: a missing committed diff is an ordering barrier. A later confirmed
    /// diff waits, then both apply in order once late BlockData repairs the gap.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sidecar_missing_diff_pauses_and_late_block_data_recovers_in_order() {
        use n42_execution::state_diff::{AccountChangeType, AccountDiff, StateDiff};

        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let (sink, applies) = SpyStateSink::new();
        let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx);
        orch.jmt = Some(sink);

        let parent = B256::repeat_byte(0x30);
        let missing_hash = B256::repeat_byte(0x31);
        let later_hash = B256::repeat_byte(0x32);
        orch.missing_sidecar_diffs.insert(1, missing_hash);
        let mut later_diff = StateDiff::default();
        for index in 0..2u8 {
            later_diff.accounts.insert(
                Address::with_last_byte(index + 1),
                AccountDiff {
                    change_type: AccountChangeType::Created,
                    balance: None,
                    nonce: None,
                    code_change: None,
                    storage: BTreeMap::new(),
                },
            );
        }
        orch.pending_sidecar_diffs
            .insert(2, (later_hash, later_diff));

        orch.advance_execution_validated_head(1, missing_hash, "confirm missing diff hash");
        orch.advance_execution_validated_head(2, later_hash, "confirm across missing diff");
        drain_spawned_tasks().await;
        assert!(
            applies.lock().unwrap().is_empty(),
            "view 2 must not apply across the missing view 1 diff"
        );
        assert!(orch.pending_sidecar_diffs.contains_key(&2));

        let late_data = test_block_data_with_empty_execution(parent, missing_hash, 1);
        orch.cache_pending_block_data(missing_hash, late_data, &[missing_hash]);
        drain_spawned_tasks().await;

        assert_eq!(
            *applies.lock().unwrap(),
            vec![0, 2],
            "recovered view 1 must apply before the already-confirmed view 2"
        );
        assert!(orch.missing_sidecar_diffs.is_empty());
        assert!(orch.pending_sidecar_diffs.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sidecar_diffs_apply_in_committed_view_order() {
        use n42_execution::state_diff::{AccountChangeType, AccountDiff, StateDiff};

        fn diff_with_accounts(count: u8) -> StateDiff {
            let mut diff = StateDiff::default();
            for index in 0..count {
                diff.accounts.insert(
                    Address::with_last_byte(index + 1),
                    AccountDiff {
                        change_type: AccountChangeType::Modified,
                        balance: None,
                        nonce: None,
                        code_change: None,
                        storage: BTreeMap::new(),
                    },
                );
            }
            diff
        }

        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let (sink, applies) = SpyStateSink::new();
        let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx);
        orch.jmt = Some(sink);
        let first_hash = B256::repeat_byte(0x23);
        let second_hash = B256::repeat_byte(0x24);
        orch.pending_sidecar_diffs
            .insert(1, (first_hash, diff_with_accounts(1)));
        orch.pending_sidecar_diffs
            .insert(2, (second_hash, diff_with_accounts(2)));

        // The later completion arrives first, but cannot authorize the staged
        // earlier hash by itself. Once the exact earlier completion arrives,
        // the worker applies both diffs in view order without regressing head.
        orch.advance_execution_validated_head(2, second_hash, "later confirmation first");
        assert!(applies.lock().unwrap().is_empty());
        orch.advance_execution_validated_head(1, first_hash, "earlier confirmation observed late");
        drain_spawned_tasks().await;

        assert_eq!(*applies.lock().unwrap(), vec![1, 2]);
        assert!(orch.pending_sidecar_diffs.is_empty());
        assert!(orch.sidecar_apply_queue.lock().unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sidecar_flush_rejects_diff_for_noncanonical_hash_at_view() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let (sink, applies) = SpyStateSink::new();
        let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx);
        orch.jmt = Some(sink);
        let forged_hash = B256::repeat_byte(0x81);
        let canonical_hash = B256::repeat_byte(0x82);
        orch.pending_sidecar_diffs.insert(
            1,
            (forged_hash, n42_execution::state_diff::StateDiff::default()),
        );

        orch.advance_execution_validated_head(1, canonical_hash, "canonical import");
        drain_spawned_tasks().await;

        assert!(applies.lock().unwrap().is_empty());
        assert!(!orch.pending_sidecar_diffs.contains_key(&1));
        assert_eq!(orch.missing_sidecar_diffs.get(&1), Some(&canonical_hash));
    }

    /// F7 defense-in-depth: if an out-of-order entry ever reaches the apply
    /// queue, the single FIFO worker skips it rather than applying a lower view
    /// after a higher one (which would diverge the append-ordered root).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sidecar_worker_skips_out_of_order_apply_across_worker_restarts() {
        use n42_execution::state_diff::{AccountChangeType, AccountDiff, StateDiff};

        fn diff_with_accounts(count: u8) -> StateDiff {
            let mut diff = StateDiff::default();
            for index in 0..count {
                diff.accounts.insert(
                    Address::with_last_byte(index + 1),
                    AccountDiff {
                        change_type: AccountChangeType::Modified,
                        balance: None,
                        nonce: None,
                        code_change: None,
                        storage: BTreeMap::new(),
                    },
                );
            }
            diff
        }

        // NB: the worker runs on spawn_blocking (a separate thread), so a
        // thread-local metrics recorder would not see its counter — the
        // observable `applies` order below is the assertion that matters.
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let (sink, applies) = SpyStateSink::new();
        let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx);
        orch.jmt = Some(sink);
        let hash = B256::repeat_byte(0x25);

        // First worker lifetime: normally apply view 2, then let the queue drain
        // and the worker exit.
        orch.pending_sidecar_diffs
            .insert(2, (hash, diff_with_accounts(2)));
        orch.advance_execution_validated_head(2, hash, "confirm view 2");
        drain_spawned_tasks().await;
        assert_eq!(*applies.lock().unwrap(), vec![2]);

        // Second worker lifetime: inject stale view 1 before normally-confirmed
        // view 3. A worker-local last-view variable would reset to zero here and
        // incorrectly apply [1, 3]; the shared high-water mark must skip 1.
        orch.sidecar_apply_queue
            .lock()
            .unwrap()
            .push_back((1, hash, diff_with_accounts(1)));
        orch.pending_sidecar_diffs
            .insert(3, (hash, diff_with_accounts(3)));
        orch.advance_execution_validated_head(3, hash, "confirm view 3");
        drain_spawned_tasks().await;

        assert_eq!(
            *applies.lock().unwrap(),
            vec![2, 3],
            "the out-of-order lower view must be skipped, not applied"
        );
    }

    /// Task 3: a stale pending finalization whose block the committed ring
    /// still holds re-drives the import LOCALLY - no sync round-trip for data
    /// this node already has.
    #[tokio::test]
    async fn stale_pending_finalization_redrives_from_committed_broadcast() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let head = B256::repeat_byte(0x31);
        let committed_hash = B256::repeat_byte(0x44);
        let raw = test_block_data(head, committed_hash, 3);
        let mock_el = Arc::new(MockExecutionLayer::new(
            PayloadStatusEnum::Valid,
            PayloadStatusEnum::Valid,
        ));
        let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx);
        orch.el = Some(mock_el.clone());
        orch.pending_block_data.insert(committed_hash, raw);
        orch.store_committed_block(3, committed_hash, QuorumCertificate::genesis(), None);
        orch.defer_finalization(3, committed_hash, QuorumCertificate::genesis());
        orch.pending_block_data.clear(); // the eviction that used to force a sync

        orch.handle_view_changed(6).await; // 3 views past the pending one

        assert!(
            orch.pending_finalization.is_none(),
            "stale pending finalization is consumed"
        );
        assert!(
            !orch.sync_in_flight,
            "locally retained broadcast must not fall back to sync"
        );
        let (hash, view, outcome, _ts) =
            tokio::time::timeout(Duration::from_secs(2), orch.import_done_rx.recv())
                .await
                .expect("local re-drive import completes")
                .expect("channel open");
        assert_eq!((hash, view), (committed_hash, 3));
        assert_eq!(
            outcome,
            ImportOutcome::Valid,
            "the retained broadcast re-imports successfully"
        );
        assert_eq!(
            mock_el.new_payload_hashes(),
            vec![committed_hash],
            "the re-drive imports the retained broadcast through new_payload"
        );
        // The block IS committed - the re-driven import legitimately finishes
        // with the commit-authorized fork-choice update (Case B semantics).
        assert_eq!(mock_el.fcu_heads(), vec![committed_hash]);
    }

    /// Task 3 fallback: with no retained broadcast the stale finalization
    /// still goes to sync (the pre-existing behavior).
    #[tokio::test]
    async fn stale_pending_finalization_without_payload_falls_back_to_sync() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let committed_hash = B256::repeat_byte(0x55);
        let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx);
        orch.connected_peers.insert(n42_network::PeerId::random());
        orch.defer_finalization(3, committed_hash, QuorumCertificate::genesis());

        orch.handle_view_changed(6).await;

        assert!(orch.pending_finalization.is_none());
        assert!(
            orch.sync_in_flight,
            "no local data -> sync is the only recovery"
        );
        assert!(!orch.bg_import_hashes.contains(&committed_hash));
    }

    /// Task 5: import paths must NEVER move reth fork choice - the
    /// single-canonical-writer invariant. Passive block data import and the
    /// commit-driven finalize are driven back to back; only the finalize may
    /// produce an attribute-less FCU.
    #[tokio::test]
    async fn import_paths_never_issue_fcu() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let head = B256::repeat_byte(0x61);
        let block_hash = B256::repeat_byte(0x62);
        let mock_el = Arc::new(MockExecutionLayer::new(
            PayloadStatusEnum::Valid,
            PayloadStatusEnum::Valid,
        ));
        let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx);
        orch.el = Some(mock_el.clone());
        orch.head_block_hash = head;
        orch.async_finalize_fcu = false;

        // Passive path: block data arrives from the network.
        orch.handle_block_data(test_block_data(head, block_hash, 1))
            .await;
        assert!(
            mock_el.fcu_heads().is_empty(),
            "passive block-data import must not call forkchoiceUpdated"
        );

        // Commit-driven finalize is the ONLY canonical writer.
        orch.finalize_committed_block(1, block_hash, QuorumCertificate::genesis())
            .await;
        assert!(
            !mock_el.fcu_heads().is_empty(),
            "finalize is expected to move the fork choice"
        );
        assert_eq!(mock_el.fcu_heads(), vec![block_hash]);
    }

    // ─────────────────────────────────────────────────────────────────────
    // Restart-boundary hardening (PR #21 re-audit T1-T4)
    // ─────────────────────────────────────────────────────────────────────

    /// T2 unit: the FCU-suppression predicate refuses only backward or
    /// sideways moves; a strictly-newer view, an idempotent re-import of the
    /// same head, and the view-0 restart/genesis seed are all admissible.
    #[test]
    fn import_would_regress_head_is_backward_or_sideways_only() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx);
        let head = B256::repeat_byte(0x22);
        orch.execution_validated_head_view = 5;
        orch.head_block_hash = head;

        assert!(
            orch.import_would_regress_head(4, B256::repeat_byte(0x33)),
            "strictly older view regresses"
        );
        assert!(
            orch.import_would_regress_head(5, B256::repeat_byte(0x33)),
            "same view, different hash is a sideways regress"
        );
        assert!(
            !orch.import_would_regress_head(5, head),
            "same view, same head is idempotent, not a regress"
        );
        assert!(
            !orch.import_would_regress_head(6, B256::repeat_byte(0x33)),
            "strictly newer view is forward"
        );

        // View 0 is the restart/genesis seed: the same-view conflict is exempt
        // so the very first advance can seed the head.
        orch.execution_validated_head_view = 0;
        assert!(!orch.import_would_regress_head(0, B256::repeat_byte(0x77)));
    }

    /// T2b core (F1): a stale block reth would accept as `Valid`, imported at a
    /// view at or below the execution-validated head, must NOT move reth fork
    /// choice backward. The guard fires ahead of the FCU side effect.
    #[tokio::test(flavor = "current_thread")]
    async fn sync_import_below_validated_head_issues_no_backward_fcu() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);

        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let head = B256::repeat_byte(0x50);
        let old_hash = B256::repeat_byte(0x40);
        let mock = Arc::new(MockExecutionLayer::new(
            PayloadStatusEnum::Valid,
            PayloadStatusEnum::Valid,
        ));
        let el: Arc<dyn ExecutionLayer> = mock.clone();
        let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx);
        orch.el = Some(el);
        orch.head_block_hash = head;
        orch.execution_validated_head_view = 10;

        let broadcast = BlockDataBroadcast {
            block_hash: old_hash,
            view: 5,
            payload_json: serde_json::to_vec(&test_execution_data(head, old_hash)).unwrap(),
            timestamp: 0,
            execution_output: None,
            leader_ready_unix_ms: 0,
        };
        let _ = orch.import_and_notify(broadcast).await;

        assert!(
            mock.fcu_heads().is_empty(),
            "an import below the validated head must not fork-choice-update reth backward"
        );
        assert_eq!(
            orch.head_block_hash, head,
            "the execution-validated head must not regress"
        );
        let skipped =
            snapshotter
                .snapshot()
                .into_vec()
                .into_iter()
                .find_map(|(key, _, _, value)| {
                    (key.key().name() == "n42_import_fcu_skipped_backward_total").then_some(value)
                });
        assert_eq!(skipped, Some(DebugValue::Counter(1)));
    }

    /// T2a: a sync response block outside the requested `[from_view, to_view]`
    /// range is dropped before QC verification and never counted.
    #[tokio::test(flavor = "current_thread")]
    async fn sync_response_drops_blocks_outside_requested_range() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);

        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let peer = n42_network::PeerId::random();
        let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx);
        orch.sync_request_range = Some((10, 20));
        orch.sync_requested_peers.insert(peer);
        let before = orch.committed_block_count;

        let response = n42_network::BlockSyncResponse {
            blocks: vec![n42_network::SyncBlock {
                view: 5, // below the requested [10, 20]
                block_hash: B256::repeat_byte(0x33),
                commit_qc: QuorumCertificate::genesis(),
                payload: vec![1, 2, 3], // non-empty so it is not skipped as empty
                validator_changes: None,
            }],
            peer_committed_view: 5,
            execution_lineage: Vec::new(),
        };
        orch.handle_sync_response(peer, response).await;

        assert_eq!(
            orch.committed_block_count, before,
            "an out-of-range block must not be counted"
        );
        let dropped =
            snapshotter
                .snapshot()
                .into_vec()
                .into_iter()
                .find_map(|(key, _, _, value)| {
                    (key.key().name() == "n42_sync_blocks_out_of_range_total").then_some(value)
                });
        assert_eq!(dropped, Some(DebugValue::Counter(1)));
    }

    /// A CommitQC child can finalize a prepared execution ancestor that has no
    /// CommitQC of its own. Restart recovery must import both raw payloads in
    /// parent order, preserve their compact outputs for later peers, advance
    /// the real execution block number, and apply both Twig/QMDB sidecars.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sync_recovers_prepared_ancestor_execution_lineage() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let peer = n42_network::PeerId::random();
        let durable_head = B256::repeat_byte(0x43);
        let prepared_hash = B256::repeat_byte(0x44);
        let committed_hash = B256::repeat_byte(0x45);
        let prepared_data =
            test_block_data_with_empty_execution_at_number(durable_head, prepared_hash, 2, 144);
        let committed_data =
            test_block_data_with_empty_execution_at_number(prepared_hash, committed_hash, 3, 145);
        let committed_broadcast: BlockDataBroadcast =
            bincode::deserialize(&committed_data).expect("test broadcast decodes");
        let mock = Arc::new(MockExecutionLayer::with_fcu_statuses(
            PayloadStatusEnum::Valid,
            [PayloadStatusEnum::Valid, PayloadStatusEnum::Valid],
        ));
        let el: Arc<dyn ExecutionLayer> = mock.clone();
        let (twig, applies) = SpyStateSink::new();
        let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx)
            .with_twig(twig);
        orch.el = Some(el);
        orch.head_block_hash = durable_head;
        orch.execution_validated_head_view = 1;
        orch.committed_block_count = 143;
        orch.sync_in_flight = true;
        orch.sync_started_at = Some(Instant::now());
        orch.sync_request_range = Some((2, 3));
        orch.sync_requested_peers.insert(peer);

        orch.handle_sync_response(
            peer,
            n42_network::BlockSyncResponse {
                blocks: vec![n42_network::SyncBlock {
                    view: 3,
                    block_hash: committed_hash,
                    commit_qc: test_commit_qc(3, committed_hash),
                    payload: committed_broadcast.payload_json.clone(),
                    validator_changes: None,
                }],
                peer_committed_view: 3,
                execution_lineage: vec![
                    n42_network::SyncPayload {
                        view: 2,
                        block_hash: prepared_hash,
                        block_data: prepared_data,
                    },
                    n42_network::SyncPayload {
                        view: 3,
                        block_hash: committed_hash,
                        block_data: committed_data,
                    },
                ],
            },
        )
        .await;
        drain_spawned_tasks().await;

        assert_eq!(
            mock.new_payload_hashes(),
            vec![prepared_hash, committed_hash],
            "reth receives the prepared ancestor before its CommitQC child"
        );
        assert_eq!(mock.fcu_heads(), vec![prepared_hash, committed_hash]);
        assert_eq!(orch.head_block_hash, committed_hash);
        assert_eq!(orch.execution_validated_head_view, 3);
        assert_eq!(
            orch.committed_block_count, 145,
            "metadata follows the execution payload number, not CommitQC count"
        );
        let retained = orch
            .committed_blocks
            .back()
            .expect("synced committed block retained");
        assert_eq!(retained.execution_lineage.len(), 2);
        assert_eq!(retained.execution_lineage[0].block_hash, prepared_hash);
        assert_eq!(retained.execution_lineage[1].block_hash, committed_hash);
        assert_eq!(
            applies.lock().expect("spy sink lock").len(),
            2,
            "both execution StateDiffs reach the binary-tree sidecar"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sync_does_not_trust_raw_lineage_that_misses_exact_head() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let peer = n42_network::PeerId::random();
        let exact_head = B256::repeat_byte(0x63);
        let foreign_parent = B256::repeat_byte(0x99);
        let prepared_hash = B256::repeat_byte(0x64);
        let committed_hash = B256::repeat_byte(0x65);
        let prepared_data = test_block_data_at_number(foreign_parent, prepared_hash, 2, 144);
        let committed_data = test_block_data_at_number(prepared_hash, committed_hash, 3, 145);
        let committed_broadcast: BlockDataBroadcast =
            bincode::deserialize(&committed_data).expect("test broadcast decodes");
        let mock = Arc::new(MockExecutionLayer::new(
            PayloadStatusEnum::Syncing,
            PayloadStatusEnum::Valid,
        ));
        let el: Arc<dyn ExecutionLayer> = mock.clone();
        let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx);
        orch.el = Some(el);
        orch.head_block_hash = exact_head;
        orch.execution_validated_head_view = 1;
        orch.committed_block_count = 143;
        orch.sync_in_flight = true;
        orch.sync_started_at = Some(Instant::now());
        orch.sync_request_range = Some((2, 3));
        orch.sync_requested_peers.insert(peer);

        orch.handle_sync_response(
            peer,
            n42_network::BlockSyncResponse {
                blocks: vec![n42_network::SyncBlock {
                    view: 3,
                    block_hash: committed_hash,
                    commit_qc: test_commit_qc(3, committed_hash),
                    payload: committed_broadcast.payload_json,
                    validator_changes: None,
                }],
                peer_committed_view: 3,
                execution_lineage: vec![
                    n42_network::SyncPayload {
                        view: 2,
                        block_hash: prepared_hash,
                        block_data: prepared_data,
                    },
                    n42_network::SyncPayload {
                        view: 3,
                        block_hash: committed_hash,
                        block_data: committed_data,
                    },
                ],
            },
        )
        .await;

        assert_eq!(
            mock.new_payload_hashes(),
            vec![committed_hash],
            "the foreign prepared payload and its cached output are never submitted"
        );
        assert!(mock.fcu_heads().is_empty());
        assert_eq!(orch.head_block_hash, exact_head);
        assert_eq!(orch.execution_validated_head_view, 1);
        assert_eq!(orch.syncing_blocks.len(), 1);
        assert!(
            orch.committed_blocks
                .back()
                .expect("QC metadata remains recoverable")
                .execution_lineage
                .is_empty(),
            "an unproven raw lineage must not enter the sync-serving ring"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unsolicited_sync_response_cannot_cancel_active_request() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let expected_peer = n42_network::PeerId::random();
        let unexpected_peer = n42_network::PeerId::random();
        let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx);
        orch.sync_in_flight = true;
        orch.sync_started_at = Some(Instant::now());
        orch.sync_request_range = Some((10, 20));
        orch.sync_requested_peers.insert(expected_peer);

        orch.handle_sync_response(
            unexpected_peer,
            n42_network::BlockSyncResponse {
                blocks: Vec::new(),
                peer_committed_view: 20,
                execution_lineage: Vec::new(),
            },
        )
        .await;

        assert!(orch.sync_in_flight, "the real request must remain active");
        assert_eq!(orch.sync_request_range, Some((10, 20)));
        assert!(orch.sync_requested_peers.contains(&expected_peer));
    }

    /// T3 (F2): restart recovery seeds the execution floor from a view/hash pair
    /// proven against reth's canonical head. Catch-up must preserve that floor.
    #[tokio::test(flavor = "current_thread")]
    async fn post_restart_catchup_floor_uses_recovered_execution_view() {
        let (engine, output_rx) = make_test_engine();
        let (network, mut cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let peer = n42_network::PeerId::random();
        let waiting = B256::repeat_byte(0x22);
        let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx);
        orch.connected_peers.insert(peer);
        orch.head_block_hash = B256::repeat_byte(0x11);
        orch = orch.with_recovered_execution_validated_head(100);

        let mut recovered = QuorumCertificate::genesis();
        recovered.view = 100;
        orch.last_commit_qc = Some(recovered);
        orch.committed_blocks.push_back(CommittedBlock {
            view: 105,
            block_hash: waiting,
            commit_qc: QuorumCertificate::genesis(),
            payload: Vec::new(),
            validator_changes: None,
            execution_lineage: Vec::new(),
        });

        orch.handle_import_done(waiting, 105, ImportOutcome::Syncing, 0)
            .await;

        let command = tokio::time::timeout(Duration::from_secs(1), cmd_rx.recv())
            .await
            .expect("a catch-up sync command should be sent")
            .expect("network command channel should remain open");
        match command {
            NetworkCommand::RequestSync { request, .. } => {
                assert_eq!(
                    request.from_view, 101,
                    "floor derives from the recovered committed view (100), not 1"
                );
            }
            other => panic!("unexpected network command: {other:?}"),
        }
    }

    /// A joining validator may receive a Decide for a descendant before it has
    /// any ancestor payloads. Its newly learned commit view is not an execution
    /// floor: catch-up must start after genesis so the missing lineage is not
    /// skipped.
    #[tokio::test(flavor = "current_thread")]
    async fn fresh_joiner_catchup_does_not_use_learned_commit_view_as_floor() {
        let (engine, output_rx) = make_test_engine();
        let (network, mut cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let peer = n42_network::PeerId::random();
        let waiting = B256::repeat_byte(0x32);
        let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx);
        orch.connected_peers.insert(peer);
        orch.head_block_hash = B256::repeat_byte(0x31);
        orch.execution_validated_head_view = 0;

        let mut learned = QuorumCertificate::genesis();
        learned.view = 3;
        orch.last_commit_qc = Some(learned);
        orch.committed_blocks.push_back(CommittedBlock {
            view: 3,
            block_hash: waiting,
            commit_qc: QuorumCertificate::genesis(),
            payload: Vec::new(),
            validator_changes: None,
            execution_lineage: Vec::new(),
        });

        orch.handle_import_done(waiting, 3, ImportOutcome::Syncing, 0)
            .await;

        let command = tokio::time::timeout(Duration::from_secs(1), cmd_rx.recv())
            .await
            .expect("a catch-up sync command should be sent")
            .expect("network command channel should remain open");
        match command {
            NetworkCommand::RequestSync { request, .. } => {
                assert_eq!(
                    request.from_view, 1,
                    "a fresh joiner must request ancestors after its execution floor"
                );
            }
            other => panic!("unexpected network command: {other:?}"),
        }
    }

    /// T4 (F3): `Accepted` from `new_payload` means the payload was stored for a
    /// side chain WITHOUT execution. It must not advance the head nor flush the
    /// staged sidecar diff; the block is queued for retry.
    #[tokio::test(flavor = "current_thread")]
    async fn accepted_new_payload_is_not_execution_validity() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let head = B256::repeat_byte(0x11);
        let block = B256::repeat_byte(0x22);
        let mock = Arc::new(MockExecutionLayer::new(
            PayloadStatusEnum::Accepted,
            PayloadStatusEnum::Accepted,
        ));
        let el: Arc<dyn ExecutionLayer> = mock.clone();
        let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx);
        orch.el = Some(el);
        orch.head_block_hash = head;
        orch.execution_validated_head_view = 0;
        orch.pending_sidecar_diffs
            .insert(3, (block, n42_execution::state_diff::StateDiff::default()));

        let broadcast = BlockDataBroadcast {
            block_hash: block,
            view: 3,
            payload_json: serde_json::to_vec(&test_execution_data(head, block)).unwrap(),
            timestamp: 0,
            execution_output: None,
            leader_ready_unix_ms: 0,
        };
        let _ = orch.import_and_notify(broadcast).await;

        assert_eq!(
            orch.head_block_hash, head,
            "Accepted (stored, not executed) must not advance the head"
        );
        assert_eq!(orch.execution_validated_head_view, 0);
        assert!(
            mock.fcu_heads().is_empty(),
            "Accepted must not fork-choice-update reth"
        );
        assert!(
            orch.pending_sidecar_diffs.contains_key(&3),
            "the staged sidecar diff must not be flushed for an unexecuted block"
        );
        assert!(
            !orch.syncing_blocks.is_empty(),
            "Accepted is queued for retry, exactly like Syncing"
        );
    }

    /// A `Valid` new-payload response is insufficient when the following FCU
    /// says `Accepted`: reth has executed the payload but has not accepted it as
    /// the requested canonical/finalized head.
    #[tokio::test(flavor = "current_thread")]
    async fn accepted_fcu_does_not_advance_execution_head() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let head = B256::repeat_byte(0x11);
        let block = B256::repeat_byte(0x22);
        let mock = Arc::new(MockExecutionLayer::new(
            PayloadStatusEnum::Valid,
            PayloadStatusEnum::Accepted,
        ));
        let el: Arc<dyn ExecutionLayer> = mock.clone();
        let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx);
        orch.el = Some(el);
        orch.head_block_hash = head;
        orch.execution_validated_head_view = 1;

        let _ = orch
            .import_and_notify(BlockDataBroadcast {
                block_hash: block,
                view: 2,
                payload_json: serde_json::to_vec(&test_execution_data(head, block)).unwrap(),
                timestamp: 0,
                execution_output: None,
                leader_ready_unix_ms: 0,
            })
            .await;

        assert_eq!(orch.head_block_hash, head);
        assert_eq!(orch.execution_validated_head_view, 1);
        assert_eq!(mock.fcu_heads(), vec![block]);
        assert_eq!(orch.syncing_blocks.len(), 1, "the FCU is retried later");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn invalid_payload_is_cached_and_second_sync_arrival_skips_engine() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let parent = B256::repeat_byte(0x31);
        let bad_hash = B256::repeat_byte(0x32);
        let mock = Arc::new(MockExecutionLayer::new(
            PayloadStatusEnum::Invalid {
                validation_error: "state root mismatch".to_owned(),
            },
            PayloadStatusEnum::Valid,
        ));
        let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx);
        orch.el = Some(mock.clone());

        for expected_submission in [false, false] {
            let broadcast = bincode::deserialize(&test_block_data(parent, bad_hash, 1))
                .expect("test broadcast decodes");
            assert_eq!(orch.import_and_notify(broadcast).await, expected_submission);
        }

        assert_eq!(
            mock.new_payload_hashes(),
            vec![bad_hash],
            "the cached second arrival must not reach new_payload"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn declared_hash_poison_is_evicted_and_honest_block_can_follow() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let parent = B256::repeat_byte(0x71);
        let honest_hash = B256::repeat_byte(0x72);
        let mock = Arc::new(MockExecutionLayer::with_new_payload_statuses(
            [
                PayloadStatusEnum::Invalid {
                    validation_error: format!(
                        "block hash mismatch: want {}, got {}",
                        B256::repeat_byte(0x73),
                        honest_hash
                    ),
                },
                PayloadStatusEnum::Valid,
            ],
            PayloadStatusEnum::Valid,
        ));
        let cache = Arc::new(MockExecutionOutputCache::default());
        let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx)
            .with_exec_output_cache(cache.clone());
        orch.el = Some(mock.clone());
        orch.head_block_hash = parent;

        let forged = BlockDataBroadcast {
            block_hash: honest_hash,
            view: 1,
            // The envelope and payload both declare the honest hash so the
            // pre-submit field check passes, while the payload contents differ
            // from the honest arrival below. Reth is what recomputes the hash.
            payload_json: serde_json::to_vec(&test_execution_data_at_number(
                parent,
                honest_hash,
                999,
            ))
            .unwrap(),
            timestamp: 1,
            // The mock cache deliberately accepts arbitrary bytes: this models
            // a peer-supplied bundle keyed by the honest block's declared hash.
            execution_output: Some(vec![0xFA, 0xCE]),
            leader_ready_unix_ms: 0,
        };
        assert!(!orch.import_and_notify(forged).await);
        assert_eq!(cache.evicted(), vec![honest_hash]);
        assert!(!cache.contains(honest_hash));
        assert!(
            !orch.bad_blocks.should_skip(honest_hash, "test"),
            "a recomputed-hash mismatch must not blacklist the declared honest hash"
        );

        let honest = BlockDataBroadcast {
            block_hash: honest_hash,
            view: 1,
            payload_json: serde_json::to_vec(&test_execution_data(parent, honest_hash)).unwrap(),
            timestamp: 1,
            execution_output: None,
            leader_ready_unix_ms: 0,
        };
        assert!(orch.import_and_notify(honest).await);
        assert_eq!(orch.head_block_hash, honest_hash);
        assert_eq!(mock.new_payload_hashes(), vec![honest_hash, honest_hash]);
    }

    /// HIGH-1: the payload itself can honestly declare `H` while an
    /// unauthenticated peer substitutes only the compact execution output. In
    /// production reth then reports a state-root mismatch. That verdict is
    /// about the injected bytes, not block `H`, so the cache must be evicted
    /// without blacklisting `H`; a later honest compact-free arrival must run.
    #[tokio::test(flavor = "current_thread")]
    async fn forged_compact_output_does_not_blacklist_honest_hash() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let parent = B256::repeat_byte(0x74);
        let honest_hash = B256::repeat_byte(0x75);
        let mock = Arc::new(MockExecutionLayer::with_new_payload_statuses(
            [
                PayloadStatusEnum::Invalid {
                    validation_error: "state root mismatch from compact output".to_owned(),
                },
                PayloadStatusEnum::Valid,
            ],
            PayloadStatusEnum::Valid,
        ));
        let cache = Arc::new(MockExecutionOutputCache::default());
        let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx)
            .with_exec_output_cache(cache.clone());
        orch.el = Some(mock.clone());
        orch.head_block_hash = parent;

        let forged_compact = BlockDataBroadcast {
            block_hash: honest_hash,
            view: 1,
            // This payload is the honest block and passes the envelope hash
            // check. Only its independently carried compact bytes are forged.
            payload_json: serde_json::to_vec(&test_execution_data(parent, honest_hash)).unwrap(),
            timestamp: 1,
            execution_output: Some(vec![0xBA, 0xD0, 0x00, 0x01]),
            leader_ready_unix_ms: 0,
        };
        assert!(!orch.import_and_notify(forged_compact).await);
        assert_eq!(cache.evicted(), vec![honest_hash]);
        assert!(!cache.contains(honest_hash));
        assert!(
            !orch.bad_blocks.should_skip(honest_hash, "test"),
            "state-root failure after compact injection must not blacklist H"
        );

        let honest = BlockDataBroadcast {
            block_hash: honest_hash,
            view: 1,
            payload_json: serde_json::to_vec(&test_execution_data(parent, honest_hash)).unwrap(),
            timestamp: 1,
            execution_output: None,
            leader_ready_unix_ms: 0,
        };
        assert!(orch.import_and_notify(honest).await);
        assert_eq!(orch.head_block_hash, honest_hash);
        assert_eq!(mock.new_payload_hashes(), vec![honest_hash, honest_hash]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn direct_block_arrival_is_filtered_by_bad_block_cache() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let parent = B256::repeat_byte(0x35);
        let bad_hash = B256::repeat_byte(0x36);
        let mock = Arc::new(MockExecutionLayer::new(
            PayloadStatusEnum::Valid,
            PayloadStatusEnum::Valid,
        ));
        let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx);
        orch.el = Some(mock.clone());
        orch.bad_blocks
            .insert_invalid_payload(bad_hash, "known invalid", "test_setup");

        orch.handle_block_data(test_block_data(parent, bad_hash, 1))
            .await;

        assert!(mock.new_payload_hashes().is_empty());
        assert!(
            !orch.pending_block_data.contains_key(&bad_hash),
            "known bad direct/gossip data must be dropped before consensus notification"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn syncing_payload_is_never_added_to_bad_block_cache() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let parent = B256::repeat_byte(0x41);
        let block_hash = B256::repeat_byte(0x42);
        let mock = Arc::new(MockExecutionLayer::new(
            PayloadStatusEnum::Syncing,
            PayloadStatusEnum::Valid,
        ));
        let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx);
        orch.el = Some(mock.clone());

        for _ in 0..2 {
            let broadcast = bincode::deserialize(&test_block_data(parent, block_hash, 1))
                .expect("test broadcast decodes");
            assert!(orch.import_and_notify(broadcast).await);
        }

        assert_eq!(mock.new_payload_hashes(), vec![block_hash, block_hash]);
        assert!(
            !orch.bad_blocks.should_skip(block_hash, "test"),
            "Syncing is retryable and must not poison the cache"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mismatched_sync_envelope_cannot_poison_declared_hash() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let parent = B256::repeat_byte(0x51);
        let declared_hash = B256::repeat_byte(0x52);
        let payload_hash = B256::repeat_byte(0x53);
        let mock = Arc::new(MockExecutionLayer::new(
            PayloadStatusEnum::Invalid {
                validation_error: "invalid".to_owned(),
            },
            PayloadStatusEnum::Valid,
        ));
        let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx);
        orch.el = Some(mock.clone());

        let submitted = orch
            .import_and_notify(BlockDataBroadcast {
                block_hash: declared_hash,
                view: 1,
                payload_json: serde_json::to_vec(&test_execution_data(parent, payload_hash))
                    .unwrap(),
                timestamp: 0,
                execution_output: None,
                leader_ready_unix_ms: 0,
            })
            .await;

        assert!(!submitted);
        assert!(mock.new_payload_hashes().is_empty());
        assert!(
            !orch.bad_blocks.should_skip(declared_hash, "test"),
            "untrusted envelope mismatches must be dropped, not cached"
        );
    }

    /// T4 rescue edge: eager `new_payload=Valid` arriving during a failed FCU
    /// only justifies retrying FCU. If that retry is still `Accepted`, the
    /// committed/build head must remain at the last canonical Valid block.
    #[tokio::test(flavor = "current_thread")]
    async fn accepted_retry_fcu_after_eager_valid_does_not_advance_head() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let head = B256::repeat_byte(0x11);
        let block = B256::repeat_byte(0x22);
        let mock = Arc::new(MockExecutionLayer::new(
            PayloadStatusEnum::Valid,
            PayloadStatusEnum::Accepted,
        ));
        let el: Arc<dyn ExecutionLayer> = mock.clone();
        let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx);
        orch.el = Some(el);
        orch.head_block_hash = head;
        orch.execution_validated_head_view = 1;
        orch.eager_import_done_tx
            .send((block, 0))
            .await
            .expect("eager completion channel remains open");

        orch.finalize_committed_block(2, block, QuorumCertificate::genesis())
            .await;

        assert_eq!(orch.head_block_hash, head);
        assert_eq!(orch.execution_validated_head_view, 1);
        assert_eq!(
            mock.fcu_heads(),
            vec![block, block],
            "the eager completion triggers exactly one FCU retry"
        );
    }

    /// T1: startup validates the persisted view/hash mapping before calling the
    /// builder. The builder restores only the proven view and never replaces
    /// the canonical head already derived from reth.
    #[test]
    fn with_recovered_execution_validated_head_preserves_reth_hash() {
        let head = B256::repeat_byte(0x60);

        let (engine, output_rx) = make_test_engine();
        let (network, _c, _p) = make_test_network();
        let (_t, net_event_rx) = mpsc::channel(8192);
        let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx);
        orch.head_block_hash = head;
        let orch = orch.with_recovered_execution_validated_head(42);
        assert_eq!(orch.execution_validated_head_view, 42);
        assert_eq!(orch.head_block_hash, head);
    }

    /// T5 (F4): the bounded eager-validation set can evict a true confirmation
    /// under speculative churn, but the finalize path drains the eager
    /// completion channel and rescues it — the head still advances. This proves
    /// the recoverability the re-audit relies on to rate the eviction bounded.
    #[tokio::test(flavor = "current_thread")]
    async fn eager_validation_evicted_by_churn_is_rescued_at_finalize() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);

        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let previous_head = B256::repeat_byte(0x34);
        let committed_hash = B256::repeat_byte(0xF0);
        // FCU returns Syncing first, forcing the finalize path to drain the
        // eager completion channel — the rescue route for an evicted entry.
        let mock_el = Arc::new(MockExecutionLayer::with_fcu_statuses(
            PayloadStatusEnum::Valid,
            [PayloadStatusEnum::Syncing, PayloadStatusEnum::Valid],
        ));
        let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx);
        orch.el = Some(mock_el);
        orch.head_block_hash = previous_head;
        orch.async_finalize_fcu = false;

        // Saturate the bounded eager set (cap 32) with unrelated churn so the
        // committed block's own eager entry is NOT present — the eviction case.
        for i in 0..32u8 {
            orch.eager_execution_validated
                .push_back(B256::repeat_byte(0x80 + i));
        }
        assert!(
            !orch.eager_execution_validated.contains(&committed_hash),
            "the committed block's eager entry has been evicted by churn"
        );

        // Its eager import still completed and is waiting on the channel.
        orch.eager_import_done_tx
            .send((committed_hash, 1))
            .await
            .expect("queue eager completion");

        orch.handle_engine_output(EngineOutput::BlockCommitted {
            view: 1,
            block_hash: committed_hash,
            commit_qc: QuorumCertificate::genesis(),
            validator_changes: None,
        })
        .await;

        assert_eq!(
            orch.head_block_hash, committed_hash,
            "the evicted-but-imported block is rescued and the head advances"
        );
        assert_eq!(orch.execution_validated_head_view, 1);
        let rescued =
            snapshotter
                .snapshot()
                .into_vec()
                .into_iter()
                .find_map(|(key, _, _, value)| {
                    (key.key().name() == "n42_eager_import_rescued_total").then_some(value)
                });
        assert_eq!(rescued, Some(DebugValue::Counter(1)));
    }
}
