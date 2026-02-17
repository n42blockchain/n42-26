use crate::consensus_state::SharedConsensusState;
use alloy_primitives::{Address, B256};
use alloy_rpc_types_engine::{ForkchoiceState, PayloadAttributes, PayloadStatusEnum};
use n42_consensus::{ConsensusEngine, ConsensusEvent, EngineOutput};
use n42_network::{NetworkEvent, NetworkHandle};
use n42_primitives::QuorumCertificate;
use reth_ethereum_engine_primitives::EthEngineTypes;
use reth_node_builder::ConsensusEngineHandle;
use reth_payload_builder::PayloadBuilderHandle;
use reth_payload_primitives::{EngineApiMessageVersion, PayloadKind, PayloadTypes};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
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
    output_rx: mpsc::UnboundedReceiver<EngineOutput>,
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
    /// Cache of block data received from network but not yet requested by engine.
    /// Key: block_hash, Value: raw broadcast bytes.
    /// Bounded to 16 entries to prevent memory issues.
    pending_block_data: HashMap<B256, Vec<u8>>,
    /// Block hashes the engine has requested (via ExecuteBlock) but data not yet received.
    pending_executions: HashSet<B256>,
    /// Deferred block finalization when Decide arrives before block data.
    /// The committed block hasn't been imported into reth yet.
    pending_finalization: Option<PendingFinalization>,
}

impl ConsensusOrchestrator {
    /// Creates a new orchestrator (basic mode, without Engine API integration).
    /// Used for testing and non-block-producing scenarios.
    pub fn new(
        engine: ConsensusEngine,
        network: NetworkHandle,
        net_event_rx: mpsc::UnboundedReceiver<NetworkEvent>,
        output_rx: mpsc::UnboundedReceiver<EngineOutput>,
    ) -> Self {
        let (block_ready_tx, block_ready_rx) = mpsc::unbounded_channel();
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
            pending_block_data: HashMap::new(),
            pending_executions: HashSet::new(),
            pending_finalization: None,
        }
    }

    /// Creates an orchestrator with full Engine API integration.
    pub fn with_engine_api(
        engine: ConsensusEngine,
        network: NetworkHandle,
        net_event_rx: mpsc::UnboundedReceiver<NetworkEvent>,
        output_rx: mpsc::UnboundedReceiver<EngineOutput>,
        beacon_engine: ConsensusEngineHandle<EthEngineTypes>,
        payload_builder: PayloadBuilderHandle<EthEngineTypes>,
        consensus_state: Arc<SharedConsensusState>,
        head_block_hash: B256,
        fee_recipient: Address,
    ) -> Self {
        let (block_ready_tx, block_ready_rx) = mpsc::unbounded_channel();

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
            pending_block_data: HashMap::new(),
            pending_executions: HashSet::new(),
            pending_finalization: None,
        }
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
                    warn!(view, "pacemaker timeout, initiating view change");
                    if let Err(e) = self.engine.on_timeout() {
                        error!(view, error = %e, "error handling timeout");
                    }
                }

                // Branch 2: Network event -> feed to consensus engine
                event = self.net_event_rx.recv() => {
                    match event {
                        Some(NetworkEvent::ConsensusMessage { source: _, message }) => {
                            if let Err(e) = self.engine.process_event(
                                ConsensusEvent::Message(message)
                            ) {
                                warn!(error = %e, "error processing consensus message");
                            }
                        }
                        Some(NetworkEvent::PeerConnected(peer_id)) => {
                            info!(%peer_id, "consensus peer connected");
                        }
                        Some(NetworkEvent::PeerDisconnected(peer_id)) => {
                            warn!(%peer_id, "consensus peer disconnected");
                        }
                        Some(NetworkEvent::BlockAnnouncement { source, data }) => {
                            debug!(%source, bytes = data.len(), "received block data broadcast");
                            self.handle_block_data(data).await;
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

                // Branch 5: Slot boundary / startup delay reached, trigger payload build
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
    }

    /// Schedules a payload build using wall-clock-aligned slot timing.
    ///
    /// If slot_time > 0, computes the next wall-clock boundary (a time where
    /// `unix_ms % slot_time_ms == 0`) and schedules the build for that instant.
    /// This ensures all nodes with synchronized clocks produce blocks at the
    /// same fixed intervals, regardless of build overhead or consensus latency.
    ///
    /// If slot_time == 0, triggers immediately (legacy behavior).
    async fn schedule_payload_build(&mut self) {
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
                    let current_view = self.engine.current_view();
                    tokio::spawn(async move {
                        // Give the builder time to pack transactions from the pool.
                        tokio::time::sleep(Duration::from_millis(500)).await;

                        match payload_builder
                            .resolve_kind(payload_id, PayloadKind::WaitForPending)
                            .await
                        {
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
                                let payload_json = serde_json::to_vec(&execution_data)
                                    .unwrap_or_default();

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
                                                            if let Err(e) = network.announce_block(encoded) {
                                                                warn!(error = %e, "failed to broadcast block data");
                                                            }
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
            EngineOutput::SendToValidator(_target, msg) => {
                // GossipSub doesn't support point-to-point; broadcast instead.
                if let Err(e) = self.network.broadcast_consensus(msg) {
                    error!(error = %e, "failed to send message to validator (broadcast fallback)");
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
                info!(view, %block_hash, "block committed by consensus");

                // Update shared consensus state (always safe, QC is valid regardless of reth state)
                if let Some(ref state) = self.consensus_state {
                    state.update_committed_qc(commit_qc.clone());
                    state.notify_block_committed(block_hash, view);
                }

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
                    } else if let Some(data) = self.pending_block_data.remove(&block_hash) {
                        // Case B: BlockData arrived first, cached but not imported
                        info!(view, %block_hash, "block data cached, importing for deferred finalization");
                        match bincode::deserialize::<BlockDataBroadcast>(&data) {
                            Ok(broadcast) => {
                                self.import_and_notify(broadcast).await;
                                self.pending_block_data.clear();
                                self.pending_executions.clear();
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

                // Always check: if this node is leader for the next view, start building.
                if self.engine.is_current_leader() {
                    info!(next_view = self.engine.current_view(), "leader for next view");
                    self.schedule_payload_build().await;
                }
            }
            EngineOutput::ViewChanged { new_view } => {
                info!(new_view, "view changed");

                // Clean up stale block data from previous views.
                self.pending_block_data.clear();

                // Preserve pending_executions and pending_finalization if a committed
                // block is awaiting import. In f=0 configs, ViewChanged fires immediately
                // after BlockCommitted (same process_event call), and clearing these would
                // lose the deferred finalization state.
                if self.pending_finalization.is_none() {
                    self.pending_executions.clear();
                }

                // After a view change (timeout recovery), if this node becomes
                // the new leader, it should trigger payload building.
                if self.engine.is_current_leader() {
                    info!(new_view, "became leader after view change, triggering payload build");
                    self.schedule_payload_build().await;
                }
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
                            info!(
                                view = pf.view,
                                %broadcast.block_hash,
                                "completing deferred finalization"
                            );
                            let _pf = self.pending_finalization.take().unwrap();
                            self.pending_block_data.clear();
                            self.pending_executions.clear();

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
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, B256};
    use n42_chainspec::ValidatorInfo;
    use n42_consensus::{ConsensusEngine, ValidatorSet};
    use n42_network::{NetworkCommand, NetworkHandle};
    use n42_primitives::{BlsSecretKey, ConsensusMessage, QuorumCertificate, Vote};
    use std::collections::{HashMap, HashSet};
    use std::time::Duration;

    /// Helper: create a test ConsensusEngine with a single validator.
    fn make_test_engine() -> (
        ConsensusEngine,
        mpsc::UnboundedReceiver<EngineOutput>,
    ) {
        let sk = BlsSecretKey::random().unwrap();
        let pk = sk.public_key();

        let validator_info = ValidatorInfo {
            address: Address::with_last_byte(1),
            bls_public_key: pk,
        };

        let vs = ValidatorSet::new(&[validator_info], 0);
        let (output_tx, output_rx) = mpsc::unbounded_channel();

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
            pending_block_data: HashMap::new(),
            pending_executions: HashSet::new(),
            pending_finalization: None,
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
            pending_block_data: HashMap::new(),
            pending_executions: HashSet::new(),
            pending_finalization: None,
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
}
