use crate::consensus_state::SharedConsensusState;
use crate::persistence::{self, ConsensusSnapshot};
use alloy_primitives::{Address, B256};
use alloy_rpc_types_engine::{ForkchoiceState, PayloadAttributes, PayloadStatusEnum};
use n42_consensus::{ConsensusEngine, ConsensusEvent, EngineOutput, verify_commit_qc, ValidatorSet};
use n42_network::{BlockSyncResponse, NetworkEvent, NetworkHandle, PeerId, SyncBlock};
use n42_primitives::QuorumCertificate;
use reth_ethereum_engine_primitives::EthEngineTypes;
use reth_node_builder::ConsensusEngineHandle;
use reth_payload_builder::PayloadBuilderHandle;
use reth_payload_primitives::{EngineApiMessageVersion, PayloadKind, PayloadTypes};
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
use metrics::{counter, gauge};
use tracing::{debug, error, info, warn};

/// Block data broadcast message sent via /n42/blocks/1 GossipSub topic.
///
/// The leader broadcasts this alongside the Proposal so followers can
/// import the block before voting.
///
/// Design reference: Aptos Raptr uses a separate TCP channel for data;
/// Ethereum uses a separate GossipSub topic for beacon_block.
#[derive(serde::Serialize, serde::Deserialize)]
struct BlockDataBroadcast {
    /// Block hash for matching with Proposal.
    block_hash: B256,
    /// Consensus view number.
    view: u64,
    /// JSON-serialized ExecutionData (reth Engine API format).
    /// Using JSON because reth's payload types implement serde for the Engine API,
    /// and some types use #[serde(untagged)] which bincode doesn't handle reliably.
    payload_json: Vec<u8>,
}

/// Deferred finalization for blocks committed by consensus but not yet imported into reth.
/// This handles the f=0 race condition where Decide arrives before BlockData.
struct PendingFinalization {
    view: u64,
    block_hash: B256,
    #[allow(dead_code)]
    commit_qc: QuorumCertificate,
}

/// A committed block stored in the ring buffer for serving sync requests.
struct CommittedBlock {
    view: u64,
    block_hash: B256,
    commit_qc: QuorumCertificate,
    /// Serialized block payload (JSON) for the execution layer.
    payload: Vec<u8>,
}

/// Default maximum number of committed blocks retained for sync (ring buffer).
/// At 8-second slots, 10,000 blocks ≈ ~22 hours of history.
/// Configurable via N42_SYNC_BUFFER_SIZE environment variable.
fn max_committed_blocks() -> usize {
    std::env::var("N42_SYNC_BUFFER_SIZE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10_000)
}

/// Timeout for a state sync request. If no response arrives within this duration,
/// the in-flight flag is reset so a new request can be sent.
/// Configurable via `N42_SYNC_TIMEOUT_SECS` environment variable.
fn sync_request_timeout() -> Duration {
    let secs = std::env::var("N42_SYNC_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(30);
    Duration::from_secs(secs)
}

/// Maximum consecutive empty block skips before producing a block anyway.
/// At 8s slots, 3 skips = 24 seconds max gap before liveness kicks in.
/// Configurable via `N42_MAX_EMPTY_SKIPS` environment variable.
fn max_consecutive_empty_skips() -> u32 {
    std::env::var("N42_MAX_EMPTY_SKIPS")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(3)
}

/// Bridges the consensus engine with the P2P network layer and reth Engine API.
///
/// The orchestrator runs as a background task (`spawn_critical`) and drives
/// four concurrent event sources via `tokio::select!`:
///
/// 1. **Network events** -> translated to `ConsensusEvent::Message` -> fed into the engine
/// 2. **Engine outputs** -> dispatched to network / Engine API
/// 3. **Pacemaker timeout** -> triggers `engine.on_timeout()` for view change
/// 4. **BlockReady channel** -> payload built by PayloadBuilder, fed to consensus engine
pub struct ConsensusOrchestrator {
    /// The HotStuff-2 consensus engine (event-driven state machine).
    engine: ConsensusEngine,
    /// Handle for sending messages to the P2P network.
    network: NetworkHandle,
    /// Receiver for events from the network service.
    net_event_rx: mpsc::UnboundedReceiver<NetworkEvent>,
    /// Receiver for outputs from the consensus engine.
    output_rx: mpsc::Receiver<EngineOutput>,
    /// Handle to the reth beacon consensus engine for block finalization.
    beacon_engine: Option<ConsensusEngineHandle<EthEngineTypes>>,
    /// Handle to the payload builder service for triggering block production.
    payload_builder: Option<PayloadBuilderHandle<EthEngineTypes>>,
    /// Shared consensus state (updated on block commit for PayloadBuilder to read).
    consensus_state: Option<Arc<SharedConsensusState>>,
    /// Current head block hash (genesis at startup, updated on commit).
    head_block_hash: B256,
    /// Sender for BlockReady events (cloned into spawned payload tasks).
    block_ready_tx: mpsc::UnboundedSender<B256>,
    /// Receiver for BlockReady events from payload build tasks.
    block_ready_rx: mpsc::UnboundedReceiver<B256>,
    /// Fee recipient address for payload attributes.
    fee_recipient: Address,
    /// Slot duration for wall-clock-aligned block production.
    /// When zero, blocks are built immediately (legacy behavior).
    /// When non-zero, builds are triggered at fixed wall-clock boundaries:
    ///   boundary = ceil(now / slot_time) * slot_time
    /// This produces precise block intervals regardless of build overhead.
    slot_time: Duration,
    /// Scheduled tokio Instant for the next payload build.
    next_build_at: Option<Instant>,
    /// The slot-aligned timestamp (Unix seconds) for the next block.
    /// Used as the block's timestamp to ensure perfectly regular intervals.
    next_slot_timestamp: Option<u64>,
    /// Count of consecutive empty block skips (for empty block optimization).
    consecutive_empty_skips: u32,
    /// Cache of block data received from network but not yet requested by engine.
    /// Key: block_hash, Value: raw broadcast bytes.
    /// Bounded to 16 entries to prevent memory issues.
    pending_block_data: BTreeMap<B256, Vec<u8>>,
    /// Block hashes the engine has requested (via ExecuteBlock) but data not yet received.
    pending_executions: HashSet<B256>,
    /// Deferred block finalization when Decide arrives before block data.
    /// The committed block hasn't been imported into reth yet.
    pending_finalization: Option<PendingFinalization>,
    /// Sender for forwarding received transactions to TxPoolBridge.
    tx_import_tx: Option<mpsc::Sender<Vec<u8>>>,
    /// Receiver for transactions to broadcast from TxPoolBridge.
    tx_broadcast_rx: Option<mpsc::Receiver<Vec<u8>>>,
    /// Ring buffer of recently committed blocks for serving sync requests from peers.
    /// Bounded to max_committed_blocks() entries (~250MB worst case at 250KB avg blocks).
    committed_blocks: VecDeque<CommittedBlock>,
    /// Set of connected peer IDs for sync target selection.
    connected_peers: HashSet<PeerId>,
    /// Whether a sync request is currently in-flight (prevents duplicate requests).
    sync_in_flight: bool,
    /// When the current sync request was initiated (for timeout detection).
    sync_started_at: Option<Instant>,
    /// Path for persisting consensus state snapshots (atomic JSON write).
    /// When set, a snapshot is saved after each BlockCommitted event.
    state_file: Option<PathBuf>,
    /// Validator set reference for verifying QCs during state sync.
    validator_set_for_sync: Option<ValidatorSet>,
    /// Sender for notifying the mobile packet generation task about committed blocks.
    /// When set, sends `(block_hash, block_number)` after each BlockCommitted event.
    mobile_packet_tx: Option<mpsc::Sender<(B256, u64)>>,
    /// Receiver for leader payload data sent back from the spawned build task.
    /// The leader's build task broadcasts block data to followers but the orchestrator
    /// needs a copy to populate committed_blocks for state sync.
    leader_payload_rx: mpsc::UnboundedReceiver<(B256, Vec<u8>)>,
    /// Sender cloned into spawned build tasks for leader payload feedback.
    leader_payload_tx: mpsc::UnboundedSender<(B256, Vec<u8>)>,
}

impl ConsensusOrchestrator {
    /// Creates a new orchestrator (basic mode, without Engine API integration).
    /// Used for testing and non-block-producing scenarios.
    pub fn new(
        engine: ConsensusEngine,
        network: NetworkHandle,
        net_event_rx: mpsc::UnboundedReceiver<NetworkEvent>,
        output_rx: mpsc::Receiver<EngineOutput>,
    ) -> Self {
        let (block_ready_tx, block_ready_rx) = mpsc::unbounded_channel();
        let (leader_payload_tx, leader_payload_rx) = mpsc::unbounded_channel();
        Self {
            engine,
            network,
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
        }
    }

    /// Creates an orchestrator with full Engine API integration.
    pub fn with_engine_api(
        engine: ConsensusEngine,
        network: NetworkHandle,
        net_event_rx: mpsc::UnboundedReceiver<NetworkEvent>,
        output_rx: mpsc::Receiver<EngineOutput>,
        beacon_engine: ConsensusEngineHandle<EthEngineTypes>,
        payload_builder: PayloadBuilderHandle<EthEngineTypes>,
        consensus_state: Arc<SharedConsensusState>,
        head_block_hash: B256,
        fee_recipient: Address,
    ) -> Self {
        let (block_ready_tx, block_ready_rx) = mpsc::unbounded_channel();
        let (leader_payload_tx, leader_payload_rx) = mpsc::unbounded_channel();

        // Read slot time from environment variable (default: 0 = immediate build).
        // When non-zero, blocks are produced at wall-clock-aligned boundaries
        // (e.g., 4000ms → builds trigger at Unix times divisible by 4 seconds).
        let slot_time_ms: u64 = std::env::var("N42_BLOCK_INTERVAL_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let slot_time = Duration::from_millis(slot_time_ms);

        if slot_time_ms > 0 {
            info!(slot_time_ms, "wall-clock-aligned slot timing configured");
        }

        Self {
            engine,
            network,
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
        }
    }

    /// Configures the path for persisting consensus state snapshots.
    pub fn with_state_persistence(mut self, path: PathBuf) -> Self {
        self.state_file = Some(path);
        self
    }

    /// Configures the validator set used for verifying QCs during state sync.
    pub fn with_validator_set(mut self, vs: ValidatorSet) -> Self {
        self.validator_set_for_sync = Some(vs);
        self
    }

    /// Configures the mobile packet generation channel.
    /// When set, the orchestrator sends `(block_hash, block_number)` after each
    /// BlockCommitted event, allowing a separate task to generate and broadcast
    /// VerificationPackets to mobile verifiers.
    pub fn with_mobile_packet_tx(
        mut self,
        tx: mpsc::Sender<(B256, u64)>,
    ) -> Self {
        self.mobile_packet_tx = Some(tx);
        self
    }

    /// Configures the transaction pool bridge channels.
    /// `tx_import_tx`: sends received network transactions to TxPoolBridge for import.
    /// `tx_broadcast_rx`: receives new local transactions from TxPoolBridge for broadcast.
    pub fn with_tx_pool_bridge(
        mut self,
        tx_import_tx: mpsc::Sender<Vec<u8>>,
        tx_broadcast_rx: mpsc::Receiver<Vec<u8>>,
    ) -> Self {
        self.tx_import_tx = Some(tx_import_tx);
        self.tx_broadcast_rx = Some(tx_broadcast_rx);
        self
    }

    /// Runs the orchestrator event loop.
    ///
    /// This method never returns under normal operation. It should be
    /// spawned as a critical background task.
    pub async fn run(mut self) {
        info!(
            view = self.engine.current_view(),
            phase = ?self.engine.current_phase(),
            head = %self.head_block_hash,
            "consensus orchestrator started"
        );

        // On startup, if this node is the leader for view 1, schedule first payload build.
        // A startup delay is needed to allow GossipSub mesh formation before proposing.
        // Without this, the leader broadcasts proposals before peers have joined the mesh,
        // and follower votes can't route back to the leader (no mesh paths yet).
        if self.engine.is_current_leader() && self.beacon_engine.is_some() {
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
                    delay_ms = startup_delay_ms,
                    validators = n,
                    "leader for view 1, waiting for GossipSub mesh formation"
                );
                // Schedule first build after startup delay.
                self.next_build_at = Some(Instant::now() + startup_delay);
                // Extend pacemaker deadline so it doesn't fire during the startup delay.
                // The full base_timeout starts AFTER the startup delay.
                self.engine.pacemaker_mut().extend_deadline(startup_delay);
            } else {
                info!("this node is leader for view 1, triggering genesis payload build");
                self.schedule_payload_build().await;
            }
        }

        loop {
            let timeout = self.engine.pacemaker().timeout_sleep();
            tokio::pin!(timeout);

            // Copy the deadline so the async block doesn't borrow self.
            let next_build_deadline = self.next_build_at;
            let build_timer = async {
                match next_build_deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending().await,
                }
            };
            tokio::pin!(build_timer);

            tokio::select! {
                // Branch 1: Pacemaker timeout -> trigger view change
                _ = &mut timeout => {
                    let view = self.engine.current_view();
                    counter!("n42_view_timeouts_total").increment(1);
                    warn!(view, "pacemaker timeout, initiating view change");
                    if let Err(e) = self.engine.on_timeout() {
                        error!(view, error = %e, "error handling timeout");
                    }
                }

                // Branch 2: Network event -> feed to consensus engine
                event = self.net_event_rx.recv() => {
                    match event {
                        Some(NetworkEvent::ConsensusMessage { source: _, message }) => {
                            counter!("n42_consensus_messages_received").increment(1);
                            if let Err(e) = self.engine.process_event(
                                ConsensusEvent::Message(message)
                            ) {
                                warn!(error = %e, "error processing consensus message");
                            }
                        }
                        Some(NetworkEvent::PeerConnected(peer_id)) => {
                            info!(%peer_id, "consensus peer connected");
                            self.connected_peers.insert(peer_id);
                            gauge!("n42_connected_peers").set(self.connected_peers.len() as f64);
                        }
                        Some(NetworkEvent::PeerDisconnected(peer_id)) => {
                            warn!(%peer_id, "consensus peer disconnected");
                            self.connected_peers.remove(&peer_id);
                            gauge!("n42_connected_peers").set(self.connected_peers.len() as f64);
                        }
                        Some(NetworkEvent::BlockAnnouncement { source, data }) => {
                            debug!(%source, bytes = data.len(), "received block data broadcast");
                            self.handle_block_data(data).await;
                        }
                        Some(NetworkEvent::TransactionReceived { source, data }) => {
                            debug!(%source, bytes = data.len(), "received transaction from mempool");
                            // Forward to TxPoolBridge via channel if configured.
                            if let Some(ref tx) = self.tx_import_tx {
                                let _ = tx.try_send(data);
                            }
                        }
                        Some(NetworkEvent::SyncRequest { peer, request_id, request }) => {
                            self.handle_sync_request(peer, request_id, request);
                        }
                        Some(NetworkEvent::SyncResponse { peer, response }) => {
                            self.handle_sync_response(peer, response).await;
                        }
                        Some(NetworkEvent::SyncRequestFailed { peer, error }) => {
                            warn!(%peer, %error, "sync request failed");
                            self.sync_in_flight = false;
                            self.sync_started_at = None;
                        }
                        Some(_) => {
                            // Verification receipts are handled by dedicated subsystems.
                        }
                        None => {
                            info!("network event channel closed, shutting down orchestrator");
                            break;
                        }
                    }
                }

                // Branch 3: Engine output -> dispatch to network / Engine API
                output = self.output_rx.recv() => {
                    match output {
                        Some(engine_output) => {
                            self.handle_engine_output(engine_output).await;
                        }
                        None => {
                            info!("engine output channel closed, shutting down orchestrator");
                            break;
                        }
                    }
                }

                // Branch 4: BlockReady from payload builder -> feed to consensus engine
                block_hash = self.block_ready_rx.recv() => {
                    if let Some(hash) = block_hash {
                        info!(%hash, view = self.engine.current_view(), "payload built, feeding BlockReady to consensus");
                        if let Err(e) = self.engine.process_event(
                            ConsensusEvent::BlockReady(hash)
                        ) {
                            error!(error = %e, "error processing BlockReady event");
                        }
                    }
                }

                // Branch 5: Transaction from TxPoolBridge -> broadcast to network
                tx_data = async {
                    match self.tx_broadcast_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    if let Some(data) = tx_data {
                        if let Err(e) = self.network.broadcast_transaction(data) {
                            debug!(error = %e, "failed to broadcast transaction");
                        }
                    }
                }

                // Branch 6: Leader payload feedback — populate committed_blocks for sync
                payload_data = self.leader_payload_rx.recv() => {
                    if let Some((hash, data)) = payload_data {
                        // Find the committed block entry and populate its payload
                        if let Some(block) = self.committed_blocks.iter_mut().rev()
                            .find(|b| b.block_hash == hash)
                        {
                            if block.payload.is_empty() {
                                // Decode the broadcast envelope to extract payload_json
                                if let Ok(broadcast) = bincode::deserialize::<BlockDataBroadcast>(&data) {
                                    block.payload = broadcast.payload_json;
                                    debug!(%hash, "populated committed block payload from leader build task");
                                }
                            }
                        }
                    }
                }

                // Branch 7: Slot boundary / startup delay reached, trigger payload build
                _ = &mut build_timer => {
                    let slot_ts = self.next_slot_timestamp.take();
                    self.next_build_at = None;

                    if slot_ts.is_none() {
                        // Startup delay completed (no slot timestamp was set).
                        // Reset pacemaker so the full base_timeout starts now.
                        let view = self.engine.current_view();
                        self.engine.pacemaker_mut().reset_for_view(view, 0);
                        info!("startup delay completed, triggering first payload build");
                    } else {
                        info!(slot_timestamp = ?slot_ts, "slot boundary reached, triggering payload build");
                    }

                    self.do_trigger_payload_build(slot_ts).await;
                }
            }
        }

        // Graceful shutdown: persist final consensus state so that a restarted
        // node resumes from the latest committed view instead of replaying.
        info!(
            view = self.engine.current_view(),
            "orchestrator shutting down, persisting final state"
        );
        if let Some(ref path) = self.state_file {
            let scheduled_epoch = self.engine.epoch_manager()
                .staged_epoch_info()
                .map(|(epoch, validators, f)| (epoch, validators.to_vec(), f));
            let snapshot = ConsensusSnapshot {
                version: 1,
                current_view: self.engine.current_view(),
                locked_qc: self.engine.locked_qc().clone(),
                last_committed_qc: self.engine.last_committed_qc().clone(),
                consecutive_timeouts: self.engine.consecutive_timeouts(),
                scheduled_epoch_transition: scheduled_epoch,
            };
            if let Err(e) = persistence::save_consensus_state(path, &snapshot) {
                error!(error = %e, "failed to persist final consensus state on shutdown");
            } else {
                info!(view = snapshot.current_view, "final consensus state persisted");
            }
        }
    }

    /// Checks if the most recent committed blocks had empty payloads,
    /// indicating low network activity where empty block skipping is appropriate.
    fn recent_blocks_empty(&self) -> bool {
        // Check last 3 committed blocks (or fewer if we haven't committed that many).
        let check_count = 3.min(self.committed_blocks.len());
        if check_count == 0 {
            return false; // No history yet, don't skip.
        }
        self.committed_blocks.iter().rev().take(check_count)
            .all(|b| b.payload.is_empty() || {
                // Check if the payload contains a block with zero transactions.
                // Empty payload JSON typically encodes a block with empty transactions list.
                // We use a heuristic: payload under 512 bytes is likely an empty block.
                b.payload.len() < 512
            })
    }

    /// Schedules a payload build using wall-clock-aligned slot timing.
    ///
    /// If slot_time > 0, computes the next wall-clock boundary (a time where
    /// `unix_ms % slot_time_ms == 0`) and schedules the build for that instant.
    /// This ensures all nodes with synchronized clocks produce blocks at the
    /// same fixed intervals, regardless of build overhead or consensus latency.
    ///
    /// If slot_time == 0, triggers immediately (legacy behavior).
    ///
    /// Empty block optimization: when the mempool is likely empty (recent blocks
    /// were empty), skip up to max_consecutive_empty_skips() slots before producing
    /// an empty block to maintain liveness.
    async fn schedule_payload_build(&mut self) {
        // Empty block skip optimization.
        if self.recent_blocks_empty() && self.consecutive_empty_skips < max_consecutive_empty_skips() {
            self.consecutive_empty_skips += 1;
            counter!("n42_empty_block_skips_total").increment(1);
            debug!(
                skip_count = self.consecutive_empty_skips,
                max = max_consecutive_empty_skips(),
                "skipping empty block proposal (low activity)"
            );
            // Re-schedule at the next slot boundary to check again.
            if !self.slot_time.is_zero() {
                let (slot_ts, delay) = self.next_slot_boundary();
                self.next_build_at = Some(Instant::now() + delay);
                self.next_slot_timestamp = Some(slot_ts);
            }
            return;
        }
        // Reset skip counter when we decide to build.
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
    ///
    /// Slot boundaries are fixed points in time defined by:
    ///   `boundary = ceil(now_ms / slot_ms) * slot_ms`
    ///
    /// This is the same approach used by BSC, Polygon, and Ethereum 2.0:
    /// all validators with synchronized clocks see identical slot boundaries.
    fn next_slot_boundary(&self) -> (u64, Duration) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let now_ms = now.as_millis() as u64;
        let slot_ms = self.slot_time.as_millis() as u64;

        // Guard against misconfiguration: slot_ms must be positive.
        if slot_ms == 0 {
            tracing::error!("slot_time is zero, defaulting to 1-second delay");
            return (now_ms / 1000 + 1, Duration::from_secs(1));
        }

        // Find the next boundary: smallest multiple of slot_ms that is > now_ms.
        let current_slot = now_ms / slot_ms;
        let next_boundary_ms = (current_slot + 1) * slot_ms;
        let delay_ms = next_boundary_ms - now_ms;

        // Block timestamp = slot boundary time in seconds.
        let slot_timestamp = next_boundary_ms / 1000;
        (slot_timestamp, Duration::from_millis(delay_ms))
    }

    /// Triggers payload building by calling fork_choice_updated with PayloadAttributes.
    ///
    /// When `slot_timestamp` is provided, it is used as the block's timestamp
    /// (for wall-clock-aligned slot timing). Otherwise, the current system time
    /// is used (legacy immediate mode).
    ///
    /// When the payload is ready, a spawned task sends the block hash to
    /// `block_ready_rx`, which feeds it to the consensus engine as BlockReady.
    async fn do_trigger_payload_build(&self, slot_timestamp: Option<u64>) {
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

        info!(
            head = %self.head_block_hash,
            timestamp,
            "triggering payload build via fork_choice_updated"
        );

        match beacon_engine
            .fork_choice_updated(fcu_state, Some(attrs), EngineApiMessageVersion::default())
            .await
        {
            Ok(result) => {
                debug!(status = ?result.payload_status.status, "fork_choice_updated response");

                if let Some(payload_id) = result.payload_id {
                    info!(?payload_id, "payload building started, spawning resolve task");

                    // Spawn an async task that waits for the payload to be built,
                    // inserts it into reth via newPayload, then sends the block
                    // hash through the channel and broadcasts block data to followers.
                    let tx = self.block_ready_tx.clone();
                    let engine_handle = beacon_engine.clone();
                    let network = self.network.clone();
                    let leader_payload_tx = self.leader_payload_tx.clone();
                    let current_view = self.engine.current_view();
                    tokio::spawn(async move {
                        // Give the builder time to pack transactions from the pool.
                        tokio::time::sleep(Duration::from_millis(500)).await;

                        // R1 fix: timeout prevents task leak if resolve_kind hangs.
                        // 30s covers extreme cases (8s slot, builds typically < 1s).
                        let resolve_result = tokio::time::timeout(
                            Duration::from_secs(30),
                            payload_builder.resolve_kind(payload_id, PayloadKind::WaitForPending),
                        ).await;

                        let payload_opt = match resolve_result {
                            Ok(result) => result,
                            Err(_) => {
                                error!("payload build timed out after 30s, task will exit");
                                return;
                            }
                        };

                        match payload_opt {
                            Some(Ok(payload)) => {
                                let hash = payload.block().hash();

                                // Insert the block into reth via Engine API newPayload.
                                // Without this call, reth has no knowledge of the built
                                // block and will report "syncing" on fork_choice_updated.
                                let execution_data =
                                    <EthEngineTypes as PayloadTypes>::block_to_payload(
                                        payload.block().clone(),
                                    );

                                // Serialize the execution payload for broadcasting to followers.
                                // Uses JSON because reth Engine API types are designed for JSON
                                // serialization (some use #[serde(untagged)] which bincode can't handle).
                                let payload_json = match serde_json::to_vec(&execution_data) {
                                    Ok(json) => json,
                                    Err(e) => {
                                        error!(%hash, error = %e, "CRITICAL: failed to serialize execution payload, block will not be broadcast");
                                        return;
                                    }
                                };

                                match engine_handle.new_payload(execution_data).await {
                                    Ok(status) => {
                                        match status.status {
                                            PayloadStatusEnum::Valid |
                                            PayloadStatusEnum::Accepted => {
                                                info!(
                                                    %hash,
                                                    status = ?status.status,
                                                    "newPayload valid, sending BlockReady"
                                                );
                                                let _ = tx.send(hash);

                                                // Broadcast block data to followers via /n42/blocks/1 topic.
                                                // Reference: Aptos Raptr uses separate TCP channel;
                                                // Ethereum uses separate GossipSub topic for beacon_block.
                                                if !payload_json.is_empty() {
                                                    let broadcast = BlockDataBroadcast {
                                                        block_hash: hash,
                                                        view: current_view,
                                                        payload_json,
                                                    };
                                                    match bincode::serialize(&broadcast) {
                                                        Ok(encoded) => {
                                                            info!(
                                                                %hash,
                                                                bytes = encoded.len(),
                                                                "broadcasting block data to followers"
                                                            );
                                                            if let Err(e) = network.announce_block(encoded.clone()) {
                                                                warn!(error = %e, "failed to broadcast block data");
                                                            }
                                                            // Feed payload back to orchestrator for state sync buffer
                                                            let _ = leader_payload_tx.send((hash, encoded));
                                                        }
                                                        Err(e) => {
                                                            error!(error = %e, "failed to serialize block data broadcast");
                                                        }
                                                    }
                                                }
                                            }
                                            _ => {
                                                error!(
                                                    %hash,
                                                    status = ?status.status,
                                                    "newPayload returned non-valid status, block rejected"
                                                );
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        error!(
                                            %hash,
                                            error = %e,
                                            "newPayload failed, block not inserted"
                                        );
                                    }
                                }
                            }
                            Some(Err(e)) => {
                                error!(error = %e, "payload build failed");
                            }
                            None => {
                                warn!("payload not found (already resolved or expired)");
                            }
                        }
                    });
                } else {
                    warn!("fork_choice_updated did not return payload_id");
                }
            }
            Err(e) => {
                error!(error = %e, "fork_choice_updated failed");
            }
        }
    }

    /// Dispatches an engine output to the appropriate destination.
    async fn handle_engine_output(&mut self, output: EngineOutput) {
        match output {
            EngineOutput::BroadcastMessage(msg) => {
                if let Err(e) = self.network.broadcast_consensus(msg) {
                    error!(error = %e, "failed to broadcast consensus message");
                }
            }
            EngineOutput::SendToValidator(target, msg) => {
                // Try directed send if we know the target peer.
                if let Some(peer_id) = self.network.validator_peer(target) {
                    if let Err(e) = self.network.send_direct(peer_id, msg.clone()) {
                        warn!(target_validator = target, error = %e, "direct send failed, falling back to broadcast");
                        let _ = self.network.broadcast_consensus(msg);
                    }
                } else {
                    // Peer not yet identified, fall back to broadcast.
                    if let Err(e) = self.network.broadcast_consensus(msg) {
                        error!(error = %e, "failed to send message to validator (broadcast fallback)");
                    }
                }
            }
            EngineOutput::ExecuteBlock(block_hash) => {
                debug!(%block_hash, pending_data = self.pending_block_data.contains_key(&block_hash), "ExecuteBlock requested");

                // Check if we already have the block data cached (BlockData arrived before Proposal)
                if let Some(data) = self.pending_block_data.remove(&block_hash) {
                    match bincode::deserialize::<BlockDataBroadcast>(&data) {
                        Ok(broadcast) => {
                            self.import_and_notify(broadcast).await;
                        }
                        Err(e) => {
                            warn!(%block_hash, error = %e, "failed to deserialize cached block data");
                        }
                    }
                } else {
                    // Wait for BlockAnnouncement to arrive from the leader
                    self.pending_executions.insert(block_hash);
                }
            }
            EngineOutput::BlockCommitted { view, block_hash, commit_qc } => {
                counter!("n42_blocks_committed_total").increment(1);
                gauge!("n42_consensus_view").set(view as f64);
                info!(view, %block_hash, "block committed by consensus");

                // Update shared consensus state (always safe, QC is valid regardless of reth state)
                if let Some(ref state) = self.consensus_state {
                    state.update_committed_qc(commit_qc.clone());
                    state.notify_block_committed(block_hash, view);
                }

                // Store committed block in ring buffer for serving sync requests.
                // The payload is populated later when block data becomes available
                // (via import_and_notify for followers, or the build task for leaders).
                self.store_committed_block(view, block_hash, commit_qc.clone());

                // Always advance head — consensus has committed this block.
                // The next payload build must reference it as parent regardless
                // of whether reth has finished importing it yet.
                self.head_block_hash = block_hash;

                // Try to finalize in reth
                if let Some(ref engine_handle) = self.beacon_engine {
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
                        // Case A: Block already in reth (leader path, or fast follower)
                        debug!(view, %block_hash, "block finalized in reth");
                        self.pending_block_data.clear();
                        self.pending_executions.clear();

                        // Notify mobile packet generation (block is now in reth)
                        if let Some(ref tx) = self.mobile_packet_tx {
                            let _ = tx.try_send((block_hash, view));
                        }
                    } else if let Some(data) = self.pending_block_data.remove(&block_hash) {
                        // Case B: BlockData arrived first, cached but not imported
                        info!(view, %block_hash, "block data cached, importing for deferred finalization");
                        match bincode::deserialize::<BlockDataBroadcast>(&data) {
                            Ok(broadcast) => {
                                self.import_and_notify(broadcast).await;
                                self.pending_block_data.clear();
                                self.pending_executions.clear();

                                // Notify mobile packet generation (block imported)
                                if let Some(ref tx) = self.mobile_packet_tx {
                                    let _ = tx.try_send((block_hash, view));
                                }
                            }
                            Err(e) => {
                                warn!(%block_hash, error = %e, "failed to deserialize cached block data");
                                self.pending_finalization = Some(PendingFinalization { view, block_hash, commit_qc });
                                self.pending_executions.insert(block_hash);
                            }
                        }
                    } else {
                        // Case C: Block data not yet received (Decide arrived before BlockData)
                        // Defer finalization until block data arrives via handle_block_data.
                        info!(view, %block_hash, "block not yet in reth, deferring finalization");
                        self.pending_finalization = Some(PendingFinalization {
                            view,
                            block_hash,
                            commit_qc,
                        });
                        self.pending_executions.insert(block_hash);
                    }
                } else {
                    // No beacon engine (test mode)
                    self.pending_block_data.clear();
                    self.pending_executions.clear();
                }

                // Persist consensus state after each commit (S3 fix).
                // Use engine's actual locked_qc/last_committed_qc instead of the
                // commit_qc, ensuring locked_qc >= commit_qc survives crash recovery.
                if let Some(ref path) = self.state_file {
                    let scheduled_epoch = self.engine.epoch_manager()
                        .staged_epoch_info()
                        .map(|(epoch, validators, f)| (epoch, validators.to_vec(), f));
                    let snapshot = ConsensusSnapshot {
                        version: 1,
                        current_view: self.engine.current_view(),
                        locked_qc: self.engine.locked_qc().clone(),
                        last_committed_qc: self.engine.last_committed_qc().clone(),
                        consecutive_timeouts: 0, // reset after commit
                        scheduled_epoch_transition: scheduled_epoch,
                    };
                    if let Err(e) = persistence::save_consensus_state(path, &snapshot) {
                        error!(error = %e, "failed to save consensus state");
                    } else {
                        debug!(view = snapshot.current_view, "consensus state persisted");
                    }
                }

                // Always check: if this node is leader for the next view, start building.
                if self.engine.is_current_leader() {
                    info!(next_view = self.engine.current_view(), "leader for next view");
                    self.schedule_payload_build().await;
                }
            }
            EngineOutput::ViewChanged { new_view } => {
                counter!("n42_view_changes_total").increment(1);
                info!(new_view, "view changed");

                // Preserve pending data if a committed block is awaiting import.
                // In f=0 configs, ViewChanged fires immediately after BlockCommitted
                // (same process_event call), and clearing these would lose the
                // deferred finalization state that depends on pending_block_data.
                if self.pending_finalization.is_none() {
                    self.pending_block_data.clear();
                    self.pending_executions.clear();
                }

                // Reset empty block skip counter after a timeout-driven view change.
                // Without this, each leader independently skips empty blocks, causing
                // a cascading dead zone where N validators × MAX_SKIPS views pass
                // without any block production. Timeouts signal that consensus is
                // struggling — the next leader must build to restore liveness.
                self.consecutive_empty_skips = 0;

                // After a view change (timeout recovery), if this node becomes
                // the new leader, it should trigger payload building.
                if self.engine.is_current_leader() {
                    info!(new_view, "became leader after view change, triggering payload build");
                    self.schedule_payload_build().await;
                }
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
                // Record evidence for accountability and future slashing.
                if let Some(ref state) = self.consensus_state {
                    state.record_equivocation(view, validator, hash1, hash2);
                }
            }
            EngineOutput::EpochTransition { new_epoch, validator_count } => {
                info!(
                    new_epoch,
                    validator_count,
                    "epoch transition: validator set updated"
                );
                // Update the validator set used for sync QC verification so that
                // new-epoch blocks can be validated by joining nodes.
                let updated_vs = self.engine.epoch_manager().current_validator_set().clone();
                self.validator_set_for_sync = Some(updated_vs);
            }
        }
    }

    /// Handles incoming block data from the leader via /n42/blocks/1 topic.
    ///
    /// Two arrival orders are supported:
    ///   1. ExecuteBlock(hash) first → data cached in pending_executions → import on arrival
    ///   2. BlockData first → cache in pending_block_data → import when ExecuteBlock arrives
    async fn handle_block_data(&mut self, data: Vec<u8>) {
        debug!(bytes = data.len(), "handle_block_data called");
        let broadcast: BlockDataBroadcast = match bincode::deserialize(&data) {
            Ok(b) => b,
            Err(e) => {
                warn!("invalid block data broadcast: {e}");
                return;
            }
        };

        let hash = broadcast.block_hash;

        if self.pending_executions.remove(&hash) {
            debug!(%hash, "block data matched pending execution, importing now");
            self.import_and_notify(broadcast).await;
        } else {
            debug!(%hash, pending_execs = ?self.pending_executions, "block data cached (no pending execution)");
            // Cache for when engine requests it via ExecuteBlock
            // Bound cache to 16 entries to prevent memory issues
            if self.pending_block_data.len() >= 16 {
                if let Some(old_key) = self.pending_block_data.keys().next().copied() {
                    self.pending_block_data.remove(&old_key);
                }
            }
            self.pending_block_data.insert(hash, data);
        }
    }

    /// Imports block data into reth via new_payload, then:
    /// 1. Calls fork_choice_updated to advance the chain head (critical for followers!)
    /// 2. Notifies the consensus engine via BlockImported event
    ///
    /// Reference: In Aptos, insert_block() starts the execution pipeline
    /// then immediately proceeds to vote construction. We wait for new_payload
    /// to ensure block validity before voting (more conservative, acceptable
    /// with 8-second slots).
    async fn import_and_notify(&mut self, broadcast: BlockDataBroadcast) {
        let Some(ref engine_handle) = self.beacon_engine else { return; };

        // Deserialize the execution payload from JSON
        let execution_data = match serde_json::from_slice(&broadcast.payload_json) {
            Ok(data) => data,
            Err(e) => {
                warn!(hash = %broadcast.block_hash, "failed to deserialize execution payload: {e}");
                return;
            }
        };

        // Import block via Engine API new_payload
        match engine_handle.new_payload(execution_data).await {
            Ok(status) => {
                let is_valid = matches!(
                    status.status,
                    PayloadStatusEnum::Valid | PayloadStatusEnum::Accepted
                );

                if is_valid {
                    // Verify block hash: cross-check engine's validated hash against
                    // the broadcast's claimed hash to detect substitution attacks.
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

                    // Advance reth chain head (CRITICAL: without this, reth stays at genesis).
                    // HotStuff-2 provides instant finality, so committed blocks are final.
                    // Reference: Ethereum beacon chain also calls fork_choice_updated
                    // after processing each block from gossip.
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

                    // Update local head
                    self.head_block_hash = broadcast.block_hash;

                    // Complete deferred finalization if this block was committed but not yet imported.
                    if let Some(ref pf) = self.pending_finalization {
                        if pf.block_hash == broadcast.block_hash {
                            let deferred_view = pf.view;
                            info!(
                                view = deferred_view,
                                %broadcast.block_hash,
                                "completing deferred finalization"
                            );
                            // Safe: outer `if let Some(ref pf)` guarantees `Some`.
                            let _pf = self.pending_finalization.take();
                            self.pending_block_data.clear();
                            self.pending_executions.clear();

                            // Notify mobile packet generation (deferred block now in reth)
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
                    }

                    // Notify consensus engine → triggers deferred vote
                    if let Err(e) = self.engine.process_event(
                        ConsensusEvent::BlockImported(broadcast.block_hash)
                    ) {
                        error!(error = %e, "error processing BlockImported");
                    }
                } else {
                    warn!(
                        hash = %broadcast.block_hash,
                        status = ?status.status,
                        "new_payload rejected block (invalid data from leader)"
                    );
                    // Don't notify engine → pacemaker timeout will trigger view change.
                    // Reference: Tendermint prevotes nil when block is invalid.
                }
            }
            Err(e) => {
                error!(
                    hash = %broadcast.block_hash,
                    error = %e,
                    "new_payload failed for imported block"
                );
            }
        }
    }

    // ── State Sync ──

    /// Stores a committed block in the ring buffer for serving sync requests.
    fn store_committed_block(&mut self, view: u64, block_hash: B256, commit_qc: QuorumCertificate) {
        // Try to get the payload from pending_block_data (follower path)
        // or it will be empty for now (leader populates it during broadcast).
        let payload = self.pending_block_data
            .get(&block_hash)
            .and_then(|data| bincode::deserialize::<BlockDataBroadcast>(data).ok())
            .map(|b| b.payload_json)
            .unwrap_or_default();

        if self.committed_blocks.len() >= max_committed_blocks() {
            self.committed_blocks.pop_front();
        }
        self.committed_blocks.push_back(CommittedBlock {
            view,
            block_hash,
            commit_qc,
            payload,
        });
    }

    /// Initiates a state sync request to a random connected peer.
    fn initiate_sync(&mut self, local_view: u64, target_view: u64) {
        if self.sync_in_flight {
            // Check for stale sync request (peer may have gone silent).
            if let Some(started) = self.sync_started_at {
                if started.elapsed() > sync_request_timeout() {
                    warn!(
                        elapsed_secs = started.elapsed().as_secs(),
                        "sync request timed out, resetting"
                    );
                    self.sync_in_flight = false;
                    self.sync_started_at = None;
                } else {
                    debug!(local_view, target_view, "sync already in flight, skipping");
                    return;
                }
            } else {
                debug!(local_view, target_view, "sync already in flight, skipping");
                return;
            }
        }

        // R2 fix: deterministic peer rotation based on view number.
        // Different views naturally select different peers, avoiding always
        // hitting the same peer (which causes persistent failure if it's down).
        let peers: Vec<_> = self.connected_peers.iter().copied().collect();
        if peers.is_empty() {
            warn!("no connected peers for sync");
            return;
        }
        let idx = (local_view as usize) % peers.len();
        let peer = peers[idx];

        info!(
            %peer,
            local_view,
            target_view,
            "initiating state sync"
        );

        let request = n42_network::BlockSyncRequest {
            from_view: local_view + 1,
            to_view: target_view,
            local_committed_view: local_view,
        };

        if let Err(e) = self.network.request_sync(peer, request) {
            error!(error = %e, "failed to send sync request");
            return;
        }

        self.sync_in_flight = true;
        self.sync_started_at = Some(Instant::now());
    }

    /// Handles an incoming sync request from a peer.
    /// Looks up committed blocks in the ring buffer and sends them back.
    fn handle_sync_request(
        &self,
        peer: PeerId,
        request_id: u64,
        request: n42_network::BlockSyncRequest,
    ) {
        counter!("n42_sync_requests_served_total").increment(1);
        info!(
            %peer,
            from_view = request.from_view,
            to_view = request.to_view,
            "handling sync request"
        );

        // Limit sync response to prevent OOM from unbounded range requests.
        const MAX_SYNC_BLOCKS: usize = 128;

        let blocks: Vec<SyncBlock> = self.committed_blocks
            .iter()
            .filter(|b| b.view >= request.from_view && b.view <= request.to_view)
            .take(MAX_SYNC_BLOCKS)
            .map(|b| SyncBlock {
                view: b.view,
                block_hash: b.block_hash,
                commit_qc: b.commit_qc.clone(),
                payload: b.payload.clone(),
            })
            .collect();

        let peer_committed_view = self.committed_blocks
            .back()
            .map(|b| b.view)
            .unwrap_or(0);

        info!(
            %peer,
            blocks_sent = blocks.len(),
            peer_committed_view,
            "sending sync response"
        );

        let response = BlockSyncResponse {
            blocks,
            peer_committed_view,
        };

        if let Err(e) = self.network.send_sync_response(request_id, response) {
            error!(error = %e, "failed to send sync response");
        }
    }

    /// Handles a sync response containing blocks from a peer.
    /// Imports each block into reth and advances local state.
    async fn handle_sync_response(
        &mut self,
        peer: PeerId,
        response: BlockSyncResponse,
    ) {
        self.sync_in_flight = false;
        self.sync_started_at = None;

        info!(
            %peer,
            blocks = response.blocks.len(),
            peer_committed_view = response.peer_committed_view,
            "received sync response"
        );

        if response.blocks.is_empty() {
            debug!("sync response contains no blocks");
            return;
        }

        let mut imported = 0u64;
        for sync_block in &response.blocks {
            // Skip blocks with empty payload (peer didn't have the data)
            if sync_block.payload.is_empty() {
                debug!(view = sync_block.view, "skipping sync block with empty payload");
                continue;
            }

            // Verify commit_qc before importing (defect 3 fix).
            // Without this, a malicious peer could inject forged blocks during sync.
            // When no validator set is configured, reject sync blocks entirely
            // rather than silently skipping verification.
            let vs = match &self.validator_set_for_sync {
                Some(vs) => vs,
                None => {
                    warn!("cannot verify sync blocks: no validator set configured, rejecting sync response");
                    return;
                }
            };
            {
                // Check CommitQC signature validity (S2 fix: use commit message format)
                if let Err(e) = verify_commit_qc(&sync_block.commit_qc, vs) {
                    warn!(
                        view = sync_block.view,
                        hash = %sync_block.block_hash,
                        error = %e,
                        "sync block has invalid commit_qc, skipping"
                    );
                    continue;
                }
                // Check QC corresponds to this block
                if sync_block.commit_qc.block_hash != sync_block.block_hash {
                    warn!(
                        view = sync_block.view,
                        block_hash = %sync_block.block_hash,
                        qc_hash = %sync_block.commit_qc.block_hash,
                        "sync block commit_qc hash mismatch, skipping"
                    );
                    continue;
                }
                if sync_block.commit_qc.view != sync_block.view {
                    warn!(
                        block_view = sync_block.view,
                        qc_view = sync_block.commit_qc.view,
                        "sync block commit_qc view mismatch, skipping"
                    );
                    continue;
                }
            }

            // Construct a BlockDataBroadcast for import_and_notify
            let broadcast = BlockDataBroadcast {
                block_hash: sync_block.block_hash,
                view: sync_block.view,
                payload_json: sync_block.payload.clone(),
            };

            self.import_and_notify(broadcast).await;

            // Store in committed_blocks buffer
            if self.committed_blocks.len() >= max_committed_blocks() {
                self.committed_blocks.pop_front();
            }
            self.committed_blocks.push_back(CommittedBlock {
                view: sync_block.view,
                block_hash: sync_block.block_hash,
                commit_qc: sync_block.commit_qc.clone(),
                payload: sync_block.payload.clone(),
            });

            imported += 1;
        }

        info!(
            imported,
            peer_committed_view = response.peer_committed_view,
            "state sync blocks imported"
        );

        // If we're still behind the peer, request more blocks
        let local_view = self.engine.current_view();
        if response.peer_committed_view > local_view + 3 {
            info!(
                local_view,
                peer_committed_view = response.peer_committed_view,
                "still behind after sync, requesting more blocks"
            );
            self.initiate_sync(local_view, response.peer_committed_view);
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
    use std::collections::HashSet;
    use std::time::Duration;

    /// Helper: create a test ConsensusEngine with a single validator.
    fn make_test_engine() -> (
        ConsensusEngine,
        mpsc::Receiver<EngineOutput>,
    ) {
        let sk = BlsSecretKey::random().unwrap();
        let pk = sk.public_key();

        let validator_info = ValidatorInfo {
            address: Address::with_last_byte(1),
            bls_public_key: pk,
        };

        let vs = ValidatorSet::new(&[validator_info], 0);
        let (output_tx, output_rx) = mpsc::channel(1024);

        let engine = ConsensusEngine::new(
            0,
            sk,
            vs,
            60000,
            120000,
            output_tx,
        );

        (engine, output_rx)
    }

    /// Helper: create a test NetworkHandle.
    fn make_test_network() -> (
        NetworkHandle,
        mpsc::UnboundedReceiver<NetworkCommand>,
    ) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        (NetworkHandle::new(cmd_tx), cmd_rx)
    }

    #[test]
    fn test_orchestrator_construction() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::unbounded_channel();

        let orch = ConsensusOrchestrator::new(engine, network, net_event_rx, output_rx);
        let _ = orch;
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
        let (network, mut cmd_rx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::unbounded_channel();

        let mut orch = ConsensusOrchestrator::new(engine, network, net_event_rx, output_rx);

        let sk = BlsSecretKey::random().unwrap();
        let sig = sk.sign(b"test");
        let vote = Vote {
            view: 1,
            block_hash: B256::repeat_byte(0xAA),
            voter: 0,
            signature: sig,
        };

        orch.handle_engine_output(EngineOutput::BroadcastMessage(
            ConsensusMessage::Vote(vote),
        ))
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
        let (network, mut cmd_rx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::unbounded_channel();

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
        let (network, _cmd_rx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::unbounded_channel();

        let mut orch = ConsensusOrchestrator::new(engine, network, net_event_rx, output_rx);
        orch.handle_engine_output(EngineOutput::ExecuteBlock(B256::repeat_byte(0xCC))).await;
    }

    #[tokio::test]
    async fn test_handle_engine_output_block_committed() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::unbounded_channel();

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
        let (network, _cmd_rx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::unbounded_channel();

        let mut orch = ConsensusOrchestrator::new(engine, network, net_event_rx, output_rx);
        orch.handle_engine_output(EngineOutput::ViewChanged { new_view: 5 }).await;
    }

    #[tokio::test]
    async fn test_orchestrator_exits_on_net_event_channel_close() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx) = make_test_network();
        let (net_event_tx, net_event_rx) = mpsc::unbounded_channel::<NetworkEvent>();

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
        let (network, _cmd_rx) = make_test_network();
        let (net_event_tx, net_event_rx) = mpsc::unbounded_channel::<NetworkEvent>();

        let orch = ConsensusOrchestrator::new(engine, network, net_event_rx, output_rx);

        let peer_id = libp2p::PeerId::random();
        net_event_tx
            .send(NetworkEvent::PeerConnected(peer_id))
            .unwrap();
        drop(net_event_tx);

        let result = tokio::time::timeout(Duration::from_secs(5), orch.run()).await;
        assert!(result.is_ok(), "orchestrator should exit after processing events");
    }

    #[tokio::test]
    async fn test_block_committed_updates_shared_state() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::unbounded_channel();

        let vs = ValidatorSet::new(&[], 0);
        let state = Arc::new(SharedConsensusState::new(vs));

        let (block_ready_tx, block_ready_rx) = mpsc::unbounded_channel();
        let (leader_payload_tx, leader_payload_rx) = mpsc::unbounded_channel();
        let mut orch = ConsensusOrchestrator {
            engine,
            network,
            net_event_rx,
            output_rx,
            beacon_engine: None,
            payload_builder: None,
            consensus_state: Some(state.clone()),
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
        };

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
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::unbounded_channel::<NetworkEvent>();

        let (block_ready_tx, block_ready_rx) = mpsc::unbounded_channel();
        let (leader_payload_tx, leader_payload_rx) = mpsc::unbounded_channel();

        let mut orch = ConsensusOrchestrator {
            engine,
            network,
            net_event_rx,
            output_rx,
            beacon_engine: None,
            payload_builder: None,
            consensus_state: None,
            head_block_hash: B256::ZERO,
            block_ready_tx: block_ready_tx.clone(),
            block_ready_rx,
            fee_recipient: Address::ZERO,
            slot_time: Duration::ZERO,
            next_build_at: None,
            next_slot_timestamp: None,
            consecutive_empty_skips: 0,
            pending_block_data: BTreeMap::new(),
            pending_executions: HashSet::new(),
            pending_finalization: None,
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
        };

        // Send a BlockReady hash through the channel.
        let test_hash = B256::repeat_byte(0xFF);
        block_ready_tx.send(test_hash).unwrap();

        // The orchestrator should process it. We can't easily test the full
        // run loop here, but we can verify the channel works.
        let received = orch.block_ready_rx.try_recv();
        assert!(received.is_ok(), "should receive BlockReady hash");
        assert_eq!(received.unwrap(), test_hash);
    }

    #[test]
    fn test_sync_timeout_resets_in_flight() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::unbounded_channel();

        let mut orch = ConsensusOrchestrator::new(engine, network, net_event_rx, output_rx);

        // Add a peer so sync can proceed.
        orch.connected_peers.insert(libp2p::PeerId::random());

        // Simulate a sync that started 60 seconds ago (well past 30s timeout).
        orch.sync_in_flight = true;
        orch.sync_started_at = Some(Instant::now() - Duration::from_secs(60));

        // A new initiate_sync should detect the timeout, reset, and send a new request.
        orch.initiate_sync(1, 10);

        // After the timeout reset + new request, sync_in_flight should be true
        // (a new request was sent).
        assert!(orch.sync_in_flight, "new sync request should be in flight");
        // And sync_started_at should be recent (not 60s ago).
        let started = orch.sync_started_at.expect("sync_started_at should be set");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "sync_started_at should be recent after timeout reset"
        );
    }
}
