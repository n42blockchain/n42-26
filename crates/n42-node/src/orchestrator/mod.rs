mod consensus_loop;
mod execution_bridge;
pub mod observer;
mod state_mgmt;

use crate::consensus_state::SharedConsensusState;
use crate::epoch_schedule::EpochSchedule;
use crate::mobile_reward::MobileRewardManager;
use crate::staking::StakingManager;
use alloy_primitives::{Address, B256};
use n42_consensus::{ConsensusEngine, EngineOutput, ValidatorSet};
use n42_network::{NetworkEvent, NetworkHandle, PeerId};
use n42_primitives::QuorumCertificate;
use reth_ethereum_engine_primitives::EthEngineTypes;
use reth_node_builder::ConsensusEngineHandle;
use reth_payload_builder::PayloadBuilderHandle;
use reth_transaction_pool::blobstore::DiskFileBlobStore;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
use metrics::{counter, gauge, histogram};
use tracing::{debug, error, info, warn};

/// zstd magic bytes: all zstd frames start with 0x28B52FFD.
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

/// Compress payload JSON with zstd (level 3 — good speed/ratio tradeoff).
pub(crate) fn compress_payload(json: &[u8]) -> Vec<u8> {
    zstd::bulk::compress(json, 3).unwrap_or_else(|e| {
        tracing::warn!(target: "n42::cl", len = json.len(), error = %e, "zstd compression failed, sending uncompressed");
        json.to_vec()
    })
}

/// Decompress payload if zstd-compressed; pass through raw JSON unchanged.
/// Backward-compatible: old nodes send uncompressed JSON, new nodes send zstd.
pub(crate) fn decompress_payload(data: &[u8]) -> std::io::Result<Vec<u8>> {
    if data.len() >= 4 && data[..4] == ZSTD_MAGIC {
        zstd::bulk::decompress(data, 64 * 1024 * 1024) // 64 MB max
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    } else {
        Ok(data.to_vec())
    }
}

/// Block data broadcast via /n42/blocks/1 GossipSub topic.
///
/// NOTE: Serialized with bincode. Adding new fields requires all nodes to upgrade
/// simultaneously — bincode does not support missing trailing fields on deserialization.
/// New→old is fine (bincode ignores trailing bytes), but old→new will fail.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct BlockDataBroadcast {
    pub(crate) block_hash: B256,
    pub(crate) view: u64,
    /// Execution payload — zstd-compressed JSON (or raw JSON from older peers).
    /// Use `decompress_payload()` before `serde_json::from_slice()`.
    pub(crate) payload_json: Vec<u8>,
    /// Block timestamp (seconds since epoch). Stored directly to avoid JSON re-parsing.
    /// Defaults to 0 for backwards compatibility with older serialized broadcasts.
    #[serde(default)]
    pub(crate) timestamp: u64,
    /// Compact Block: zstd-compressed bincode of `CompactBlockExecution`.
    /// When present, followers inject this into payload_cache to skip EVM re-execution.
    /// None for backwards compatibility with older peers.
    #[serde(default)]
    pub(crate) execution_output: Option<Vec<u8>>,
}

/// Serializable proxy for `(BlockExecutionOutput<Receipt>, Vec<Address>)`.
/// `BlockExecutionResult` from alloy-evm doesn't derive serde, so we flatten it.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct CompactBlockExecution {
    pub(crate) bundle_state: revm::database::states::BundleState,
    pub(crate) receipts: Vec<reth_ethereum_primitives::Receipt>,
    pub(crate) requests: alloy_eips::eip7685::Requests,
    pub(crate) gas_used: u64,
    pub(crate) blob_gas_used: u64,
    pub(crate) senders: Vec<Address>,
}

/// Blob sidecar broadcast via /n42/blobs/1 GossipSub topic.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct BlobSidecarBroadcast {
    pub(crate) block_hash: B256,
    pub(crate) view: u64,
    pub(crate) sidecars: Vec<(B256, Vec<u8>)>,
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

impl PipelineTiming {
    fn new_follower() -> Self {
        Self { created: Instant::now(), is_leader: false, build_complete: None, import_complete: None, committed: None }
    }

    /// Produces a one-line summary of pipeline stage durations (ms).
    fn summary(&self) -> String {
        let role = if self.is_leader { "L" } else { "F" };
        let build_ms = self.build_complete.map(|t| t.duration_since(self.created).as_millis() as u64);
        let import_ms = self.import_complete.map(|t| t.duration_since(self.created).as_millis() as u64);
        let commit_ms = self.committed.map(|t| t.duration_since(self.created).as_millis() as u64);
        // Overlap: how much of import happened during consensus (before commit)
        let import_before_commit = match (self.import_complete, self.committed) {
            (Some(imp), Some(com)) if imp <= com => Some(com.duration_since(imp).as_millis() as u64),
            _ => None,
        };
        format!(
            "role={role} build={build}ms import={import}ms commit={commit}ms import_headroom={headroom}ms",
            build = build_ms.map(|v| v.to_string()).unwrap_or_else(|| "-".into()),
            import = import_ms.map(|v| v.to_string()).unwrap_or_else(|| "-".into()),
            commit = commit_ms.map(|v| v.to_string()).unwrap_or_else(|| "-".into()),
            headroom = import_before_commit.map(|v| v.to_string()).unwrap_or_else(|| "-".into()),
        )
    }

    /// Emit per-stage metrics histograms.
    fn emit_metrics(&self) {
        if let Some(t) = self.build_complete {
            histogram!("n42_pipeline_build_ms").record(t.duration_since(self.created).as_millis() as f64);
        }
        if let Some(t) = self.import_complete {
            histogram!("n42_pipeline_import_ms").record(t.duration_since(self.created).as_millis() as f64);
        }
        if let Some(t) = self.committed {
            let total = t.duration_since(self.created).as_millis() as f64;
            histogram!("n42_pipeline_total_ms").record(total);
        }
        // Overlap ratio: what fraction of total time was "useful work" vs waiting
        if let (Some(build), Some(import), Some(commit)) = (self.build_complete, self.import_complete, self.committed) {
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

pub(crate) struct CommittedBlock {
    pub(crate) view: u64,
    pub(crate) block_hash: B256,
    pub(crate) commit_qc: QuorumCertificate,
    pub(crate) payload: Vec<u8>,
}

/// Bridges the consensus engine with the P2P network layer and reth Engine API.
///
/// Runs as a background task via `tokio::select!` over network events, engine outputs,
/// pacemaker timeouts, and BlockReady signals.
pub struct ConsensusOrchestrator {
    engine: ConsensusEngine,
    network: NetworkHandle,
    /// High-priority: consensus messages only (Vote, Proposal, PrepareQC, etc.)
    consensus_event_rx: mpsc::Receiver<NetworkEvent>,
    /// Lower-priority: data events (BlockData, TX, Sync, Peers)
    net_event_rx: mpsc::Receiver<NetworkEvent>,
    output_rx: mpsc::Receiver<EngineOutput>,
    beacon_engine: Option<ConsensusEngineHandle<EthEngineTypes>>,
    payload_builder: Option<PayloadBuilderHandle<EthEngineTypes>>,
    consensus_state: Option<Arc<SharedConsensusState>>,
    head_block_hash: B256,
    block_ready_tx: mpsc::UnboundedSender<B256>,
    block_ready_rx: mpsc::UnboundedReceiver<B256>,
    fee_recipient: Address,
    slot_time: Duration,
    next_build_at: Option<Instant>,
    next_slot_timestamp: Option<u64>,
    consecutive_empty_skips: u32,
    pending_block_data: BTreeMap<B256, Vec<u8>>,
    pending_executions: HashSet<B256>,
    pending_finalization: Option<PendingFinalization>,
    /// Blocks that returned `Syncing` from new_payload, queued for retry.
    /// Each entry is `(serialized_data, retry_count)` — dropped after 3 retries.
    syncing_blocks: VecDeque<(Vec<u8>, u32)>,
    tx_import_tx: Option<mpsc::Sender<Vec<u8>>>,
    tx_broadcast_rx: Option<mpsc::Receiver<Vec<u8>>>,
    committed_blocks: VecDeque<CommittedBlock>,
    connected_peers: HashSet<PeerId>,
    sync_in_flight: bool,
    sync_started_at: Option<Instant>,
    state_file: Option<PathBuf>,
    validator_set_for_sync: Option<ValidatorSet>,
    mobile_packet_tx: Option<mpsc::Sender<(B256, u64)>>,
    leader_payload_rx: mpsc::UnboundedReceiver<(B256, Vec<u8>)>,
    leader_payload_tx: mpsc::UnboundedSender<(B256, Vec<u8>)>,
    /// Receives notifications when a background `import_and_notify` task completes.
    /// Tuple: (block_hash, view, success).
    import_done_rx: mpsc::UnboundedReceiver<(B256, u64, bool, u64)>,
    import_done_tx: mpsc::UnboundedSender<(B256, u64, bool, u64)>,
    blob_store: Option<DiskFileBlobStore>,
    /// True while a background import task is running.
    bg_import_in_flight: bool,
    /// Queue of pending imports waiting for the current bg import to finish.
    /// Entries: (serialized_block_data, block_hash, view).
    bg_import_queue: VecDeque<(Vec<u8>, B256, u64)>,
    /// Speculative build: eager import tasks notify when a block is imported to reth.
    /// Allows the next leader to start building before consensus commits the current block.
    eager_import_done_tx: mpsc::UnboundedSender<(B256, u64)>,
    eager_import_done_rx: mpsc::UnboundedReceiver<(B256, u64)>,
    /// Hash of the block whose speculative build is in progress.
    /// Set when build is triggered by eager import (before consensus commit).
    /// Cleared on ViewChanged or when finalize confirms the block.
    speculative_build_hash: Option<B256>,
    /// Parent hash for which a payload build task is currently running.
    /// Prevents duplicate builds based on the same parent, which produce different
    /// blocks at the same height and flood reth with conflicting `new_payload` calls
    /// — triggering pipeline sync and chain stalls.
    building_on_parent: Option<B256>,
    /// When the current payload build was triggered. Used to measure actual build duration
    /// in PipelineTiming (created → build_complete). Reset when build finishes or is cancelled.
    build_triggered_at: Option<Instant>,
    /// Notifies the orchestrator when a payload resolve task finishes (success or failure).
    /// Used to clear `building_on_parent` so new builds are not permanently blocked
    /// when a resolve task takes longer than the pacemaker timeout.
    build_complete_tx: mpsc::UnboundedSender<()>,
    build_complete_rx: mpsc::UnboundedReceiver<()>,
    mobile_reward_manager: Option<Arc<Mutex<MobileRewardManager>>>,
    staking_manager: Option<Arc<Mutex<StakingManager>>>,
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
    /// Per-block pipeline timing tracker. Populated incrementally as events flow
    /// through the orchestrator. Logged and emitted as metrics at commit time.
    /// Bounded to 32 entries; older entries are evicted.
    pipeline_timings: HashMap<B256, PipelineTiming>,
    /// Guards follower eager import: tracks the last block number sent to reth
    /// via new_payload. Prevents multiple eager imports for the same block number
    /// with different hashes, which triggers reth pipeline sync and chain stalls.
    eager_import_block_guard: Arc<std::sync::atomic::AtomicU64>,
    /// Fast propose mode: skip slot boundary alignment, build immediately after
    /// ViewChanged/BlockCommitted. Consensus voting naturally paces block production.
    /// Enabled by `N42_FAST_PROPOSE=1`. Default: false (grid-aligned slots).
    fast_propose: bool,
}

impl ConsensusOrchestrator {
    pub fn new(
        engine: ConsensusEngine,
        network: NetworkHandle,
        net_event_rx: mpsc::Receiver<NetworkEvent>,
        output_rx: mpsc::Receiver<EngineOutput>,
    ) -> Self {
        // Tests use this constructor; create a dummy consensus channel.
        let (_dummy_tx, consensus_event_rx) = mpsc::channel(1);
        let (block_ready_tx, block_ready_rx) = mpsc::unbounded_channel();
        let (leader_payload_tx, leader_payload_rx) = mpsc::unbounded_channel();
        let (import_done_tx, import_done_rx) = mpsc::unbounded_channel();
        let (eager_import_done_tx, eager_import_done_rx) = mpsc::unbounded_channel();
        let (build_complete_tx, build_complete_rx) = mpsc::unbounded_channel();
        Self {
            engine,
            network,
            consensus_event_rx,
            net_event_rx,
            output_rx,
            beacon_engine: None,
            payload_builder: None,
            consensus_state: None,
            head_block_hash: B256::ZERO,
            block_ready_tx,
            block_ready_rx,
            fee_recipient: Address::ZERO,
            slot_time: Duration::ZERO,
            next_build_at: None,
            next_slot_timestamp: None,
            consecutive_empty_skips: 0,
            pending_block_data: BTreeMap::new(),
            pending_executions: HashSet::new(),
            pending_finalization: None,
            syncing_blocks: VecDeque::new(),
            tx_import_tx: None,
            tx_broadcast_rx: None,
            committed_blocks: VecDeque::new(),
            connected_peers: HashSet::new(),
            sync_in_flight: false,
            sync_started_at: None,
            state_file: None,
            validator_set_for_sync: None,
            mobile_packet_tx: None,
            leader_payload_rx,
            leader_payload_tx,
            import_done_rx,
            import_done_tx,
            blob_store: None,
            bg_import_in_flight: false,
            bg_import_queue: VecDeque::new(),
            eager_import_done_tx,
            eager_import_done_rx,
            speculative_build_hash: None,
            building_on_parent: None,
            build_triggered_at: None,
            build_complete_tx,
            build_complete_rx,
            mobile_reward_manager: None,
            staking_manager: None,
            committed_block_count: 0,
            last_committed_timestamp: 0,
            view_started_at: None,
            epoch_schedule: None,
            tx_forward_buffer: Vec::new(),
            pipeline_timings: HashMap::new(),
            eager_import_block_guard: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            fast_propose: std::env::var("N42_FAST_PROPOSE")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0) > 0,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_engine_api(
        engine: ConsensusEngine,
        network: NetworkHandle,
        consensus_event_rx: mpsc::Receiver<NetworkEvent>,
        net_event_rx: mpsc::Receiver<NetworkEvent>,
        output_rx: mpsc::Receiver<EngineOutput>,
        beacon_engine: ConsensusEngineHandle<EthEngineTypes>,
        payload_builder: PayloadBuilderHandle<EthEngineTypes>,
        consensus_state: Arc<SharedConsensusState>,
        head_block_hash: B256,
        fee_recipient: Address,
    ) -> Self {
        let (block_ready_tx, block_ready_rx) = mpsc::unbounded_channel();
        let (leader_payload_tx, leader_payload_rx) = mpsc::unbounded_channel();
        let (import_done_tx, import_done_rx) = mpsc::unbounded_channel();
        let (eager_import_done_tx, eager_import_done_rx) = mpsc::unbounded_channel();
        let (build_complete_tx, build_complete_rx) = mpsc::unbounded_channel();

        let slot_time_ms: u64 = std::env::var("N42_BLOCK_INTERVAL_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let slot_time = Duration::from_millis(slot_time_ms);

        let fast_propose = std::env::var("N42_FAST_PROPOSE")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0) > 0;

        if slot_time_ms > 0 {
            info!(target: "n42::cl::orchestrator", slot_time_ms, fast_propose, "slot timing configured");
        }

        Self {
            engine,
            network,
            consensus_event_rx,
            net_event_rx,
            output_rx,
            beacon_engine: Some(beacon_engine),
            payload_builder: Some(payload_builder),
            consensus_state: Some(consensus_state),
            head_block_hash,
            block_ready_tx,
            block_ready_rx,
            fee_recipient,
            slot_time,
            next_build_at: None,
            next_slot_timestamp: None,
            consecutive_empty_skips: 0,
            pending_block_data: BTreeMap::new(),
            pending_executions: HashSet::new(),
            pending_finalization: None,
            syncing_blocks: VecDeque::new(),
            tx_import_tx: None,
            tx_broadcast_rx: None,
            committed_blocks: VecDeque::new(),
            connected_peers: HashSet::new(),
            sync_in_flight: false,
            sync_started_at: None,
            state_file: None,
            validator_set_for_sync: None,
            mobile_packet_tx: None,
            leader_payload_rx,
            leader_payload_tx,
            import_done_rx,
            import_done_tx,
            blob_store: None,
            bg_import_in_flight: false,
            bg_import_queue: VecDeque::new(),
            eager_import_done_tx,
            eager_import_done_rx,
            speculative_build_hash: None,
            building_on_parent: None,
            build_triggered_at: None,
            build_complete_tx,
            build_complete_rx,
            mobile_reward_manager: None,
            staking_manager: None,
            committed_block_count: 0,
            last_committed_timestamp: 0,
            view_started_at: None,
            epoch_schedule: None,
            tx_forward_buffer: Vec::new(),
            pipeline_timings: HashMap::new(),
            eager_import_block_guard: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            fast_propose,
        }
    }

    pub fn with_blob_store(mut self, blob_store: DiskFileBlobStore) -> Self {
        self.blob_store = Some(blob_store);
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

    pub fn with_mobile_reward_manager(mut self, mgr: Arc<Mutex<MobileRewardManager>>) -> Self {
        self.mobile_reward_manager = Some(mgr);
        self
    }

    pub fn with_staking_manager(mut self, mgr: Arc<Mutex<StakingManager>>) -> Self {
        self.staking_manager = Some(mgr);
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
    pub fn with_epoch_schedule(mut self, schedule: EpochSchedule) -> Self {
        self.epoch_schedule = Some(schedule);
        self
    }

    pub fn with_tx_pool_bridge(
        mut self,
        tx_import_tx: mpsc::Sender<Vec<u8>>,
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

        loop {
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

            // Biased select: consensus-critical channels are checked FIRST.
            // Without biased, tokio randomly permutes branch order on each poll,
            // causing consensus votes to compete with high-frequency TX events.
            // Under 20K+ TPS, this random scheduling delays R1_collect by 500-1000ms.
            tokio::select! {
                biased;

                // === Priority 1: Safety-critical ===
                _ = &mut timeout => {
                    let view = self.engine.current_view();
                    counter!("n42_view_timeouts_total").increment(1);
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

                // === Priority 3: Consensus network events (high-priority, dedicated channel) ===
                // Consensus messages (votes, proposals, QCs) are routed to a separate channel
                // so they are never queued behind high-volume TX/BlockData events.
                event = self.consensus_event_rx.recv() => {
                    match event {
                        Some(ev) => self.handle_network_event(ev).await,
                        None => {
                            // Consensus channel closed is not fatal — may be observer mode
                            debug!(target: "n42::cl::orchestrator", "consensus event channel closed");
                        }
                    }
                }

                // === Priority 4: Block build completion ===
                block_hash = self.block_ready_rx.recv() => {
                    if let Some(hash) = block_hash {
                        // Build succeeded — clear the parent guard immediately so future
                        // builds for the same parent (after view change) are not blocked.
                        self.building_on_parent = None;

                        // Pipeline: leader build complete — use build_triggered_at as the
                        // starting point so build duration is accurately measured.
                        let now = Instant::now();
                        let timing = PipelineTiming {
                            created: self.build_triggered_at.take().unwrap_or(now),
                            is_leader: true,
                            build_complete: Some(now),
                            import_complete: None,
                            committed: None,
                        };
                        self.record_pipeline_timing(hash, timing);

                        info!(target: "n42::cl::orchestrator", %hash, view = self.engine.current_view(), "payload built, feeding BlockReady to consensus");
                        if let Err(e) = self.engine.process_event(
                            n42_consensus::ConsensusEvent::BlockReady(hash)
                        ) {
                            error!(target: "n42::cl::orchestrator", error = %e, "error processing BlockReady event");
                        }
                    }
                }

                // === Priority 5: Data network events (TX forward, block data, sync, peers) ===
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

                // === Priority 6: Import and build lifecycle ===
                import_result = self.import_done_rx.recv() => {
                    if let Some((hash, view, success, block_ts)) = import_result {
                        self.handle_import_done(hash, view, success, block_ts).await;
                    }
                }

                eager_done = self.eager_import_done_rx.recv() => {
                    if let Some((hash, block_ts)) = eager_done {
                        // Pipeline: import complete — record timing.
                        if let Some(timing) = self.pipeline_timings.get_mut(&hash) {
                            timing.import_complete = Some(Instant::now());
                        }
                        self.handle_eager_import_done(hash, block_ts).await;
                    }
                }

                payload_data = self.leader_payload_rx.recv() => {
                    if let Some((hash, data)) = payload_data {
                        self.handle_leader_payload_feedback(hash, data).await;
                    }
                }

                _ = self.build_complete_rx.recv() => {
                    // Payload resolve task finished (success or failure).
                    // Clear the parent guard so future builds are not permanently blocked.
                    // On success, block_ready_rx already cleared it; this handles failure paths
                    // (build error, timeout, panic) where no BlockReady signal is sent.
                    if self.building_on_parent.is_some() {
                        debug!(target: "n42::cl::orchestrator", "build task completed, clearing building_on_parent guard");
                        self.building_on_parent = None;
                        self.build_triggered_at = None;
                    }
                }

                // === Priority 7: Slot timing ===
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
                        // Check if this is a startup delay or a retry.
                        if view <= 1 {
                            self.engine.pacemaker_mut().reset_for_view(view, 0);
                            info!(target: "n42::cl::orchestrator", "startup delay completed, triggering first payload build");
                        } else {
                            info!(target: "n42::cl::orchestrator", view, "build retry timer fired, re-attempting payload build");
                        }
                    } else {
                        info!(target: "n42::cl::orchestrator", slot_timestamp = ?slot_ts, "slot boundary reached, triggering payload build");
                    }

                    self.do_trigger_payload_build(slot_ts).await;
                }

                // === Priority 8: TX forwarding (lowest priority) ===
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
                    // Batch-drain up to 63 more from the channel.
                    if let Some(rx) = self.tx_broadcast_rx.as_mut() {
                        for _ in 0..63 {
                            match rx.try_recv() {
                                Ok(data) => { self.tx_forward_buffer.push(data); }
                                Err(_) => break,
                            }
                        }
                    }
                    // Flush if buffer is large enough.
                    if self.tx_forward_buffer.len() >= 64 {
                        self.flush_tx_forward_buffer();
                    }
                }

                _ = &mut tx_flush_timer => {
                    self.flush_tx_forward_buffer();
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
        if !self.engine.is_current_leader() || self.beacon_engine.is_none() {
            return;
        }

        let n = self.engine.validator_count() as u64;
        let startup_delay_ms: u64 = std::env::var("N42_STARTUP_DELAY_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| {
                if n <= 1 { 0 } else { 5000 + n * 500 }
            });

        if startup_delay_ms > 0 {
            let startup_delay = Duration::from_millis(startup_delay_ms);
            info!(
                target: "n42::cl::orchestrator",
                delay_ms = startup_delay_ms,
                validators = n,
                "leader for view 1, waiting for GossipSub mesh formation"
            );
            self.next_build_at = Some(Instant::now() + startup_delay);
            self.engine.pacemaker_mut().extend_deadline(startup_delay);
        } else {
            info!(target: "n42::cl::orchestrator", "this node is leader for view 1, triggering genesis payload build");
            self.schedule_payload_build().await;
        }
    }

    /// Records a pipeline timing entry for a block, with bounded map size.
    fn record_pipeline_timing(&mut self, hash: B256, timing: PipelineTiming) {
        const MAX_PIPELINE_ENTRIES: usize = 32;
        if self.pipeline_timings.len() >= MAX_PIPELINE_ENTRIES {
            // Evict oldest entry (smallest `created` timestamp).
            if let Some(oldest) = self.pipeline_timings
                .iter()
                .min_by_key(|(_, t)| t.created)
                .map(|(k, _)| *k)
            {
                self.pipeline_timings.remove(&oldest);
            }
        }
        self.pipeline_timings.insert(hash, timing);
    }

    /// Flush buffered txs to the current leader via the tx_forward protocol.
    fn flush_tx_forward_buffer(&mut self) {
        if self.tx_forward_buffer.is_empty() {
            return;
        }

        // N42_DISABLE_TX_FORWARD=1: keep txs in local pool only (for sync inject benchmarks).
        // Txs are still accepted into the local pool but never forwarded to leader.
        static DISABLE_TX_FORWARD: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let disabled = *DISABLE_TX_FORWARD.get_or_init(|| {
            std::env::var("N42_DISABLE_TX_FORWARD").map(|v| v == "1").unwrap_or(false)
        });
        if disabled {
            // Feed all buffered txs into local pool and return (no forwarding).
            if let Some(ref tx_import) = self.tx_import_tx {
                let txs = std::mem::take(&mut self.tx_forward_buffer);
                for data in txs {
                    let _ = tx_import.try_send(data);
                }
            } else {
                self.tx_forward_buffer.clear();
            }
            return;
        }

        let leader_idx = self.engine.current_leader_index();
        let validator_peers = self.network.all_validator_peers();

        // If we are the leader, feed txs directly into the local pool.
        if self.engine.is_current_leader() {
            let txs = std::mem::take(&mut self.tx_forward_buffer);
            let count = txs.len();
            if let Some(ref tx_import) = self.tx_import_tx {
                for data in txs {
                    let _ = tx_import.try_send(data);
                }
            }
            counter!("n42_tx_forward_local").increment(count as u64);
            return;
        }

        // Find the leader's PeerId.
        let leader_peer = validator_peers.iter().find(|(idx, _)| *idx == leader_idx).map(|(_, pid)| *pid);

        match leader_peer {
            Some(peer) => {
                let txs = std::mem::take(&mut self.tx_forward_buffer);
                let count = txs.len();
                debug!(target: "n42::cl::orchestrator", count, leader_idx, %peer, "flushing tx forward buffer to leader");
                let _ = self.network.forward_tx_batch(peer, txs);
                counter!("n42_tx_forward_batches").increment(1);
                counter!("n42_tx_forward_txs").increment(count as u64);
            }
            None => {
                // Leader not connected yet — fall back to keeping in buffer.
                // If buffer grows too large, drop oldest to prevent memory bloat.
                let buf_len = self.tx_forward_buffer.len();
                warn!(target: "n42::cl::orchestrator", leader_idx, buf_len, peers = validator_peers.len(), "leader peer not found for tx forward");
                if buf_len > 4096 {
                    let excess = buf_len - 2048;
                    self.tx_forward_buffer.drain(..excess);
                    counter!("n42_tx_forward_dropped").increment(excess as u64);
                    warn!(target: "n42::cl::orchestrator", leader_idx, "leader peer not found, dropped oldest txs from forward buffer");
                }
            }
        }
    }

    async fn handle_network_event(&mut self, event: NetworkEvent) {
        match event {
            NetworkEvent::ConsensusMessage { source: _, message } => {
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
                debug!(target: "n42::cl::orchestrator", msg_type, view = self.engine.current_view(), "processing consensus message");
                match self.engine.process_event(
                    n42_consensus::ConsensusEvent::Message(*message)
                ) {
                    Ok(()) => {}
                    Err(e) => {
                        if matches!(e, n42_consensus::N42ConsensusError::SafetyViolation { .. }) {
                            debug!(target: "n42::cl::orchestrator", error = %e, "benign safety check (QC ordering race)");
                        } else {
                            warn!(target: "n42::cl::orchestrator", error = %e, "error processing consensus message");
                        }
                    }
                }
            }
            NetworkEvent::PeerConnected(peer_id) => {
                info!(target: "n42::cl::orchestrator", %peer_id, "consensus peer connected");
                self.connected_peers.insert(peer_id);
                gauge!("n42_connected_peers").set(self.connected_peers.len() as f64);
            }
            NetworkEvent::PeerDisconnected(peer_id) => {
                warn!(target: "n42::cl::orchestrator", %peer_id, "consensus peer disconnected");
                self.connected_peers.remove(&peer_id);
                gauge!("n42_connected_peers").set(self.connected_peers.len() as f64);
            }
            NetworkEvent::BlockAnnouncement { source, data } => {
                tracing::debug!(target: "n42::cl::orchestrator", %source, bytes = data.len(), "received block data broadcast");
                self.handle_block_data(data).await;
            }
            NetworkEvent::TransactionReceived { source: _, data } => {
                if let Some(ref tx) = self.tx_import_tx {
                    let _ = tx.try_send(data);
                }
            }
            NetworkEvent::TxForwardReceived { source: _, txs } => {
                // Leader receives forwarded txs from validators — feed into local pool.
                let count = txs.len();
                if let Some(ref tx_import) = self.tx_import_tx {
                    for data in txs {
                        let _ = tx_import.try_send(data);
                    }
                }
                counter!("n42_tx_forward_imported").increment(count as u64);
            }
            NetworkEvent::SyncRequest { peer, request_id, request } => {
                self.handle_sync_request(peer, request_id, request);
            }
            NetworkEvent::SyncResponse { peer, response } => {
                self.handle_sync_response(peer, response).await;
            }
            NetworkEvent::SyncRequestFailed { peer, error } => {
                warn!(target: "n42::cl::orchestrator", %peer, %error, "sync request failed");
                self.sync_in_flight = false;
                self.sync_started_at = None;
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
    use alloy_primitives::{Address, B256};
    use n42_chainspec::ValidatorInfo;
    use n42_consensus::{ConsensusEngine, ValidatorSet};
    use n42_network::{NetworkCommand, NetworkHandle};
    use n42_primitives::{BlsSecretKey, ConsensusMessage, QuorumCertificate, Vote};
    use std::time::Duration;

    fn make_test_engine() -> (ConsensusEngine, mpsc::Receiver<EngineOutput>) {
        let sk = BlsSecretKey::random().unwrap();
        let pk = sk.public_key();

        let validator_info = ValidatorInfo {
            address: Address::with_last_byte(1),
            bls_public_key: pk,
        };

        let vs = ValidatorSet::new(&[validator_info], 0);
        let (output_tx, output_rx) = mpsc::channel(1024);

        let engine = ConsensusEngine::new(0, sk, vs, 60000, 120000, output_tx);
        (engine, output_rx)
    }

    /// Returns (handle, normal_cmd_rx, priority_cmd_rx).
    fn make_test_network() -> (
        NetworkHandle,
        mpsc::UnboundedReceiver<NetworkCommand>,
        mpsc::UnboundedReceiver<NetworkCommand>,
    ) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (ptx, prx) = mpsc::unbounded_channel();
        (NetworkHandle::new(cmd_tx, ptx), cmd_rx, prx)
    }

    #[test]
    fn test_orchestrator_construction() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let _ = ConsensusOrchestrator::new(engine, network, net_event_rx, output_rx);
    }

    #[test]
    fn test_engine_initial_state() {
        let (engine, _output_rx) = make_test_engine();
        assert_eq!(engine.current_view(), 1, "initial view should be 1");
        assert!(engine.is_current_leader(), "single validator should be leader");
    }

    #[tokio::test]
    async fn test_handle_engine_output_broadcast() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, mut cmd_rx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let mut orch = ConsensusOrchestrator::new(engine, network, net_event_rx, output_rx);

        let sk = BlsSecretKey::random().unwrap();
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
    async fn test_handle_engine_output_send_to_validator() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, mut cmd_rx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let mut orch = ConsensusOrchestrator::new(engine, network, net_event_rx, output_rx);

        let sk = BlsSecretKey::random().unwrap();
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
    async fn test_handle_engine_output_execute_block() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let mut orch = ConsensusOrchestrator::new(engine, network, net_event_rx, output_rx);
        orch.handle_engine_output(EngineOutput::ExecuteBlock(B256::repeat_byte(0xCC)))
            .await;
    }

    #[tokio::test]
    async fn test_handle_engine_output_block_committed() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let mut orch = ConsensusOrchestrator::new(engine, network, net_event_rx, output_rx);

        let commit_qc = QuorumCertificate::genesis();
        orch.handle_engine_output(EngineOutput::BlockCommitted {
            view: 1,
            block_hash: B256::repeat_byte(0xDD),
            commit_qc,
        })
        .await;

        assert_eq!(
            orch.head_block_hash,
            B256::repeat_byte(0xDD),
            "head should be updated after commit"
        );
    }

    #[tokio::test]
    async fn test_handle_engine_output_view_changed() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let mut orch = ConsensusOrchestrator::new(engine, network, net_event_rx, output_rx);
        orch.handle_engine_output(EngineOutput::ViewChanged { new_view: 5 })
            .await;
    }

    #[tokio::test]
    async fn test_orchestrator_exits_on_net_event_channel_close() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, _prx) = make_test_network();
        let (net_event_tx, net_event_rx) = mpsc::channel::<NetworkEvent>(8192);
        let orch = ConsensusOrchestrator::new(engine, network, net_event_rx, output_rx);
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
        let orch = ConsensusOrchestrator::new(engine, network, net_event_rx, output_rx);

        let peer_id = libp2p::PeerId::random();
        net_event_tx.try_send(NetworkEvent::PeerConnected(peer_id)).unwrap();
        drop(net_event_tx);

        let result = tokio::time::timeout(Duration::from_secs(5), orch.run()).await;
        assert!(result.is_ok(), "orchestrator should exit after processing events");
    }

    fn make_test_orchestrator_with_state(
        state: Option<Arc<SharedConsensusState>>,
    ) -> ConsensusOrchestrator {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let mut orch = ConsensusOrchestrator::new(engine, network, net_event_rx, output_rx);
        orch.consensus_state = state;
        orch
    }

    #[tokio::test]
    async fn test_block_committed_updates_shared_state() {
        let vs = ValidatorSet::new(&[], 0);
        let state = Arc::new(SharedConsensusState::new(vs));
        let mut orch = make_test_orchestrator_with_state(Some(state.clone()));

        assert!(state.load_committed_qc().is_none(), "should start with no QC");

        let commit_qc = QuorumCertificate::genesis();
        orch.handle_engine_output(EngineOutput::BlockCommitted {
            view: 1,
            block_hash: B256::repeat_byte(0xEE),
            commit_qc,
        })
        .await;

        assert!(state.load_committed_qc().is_some(), "should have QC after commit");
    }

    #[tokio::test]
    async fn test_block_ready_channel() {
        let mut orch = make_test_orchestrator_with_state(None);
        let block_ready_tx = orch.block_ready_tx.clone();

        let test_hash = B256::repeat_byte(0xFF);
        block_ready_tx.send(test_hash).unwrap();

        let received = orch.block_ready_rx.try_recv();
        assert!(received.is_ok(), "should receive BlockReady hash");
        assert_eq!(received.unwrap(), test_hash);
    }

    #[test]
    fn test_sync_timeout_resets_in_flight() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let mut orch = ConsensusOrchestrator::new(engine, network, net_event_rx, output_rx);

        orch.connected_peers.insert(libp2p::PeerId::random());
        orch.sync_in_flight = true;
        orch.sync_started_at = Some(Instant::now() - Duration::from_secs(60));

        orch.initiate_sync(1, 10);

        assert!(orch.sync_in_flight, "new sync request should be in flight");
        let started = orch.sync_started_at.expect("sync_started_at should be set");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "sync_started_at should be recent after timeout reset"
        );
    }
}
