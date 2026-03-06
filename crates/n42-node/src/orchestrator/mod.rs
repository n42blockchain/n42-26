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
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
use metrics::{counter, gauge};
use tracing::{debug, error, info, warn};

/// Block data broadcast via /n42/blocks/1 GossipSub topic.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct BlockDataBroadcast {
    pub(crate) block_hash: B256,
    pub(crate) view: u64,
    /// JSON-serialized execution payload (bincode can't handle reth's `#[serde(untagged)]`).
    pub(crate) payload_json: Vec<u8>,
    /// Block timestamp (seconds since epoch). Stored directly to avoid JSON re-parsing.
    /// Defaults to 0 for backwards compatibility with older serialized broadcasts.
    #[serde(default)]
    pub(crate) timestamp: u64,
}

/// Blob sidecar broadcast via /n42/blobs/1 GossipSub topic.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct BlobSidecarBroadcast {
    pub(crate) block_hash: B256,
    pub(crate) view: u64,
    pub(crate) sidecars: Vec<(B256, Vec<u8>)>,
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
}

impl ConsensusOrchestrator {
    pub fn new(
        engine: ConsensusEngine,
        network: NetworkHandle,
        net_event_rx: mpsc::Receiver<NetworkEvent>,
        output_rx: mpsc::Receiver<EngineOutput>,
    ) -> Self {
        let (block_ready_tx, block_ready_rx) = mpsc::unbounded_channel();
        let (leader_payload_tx, leader_payload_rx) = mpsc::unbounded_channel();
        let (import_done_tx, import_done_rx) = mpsc::unbounded_channel();
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
            mobile_reward_manager: None,
            staking_manager: None,
            committed_block_count: 0,
            last_committed_timestamp: 0,
            view_started_at: None,
            epoch_schedule: None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_engine_api(
        engine: ConsensusEngine,
        network: NetworkHandle,
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

        let slot_time_ms: u64 = std::env::var("N42_BLOCK_INTERVAL_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let slot_time = Duration::from_millis(slot_time_ms);

        if slot_time_ms > 0 {
            info!(target: "n42::cl::orchestrator", slot_time_ms, "wall-clock-aligned slot timing configured");
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
            mobile_reward_manager: None,
            staking_manager: None,
            committed_block_count: 0,
            last_committed_timestamp: 0,
            view_started_at: None,
            epoch_schedule: None,
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

            tokio::select! {
                _ = &mut timeout => {
                    let view = self.engine.current_view();
                    counter!("n42_view_timeouts_total").increment(1);
                    warn!(target: "n42::cl::orchestrator", view, "pacemaker timeout, initiating view change");
                    if let Err(e) = self.engine.on_timeout() {
                        error!(target: "n42::cl::orchestrator", view, error = %e, "error handling timeout");
                    }
                }

                event = self.net_event_rx.recv() => {
                    match event {
                        Some(ev) => self.handle_network_event(ev).await,
                        None => {
                            info!(target: "n42::cl::orchestrator", "network event channel closed, shutting down orchestrator");
                            break;
                        }
                    }
                }

                output = self.output_rx.recv() => {
                    match output {
                        Some(engine_output) => self.handle_engine_output(engine_output).await,
                        None => {
                            info!(target: "n42::cl::orchestrator", "engine output channel closed, shutting down orchestrator");
                            break;
                        }
                    }
                }

                block_hash = self.block_ready_rx.recv() => {
                    if let Some(hash) = block_hash {
                        info!(target: "n42::cl::orchestrator", %hash, view = self.engine.current_view(), "payload built, feeding BlockReady to consensus");
                        if let Err(e) = self.engine.process_event(
                            n42_consensus::ConsensusEvent::BlockReady(hash)
                        ) {
                            error!(target: "n42::cl::orchestrator", error = %e, "error processing BlockReady event");
                        }
                    }
                }

                tx_data = async {
                    match self.tx_broadcast_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    if let Some(data) = tx_data
                        && let Err(e) = self.network.broadcast_transaction(data)
                    {
                        tracing::debug!(target: "n42::cl::orchestrator", error = %e, "failed to broadcast transaction");
                    }
                }

                payload_data = self.leader_payload_rx.recv() => {
                    if let Some((hash, data)) = payload_data {
                        self.handle_leader_payload_feedback(hash, data).await;
                    }
                }

                import_result = self.import_done_rx.recv() => {
                    if let Some((hash, view, success, block_ts)) = import_result {
                        self.handle_import_done(hash, view, success, block_ts).await;
                    }
                }

                _ = &mut build_timer => {
                    let slot_ts = self.next_slot_timestamp.take();
                    self.next_build_at = None;

                    if slot_ts.is_none() {
                        // Startup delay completed: reset pacemaker so the full base_timeout starts now.
                        let view = self.engine.current_view();
                        self.engine.pacemaker_mut().reset_for_view(view, 0);
                        info!(target: "n42::cl::orchestrator", "startup delay completed, triggering first payload build");
                    } else {
                        info!(target: "n42::cl::orchestrator", slot_timestamp = ?slot_ts, "slot boundary reached, triggering payload build");
                    }

                    self.do_trigger_payload_build(slot_ts).await;
                }
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

    async fn handle_network_event(&mut self, event: NetworkEvent) {
        match event {
            NetworkEvent::ConsensusMessage { source: _, message } => {
                counter!("n42_consensus_messages_received").increment(1);
                if let Err(e) = self.engine.process_event(
                    n42_consensus::ConsensusEvent::Message(*message)
                ) {
                    if matches!(e, n42_consensus::N42ConsensusError::SafetyViolation { .. }) {
                        debug!(target: "n42::cl::orchestrator", error = %e, "benign safety check (QC ordering race)");
                    } else {
                        warn!(target: "n42::cl::orchestrator", error = %e, "error processing consensus message");
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

    fn make_test_network() -> (NetworkHandle, mpsc::UnboundedReceiver<NetworkCommand>) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        (NetworkHandle::new(cmd_tx), cmd_rx)
    }

    #[test]
    fn test_orchestrator_construction() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx) = make_test_network();
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
        let (network, mut cmd_rx) = make_test_network();
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
        let (network, mut cmd_rx) = make_test_network();
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
        let (network, _cmd_rx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let mut orch = ConsensusOrchestrator::new(engine, network, net_event_rx, output_rx);
        orch.handle_engine_output(EngineOutput::ExecuteBlock(B256::repeat_byte(0xCC)))
            .await;
    }

    #[tokio::test]
    async fn test_handle_engine_output_block_committed() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx) = make_test_network();
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
        let (network, _cmd_rx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let mut orch = ConsensusOrchestrator::new(engine, network, net_event_rx, output_rx);
        orch.handle_engine_output(EngineOutput::ViewChanged { new_view: 5 })
            .await;
    }

    #[tokio::test]
    async fn test_orchestrator_exits_on_net_event_channel_close() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx) = make_test_network();
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
        let (network, _cmd_rx) = make_test_network();
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
        let (network, _cmd_rx) = make_test_network();
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
        let (network, _cmd_rx) = make_test_network();
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
