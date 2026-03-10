use futures::StreamExt;
use libp2p::gossipsub;
use libp2p::swarm::SwarmEvent;
use libp2p::{Multiaddr, PeerId, Swarm};
use n42_primitives::ConsensusMessage;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::interval;

use crate::block_direct::{BlockDirectRequest, BlockDirectResponse};
use crate::consensus_direct::{ConsensusDirectRequest, ConsensusDirectResponse};
use crate::error::NetworkError;
use crate::tx_forward::{TxForwardRequest, TxForwardResponse};
use crate::gossipsub::handlers::{decode_consensus_message, encode_consensus_message, validate_message};
use crate::gossipsub::topics::{
    blob_sidecar_topic, block_announce_topic, consensus_topic, mempool_topic,
    verification_receipts_topic,
};
use crate::reconnection::ReconnectionManager;
use crate::state_sync::{BlockSyncRequest, BlockSyncResponse};
use crate::transport::{N42Behaviour, N42BehaviourEvent};

/// Commands sent to the network service from the node layer.
#[derive(Debug)]
pub enum NetworkCommand {
    BroadcastConsensus(ConsensusMessage),
    SendDirect { peer: PeerId, message: ConsensusMessage },
    AnnounceBlock(Vec<u8>),
    BroadcastTransaction(Vec<u8>),
    BroadcastBlobSidecar(Vec<u8>),
    /// Send block data directly to a specific peer (leader → validator).
    SendBlockDirect { peer: PeerId, data: Vec<u8> },
    /// Forward a batch of RLP-encoded transactions to the current leader.
    ForwardTxBatch { peer: PeerId, txs: Vec<Vec<u8>> },
    Dial(Multiaddr),
    RequestSync { peer: PeerId, request: BlockSyncRequest },
    SendSyncResponse { request_id: u64, response: BlockSyncResponse },
    RegisterPeer { peer_id: PeerId, addrs: Vec<Multiaddr>, trusted: bool },
    AddKademliaPeer { peer_id: PeerId, addrs: Vec<Multiaddr> },
}

/// Events produced by the network service for the node layer.
#[derive(Debug)]
pub enum NetworkEvent {
    ConsensusMessage { source: PeerId, message: Box<ConsensusMessage> },
    BlockAnnouncement { source: PeerId, data: Vec<u8> },
    VerificationReceipt { source: PeerId, data: Vec<u8> },
    TransactionReceived { source: PeerId, data: Vec<u8> },
    BlobSidecarReceived { source: PeerId, data: Vec<u8> },
    PeerConnected(PeerId),
    PeerDisconnected(PeerId),
    /// Batch of transactions forwarded from a non-leader validator.
    TxForwardReceived { source: PeerId, txs: Vec<Vec<u8>> },
    SyncRequest { peer: PeerId, request_id: u64, request: BlockSyncRequest },
    SyncResponse { peer: PeerId, response: BlockSyncResponse },
    SyncRequestFailed { peer: PeerId, error: String },
}

/// Handle for sending commands to the running `NetworkService`.
///
/// Cheaply cloneable. The main interface for the node layer to interact with
/// the network.
#[derive(Clone, Debug)]
pub struct NetworkHandle {
    command_tx: mpsc::UnboundedSender<NetworkCommand>,
    /// High-priority channel for block data and consensus commands.
    /// These bypass the potentially deep tx-broadcast backlog in `command_tx`.
    priority_tx: mpsc::UnboundedSender<NetworkCommand>,
    /// Validator index → PeerId mapping, populated from Identify events.
    validator_peer_map: Arc<RwLock<HashMap<u32, PeerId>>>,
}

impl NetworkHandle {
    pub fn new(
        command_tx: mpsc::UnboundedSender<NetworkCommand>,
        priority_tx: mpsc::UnboundedSender<NetworkCommand>,
    ) -> Self {
        Self {
            command_tx,
            priority_tx,
            validator_peer_map: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Returns the PeerId for a given validator index, if known.
    pub fn validator_peer(&self, index: u32) -> Option<PeerId> {
        match self.validator_peer_map.read() {
            Ok(map) => map.get(&index).copied(),
            Err(e) => {
                tracing::error!("validator_peer_map lock poisoned: {}", e);
                None
            }
        }
    }

    pub fn send_direct(&self, peer: PeerId, msg: ConsensusMessage) -> Result<(), NetworkError> {
        self.send_priority(NetworkCommand::SendDirect { peer, message: msg })
    }

    pub fn broadcast_consensus(&self, msg: ConsensusMessage) -> Result<(), NetworkError> {
        self.send_priority(NetworkCommand::BroadcastConsensus(msg))
    }

    pub fn announce_block(&self, data: Vec<u8>) -> Result<(), NetworkError> {
        self.send_priority(NetworkCommand::AnnounceBlock(data))
    }

    pub fn broadcast_transaction(&self, data: Vec<u8>) -> Result<(), NetworkError> {
        self.send(NetworkCommand::BroadcastTransaction(data))
    }

    pub fn broadcast_blob_sidecar(&self, data: Vec<u8>) -> Result<(), NetworkError> {
        self.send(NetworkCommand::BroadcastBlobSidecar(data))
    }

    /// Sends block data directly to a specific validator peer.
    pub fn send_block_direct(&self, peer: PeerId, data: Vec<u8>) -> Result<(), NetworkError> {
        self.send_priority(NetworkCommand::SendBlockDirect { peer, data })
    }

    /// Forwards a batch of RLP-encoded transactions to the current leader.
    pub fn forward_tx_batch(&self, peer: PeerId, txs: Vec<Vec<u8>>) -> Result<(), NetworkError> {
        self.send(NetworkCommand::ForwardTxBatch { peer, txs })
    }

    /// Returns all known validator peers (index → PeerId).
    pub fn all_validator_peers(&self) -> Vec<(u32, PeerId)> {
        match self.validator_peer_map.read() {
            Ok(map) => map.iter().map(|(&idx, &pid)| (idx, pid)).collect(),
            Err(_) => Vec::new(),
        }
    }

    pub fn dial(&self, addr: Multiaddr) -> Result<(), NetworkError> {
        self.send(NetworkCommand::Dial(addr))
    }

    pub fn request_sync(&self, peer: PeerId, request: BlockSyncRequest) -> Result<(), NetworkError> {
        self.send(NetworkCommand::RequestSync { peer, request })
    }

    pub fn send_sync_response(
        &self,
        request_id: u64,
        response: BlockSyncResponse,
    ) -> Result<(), NetworkError> {
        self.send(NetworkCommand::SendSyncResponse { request_id, response })
    }

    /// Registers a peer for automatic reconnection.
    /// Trusted peers are retried indefinitely; discovered peers up to a limited count.
    pub fn register_peer(
        &self,
        peer_id: PeerId,
        addrs: Vec<Multiaddr>,
        trusted: bool,
    ) -> Result<(), NetworkError> {
        const MAX_ADDRS_PER_PEER: usize = 16;
        if addrs.len() > MAX_ADDRS_PER_PEER {
            return Err(NetworkError::Dial(format!(
                "too many addresses for peer ({}), max is {}", addrs.len(), MAX_ADDRS_PER_PEER
            )));
        }
        self.send(NetworkCommand::RegisterPeer { peer_id, addrs, trusted })
    }

    pub fn add_kademlia_peer(
        &self,
        peer_id: PeerId,
        addrs: Vec<Multiaddr>,
    ) -> Result<(), NetworkError> {
        self.send(NetworkCommand::AddKademliaPeer { peer_id, addrs })
    }

    fn send(&self, cmd: NetworkCommand) -> Result<(), NetworkError> {
        self.command_tx.send(cmd).map_err(|_| NetworkError::ChannelClosed)
    }

    /// Send via the high-priority channel (block data, consensus, sync).
    fn send_priority(&self, cmd: NetworkCommand) -> Result<(), NetworkError> {
        self.priority_tx.send(cmd).map_err(|_| NetworkError::ChannelClosed)
    }
}

/// The network service drives the libp2p swarm and bridges messages between
/// the P2P network and the node layer.
pub struct NetworkService {
    swarm: Swarm<N42Behaviour>,
    command_rx: mpsc::UnboundedReceiver<NetworkCommand>,
    /// High-priority commands (block data, consensus) — drained before `command_rx`.
    priority_rx: mpsc::UnboundedReceiver<NetworkCommand>,
    /// High-priority channel for consensus messages (Vote, Proposal, PrepareQC, etc.).
    consensus_event_tx: mpsc::Sender<NetworkEvent>,
    /// Lower-priority channel for data events (BlockData, TX, Sync, Peers).
    event_tx: mpsc::Sender<NetworkEvent>,
    consensus_topic_hash: gossipsub::TopicHash,
    block_announce_topic_hash: gossipsub::TopicHash,
    mempool_topic_hash: gossipsub::TopicHash,
    blob_sidecar_topic_hash: gossipsub::TopicHash,
    verification_receipts_topic_hash: gossipsub::TopicHash,
    /// Maps internal request ID → libp2p ResponseChannel for sending sync replies.
    pending_sync_channels:
        HashMap<u64, libp2p::request_response::ResponseChannel<BlockSyncResponse>>,
    next_sync_id: u64,
    validator_peer_map: Arc<RwLock<HashMap<u32, PeerId>>>,
    /// Reverse map: PeerId → validator_index, for authenticating block direct pushes.
    peer_validator_map: HashMap<PeerId, u32>,
    reconnection: ReconnectionManager,
    /// When true, skip GossipSub tx publishing/forwarding to avoid event queue flooding.
    tx_gossip_disabled: bool,
}

impl NetworkService {
    /// Creates a new network service. Returns the service, a handle, a consensus event
    /// receiver (high-priority), and a data event receiver (lower-priority).
    ///
    /// Splitting the channels ensures that consensus messages (Vote, Proposal, CommitVote,
    /// PrepareQC, Decide, Timeout, NewView) are never queued behind high-volume data
    /// events (TxForward, BlockAnnouncement, TransactionReceived).
    pub fn new(
        mut swarm: Swarm<N42Behaviour>,
    ) -> Result<(Self, NetworkHandle, mpsc::Receiver<NetworkEvent>, mpsc::Receiver<NetworkEvent>), NetworkError> {
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (priority_tx, priority_rx) = mpsc::unbounded_channel();
        let (consensus_event_tx, consensus_event_rx) = mpsc::channel(2048);
        let (event_tx, event_rx) = mpsc::channel(8192);

        // TX gossip is disabled by default (TX Forward to Leader replaces it).
        // Set N42_ENABLE_TX_GOSSIP=1 to re-enable legacy GossipSub tx flooding.
        let tx_gossip_disabled = std::env::var("N42_ENABLE_TX_GOSSIP").is_err();
        if tx_gossip_disabled {
            tracing::info!("TX gossip disabled (default): using TX Forward to Leader. Set N42_ENABLE_TX_GOSSIP=1 to re-enable.");
        }

        let topics = [
            consensus_topic(),
            block_announce_topic(),
            verification_receipts_topic(),
            mempool_topic(),
            blob_sidecar_topic(),
        ];
        for (i, topic) in topics.iter().enumerate() {
            // Skip mempool topic (index 3) when tx gossip is disabled
            if tx_gossip_disabled && i == 3 {
                continue;
            }
            swarm
                .behaviour_mut()
                .gossipsub
                .subscribe(topic)
                .map_err(|e| NetworkError::Subscribe(e.to_string()))?;
        }

        let [consensus, block_announce, verification, mempool, blob_sidecar] = topics;
        let consensus_topic_hash = consensus.hash();
        let block_announce_topic_hash = block_announce.hash();
        let verification_receipts_topic_hash = verification.hash();
        let mempool_topic_hash = mempool.hash();
        let blob_sidecar_topic_hash = blob_sidecar.hash();

        let validator_peer_map = Arc::new(RwLock::new(HashMap::new()));
        let handle = NetworkHandle {
            command_tx,
            priority_tx,
            validator_peer_map: validator_peer_map.clone(),
        };

        let service = Self {
            swarm,
            command_rx,
            priority_rx,
            consensus_event_tx,
            event_tx,
            consensus_topic_hash,
            block_announce_topic_hash,
            mempool_topic_hash,
            blob_sidecar_topic_hash,
            verification_receipts_topic_hash,
            pending_sync_channels: HashMap::new(),
            next_sync_id: 0,
            validator_peer_map,
            peer_validator_map: HashMap::new(),
            reconnection: ReconnectionManager::new(),
            tx_gossip_disabled,
        };

        Ok((service, handle, consensus_event_rx, event_rx))
    }

    pub fn listen_on(&mut self, addr: Multiaddr) -> Result<(), NetworkError> {
        self.swarm
            .listen_on(addr)
            .map(|_| ())
            .map_err(|e| NetworkError::Listen(e.to_string()))
    }

    pub fn dial(&mut self, addr: Multiaddr) -> Result<(), NetworkError> {
        self.swarm.dial(addr).map_err(|e| NetworkError::Dial(e.to_string()))
    }

    /// Runs the event loop. Should be spawned as a background task.
    pub async fn run(mut self) {
        tracing::info!("network service started");

        let mut reconnect_interval = interval(Duration::from_secs(5));
        reconnect_interval.tick().await;

        let mut dht_refresh_interval = interval(Duration::from_secs(300));
        dht_refresh_interval.tick().await;

        if let Some(kad) = self.swarm.behaviour_mut().kademlia.as_mut()
            && let Err(e) = kad.bootstrap()
        {
            tracing::debug!(error = ?e, "kademlia bootstrap skipped (no known peers yet)");
        }

        loop {
            // Drain all pending priority commands first (block data, consensus).
            // This ensures block transmission is never delayed by tx broadcast backlog.
            while let Ok(cmd) = self.priority_rx.try_recv() {
                self.handle_command(cmd);
            }

            tokio::select! {
                // biased: check priority channel first each iteration.
                biased;

                cmd = self.priority_rx.recv() => {
                    match cmd {
                        Some(command) => self.handle_command(command),
                        None => {
                            // Priority channel closed but normal channel may still work.
                        }
                    }
                }
                event = self.swarm.select_next_some() => {
                    self.handle_swarm_event(event);
                }
                cmd = self.command_rx.recv() => {
                    match cmd {
                        Some(command) => self.handle_command(command),
                        None => {
                            tracing::info!("command channel closed, shutting down");
                            break;
                        }
                    }
                }
                _ = reconnect_interval.tick() => {
                    let to_reconnect = self.reconnection.peers_to_reconnect();
                    for (peer_id, addr) in to_reconnect {
                        tracing::debug!(%peer_id, %addr, "attempting reconnection");
                        metrics::counter!("n42_reconnection_attempts").increment(1);
                        if let Err(e) = self.swarm.dial(addr.clone()) {
                            tracing::debug!(%peer_id, %addr, error = %e, "reconnect dial failed");
                            self.reconnection.on_dial_failure(&peer_id);
                        }
                    }
                }
                _ = dht_refresh_interval.tick() => {
                    if let Some(kad) = self.swarm.behaviour_mut().kademlia.as_mut() {
                        kad.get_closest_peers(PeerId::random());
                        tracing::debug!("kademlia DHT refresh");
                    }
                }
            }
        }
    }

    fn handle_swarm_event(&mut self, event: SwarmEvent<N42BehaviourEvent>) {
        match event {
            SwarmEvent::Behaviour(N42BehaviourEvent::Gossipsub(gossipsub::Event::Message {
                propagation_source,
                message,
                message_id: _,
            })) => {
                self.handle_gossipsub_message(propagation_source, message);
            }
            SwarmEvent::Behaviour(N42BehaviourEvent::Gossipsub(
                gossipsub::Event::Subscribed { peer_id, topic },
            )) => {
                tracing::debug!(%peer_id, %topic, "peer subscribed to topic");
            }
            SwarmEvent::Behaviour(N42BehaviourEvent::StateSync(event)) => {
                self.handle_state_sync_event(event);
            }
            SwarmEvent::Behaviour(N42BehaviourEvent::ConsensusDirect(event)) => {
                self.handle_consensus_direct_event(event);
            }
            SwarmEvent::Behaviour(N42BehaviourEvent::BlockDirect(event)) => {
                self.handle_block_direct_event(event);
            }
            SwarmEvent::Behaviour(N42BehaviourEvent::TxForward(event)) => {
                self.handle_tx_forward_event(event);
            }
            SwarmEvent::Behaviour(N42BehaviourEvent::Identify(
                libp2p::identify::Event::Received { peer_id, info, .. },
            )) => {
                self.handle_identify_event(peer_id, info);
            }
            SwarmEvent::Behaviour(N42BehaviourEvent::Kademlia(event)) => {
                self.handle_kademlia_event(event);
            }
            SwarmEvent::Behaviour(N42BehaviourEvent::ConnectionLimits(event)) => match event {},
            SwarmEvent::Behaviour(N42BehaviourEvent::Mdns(event)) => {
                self.handle_mdns_event(event);
            }
            SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                tracing::info!(%peer_id, "peer connected");
                metrics::gauge!("n42_active_peer_connections").increment(1.0);
                self.reconnection.on_connected(&peer_id);
                self.emit_event(NetworkEvent::PeerConnected(peer_id));
            }
            SwarmEvent::ConnectionClosed { peer_id, .. } => {
                tracing::info!(%peer_id, "peer disconnected");
                metrics::gauge!("n42_active_peer_connections").decrement(1.0);
                self.reconnection.on_disconnected(&peer_id);
                // Clean up validator mappings for disconnected peer
                if let Some(idx) = self.peer_validator_map.remove(&peer_id) {
                    tracing::info!(%peer_id, validator_index = idx, "removed validator mapping for disconnected peer");
                    if let Ok(mut map) = self.validator_peer_map.write() {
                        if map.get(&idx) == Some(&peer_id) {
                            map.remove(&idx);
                        }
                    }
                }
                self.emit_event(NetworkEvent::PeerDisconnected(peer_id));
            }
            SwarmEvent::OutgoingConnectionError { peer_id: Some(peer_id), .. } => {
                tracing::debug!(%peer_id, "outgoing connection error");
                self.reconnection.on_dial_failure(&peer_id);
            }
            SwarmEvent::NewListenAddr { address, .. } => {
                let local_peer_id = *self.swarm.local_peer_id();
                tracing::info!(%local_peer_id, %address, "listening on address");
            }
            _ => {}
        }
    }

    fn handle_identify_event(&mut self, peer_id: PeerId, info: libp2p::identify::Info) {
        tracing::debug!(%peer_id, protocol = ?info.protocol_version, "identified peer");

        // agent_version format: "n42/1.0.0/v{index}"
        // Security note: validator index is self-reported via agent_version.
        // Real authentication happens at the BLS signature level in consensus.
        // A fake mapping only causes direct-push fallback to GossipSub.
        if let Some(idx_str) = info.agent_version.strip_prefix("n42/1.0.0/v")
            && let Ok(idx) = idx_str.parse::<u32>()
        {
            tracing::info!(%peer_id, validator_index = idx, "mapped peer to validator");
            // Update reverse map: remove any old peer that claimed this index
            self.peer_validator_map.retain(|_, v| *v != idx);
            self.peer_validator_map.insert(peer_id, idx);
            match self.validator_peer_map.write() {
                Ok(mut map) => { map.insert(idx, peer_id); }
                Err(e) => {
                    tracing::error!("validator_peer_map lock poisoned on write: {}", e);
                }
            }
        }

        if let Some(kad) = self.swarm.behaviour_mut().kademlia.as_mut() {
            for addr in &info.listen_addrs {
                kad.add_address(&peer_id, addr.clone());
            }
        }
    }

    fn handle_gossipsub_message(&self, source: PeerId, message: gossipsub::Message) {
        metrics::counter!("n42_gossipsub_messages_received").increment(1);
        tracing::info!(target: "n42::net", %source, topic = %message.topic, bytes = message.data.len(), "gossipsub message received");

        let acceptance = validate_message(
            &message.topic,
            &message.data,
            &self.consensus_topic_hash,
            &self.block_announce_topic_hash,
            &self.mempool_topic_hash,
            &self.blob_sidecar_topic_hash,
        );

        if !matches!(acceptance, gossipsub::MessageAcceptance::Accept) {
            tracing::warn!(%source, "rejected invalid gossipsub message");
            return;
        }

        let topic = &message.topic;
        let event = match () {
            _ if *topic == self.consensus_topic_hash => {
                match decode_consensus_message(&message.data) {
                    Ok(msg) => {
                        tracing::trace!(%source, "received consensus message");
                        NetworkEvent::ConsensusMessage { source, message: Box::new(msg) }
                    }
                    Err(e) => {
                        tracing::warn!(%source, error = %e, "failed to decode consensus message");
                        return;
                    }
                }
            }
            _ if *topic == self.block_announce_topic_hash => {
                NetworkEvent::BlockAnnouncement { source, data: message.data }
            }
            _ if *topic == self.mempool_topic_hash => {
                NetworkEvent::TransactionReceived { source, data: message.data }
            }
            _ if *topic == self.blob_sidecar_topic_hash => {
                NetworkEvent::BlobSidecarReceived { source, data: message.data }
            }
            _ if *topic == self.verification_receipts_topic_hash => {
                NetworkEvent::VerificationReceipt { source, data: message.data }
            }
            _ => {
                tracing::warn!(%source, %topic, "received message on unknown topic");
                return;
            }
        };

        self.emit_event(event);
    }

    fn handle_state_sync_event(
        &mut self,
        event: libp2p::request_response::Event<BlockSyncRequest, BlockSyncResponse>,
    ) {
        match event {
            libp2p::request_response::Event::Message { peer, message, .. } => match message {
                libp2p::request_response::Message::Request { request, channel, .. } => {
                    metrics::counter!("n42_state_sync_requests").increment(1);
                    let id = self.next_sync_id;
                    self.next_sync_id = self.next_sync_id.wrapping_add(1);

                    // Evict oldest stale channels to bound memory growth.
                    const MAX_PENDING: usize = 64;
                    if self.pending_sync_channels.len() >= MAX_PENDING {
                        let stale_count = self.pending_sync_channels.len() - MAX_PENDING + 1;
                        let mut ids: Vec<u64> =
                            self.pending_sync_channels.keys().copied().collect();
                        ids.sort_unstable();
                        for stale_id in ids.into_iter().take(stale_count) {
                            tracing::warn!(request_id = stale_id, "evicting stale sync channel");
                            self.pending_sync_channels.remove(&stale_id);
                        }
                    }

                    self.pending_sync_channels.insert(id, channel);
                    tracing::debug!(%peer, request_id = id, "received sync request");
                    self.emit_event(NetworkEvent::SyncRequest { peer, request_id: id, request });
                }
                libp2p::request_response::Message::Response { response, .. } => {
                    tracing::debug!(%peer, blocks = response.blocks.len(), "received sync response");
                    self.emit_event(NetworkEvent::SyncResponse { peer, response });
                }
            },
            libp2p::request_response::Event::OutboundFailure { peer, error, .. } => {
                tracing::warn!(%peer, %error, "sync request failed");
                self.emit_event(NetworkEvent::SyncRequestFailed {
                    peer,
                    error: error.to_string(),
                });
            }
            libp2p::request_response::Event::InboundFailure { peer, error, .. } => {
                tracing::debug!(%peer, %error, "sync inbound failure");
            }
            libp2p::request_response::Event::ResponseSent { peer, .. } => {
                tracing::debug!(%peer, "sync response sent");
            }
        }
    }

    fn handle_consensus_direct_event(
        &mut self,
        event: libp2p::request_response::Event<ConsensusDirectRequest, ConsensusDirectResponse>,
    ) {
        match event {
            libp2p::request_response::Event::Message { peer, message, .. } => match message {
                libp2p::request_response::Message::Request { request, channel, .. } => {
                    metrics::counter!("n42_direct_messages_received").increment(1);
                    match decode_consensus_message(&request.message_bytes) {
                        Ok(msg) => {
                            self.emit_event(NetworkEvent::ConsensusMessage {
                                source: peer,
                                message: Box::new(msg),
                            });
                            let _ = self.swarm.behaviour_mut().consensus_direct.send_response(
                                channel,
                                ConsensusDirectResponse { accepted: true },
                            );
                        }
                        Err(e) => {
                            tracing::warn!(%peer, error = %e, "invalid consensus direct message");
                            let _ = self.swarm.behaviour_mut().consensus_direct.send_response(
                                channel,
                                ConsensusDirectResponse { accepted: false },
                            );
                        }
                    }
                }
                libp2p::request_response::Message::Response { .. } => {
                    // ACK received — fire-and-forget semantics, no action needed.
                }
            },
            libp2p::request_response::Event::OutboundFailure { peer, error, .. } => {
                tracing::debug!(%peer, ?error, "consensus direct send failed");
                metrics::counter!("n42_direct_send_failures").increment(1);
            }
            libp2p::request_response::Event::InboundFailure { .. } => {}
            libp2p::request_response::Event::ResponseSent { .. } => {}
        }
    }

    fn handle_block_direct_event(
        &mut self,
        event: libp2p::request_response::Event<BlockDirectRequest, BlockDirectResponse>,
    ) {
        match event {
            libp2p::request_response::Event::Message { peer, message, .. } => match message {
                libp2p::request_response::Message::Request { request, channel, .. } => {
                    metrics::counter!("n42_block_direct_received").increment(1);
                    // Verify sender is a known validator
                    if let Some(&validator_idx) = self.peer_validator_map.get(&peer) {
                        tracing::debug!(%peer, validator_index = validator_idx, bytes = request.data.len(), "received block via direct push from validator");
                        self.emit_event(NetworkEvent::BlockAnnouncement {
                            source: peer,
                            data: request.data,
                        });
                        let _ = self.swarm.behaviour_mut().block_direct.send_response(
                            channel,
                            BlockDirectResponse { accepted: true },
                        );
                    } else {
                        tracing::warn!(%peer, bytes = request.data.len(), "rejected block direct push from non-validator peer");
                        metrics::counter!("n42_block_direct_rejected_non_validator").increment(1);
                        let _ = self.swarm.behaviour_mut().block_direct.send_response(
                            channel,
                            BlockDirectResponse { accepted: false },
                        );
                    }
                }
                libp2p::request_response::Message::Response { .. } => {
                    // ACK received — fire-and-forget.
                }
            },
            libp2p::request_response::Event::OutboundFailure { peer, error, .. } => {
                tracing::debug!(%peer, ?error, "block direct send failed");
                metrics::counter!("n42_block_direct_send_failures").increment(1);
            }
            libp2p::request_response::Event::InboundFailure { .. } => {}
            libp2p::request_response::Event::ResponseSent { .. } => {}
        }
    }

    fn handle_tx_forward_event(
        &mut self,
        event: libp2p::request_response::Event<TxForwardRequest, TxForwardResponse>,
    ) {
        match event {
            libp2p::request_response::Event::Message { peer, message, .. } => match message {
                libp2p::request_response::Message::Request { request, channel, .. } => {
                    let count = request.txs.len();
                    metrics::counter!("n42_tx_forward_batches_received").increment(1);
                    metrics::counter!("n42_tx_forward_txs_received").increment(count as u64);
                    if self.peer_validator_map.contains_key(&peer) {
                        tracing::debug!(%peer, count, "received forwarded tx batch from validator");
                        self.emit_event(NetworkEvent::TxForwardReceived {
                            source: peer,
                            txs: request.txs,
                        });
                        let _ = self.swarm.behaviour_mut().tx_forward.send_response(
                            channel,
                            TxForwardResponse { accepted: true },
                        );
                    } else {
                        tracing::warn!(%peer, count, "rejected tx forward from non-validator peer");
                        let _ = self.swarm.behaviour_mut().tx_forward.send_response(
                            channel,
                            TxForwardResponse { accepted: false },
                        );
                    }
                }
                libp2p::request_response::Message::Response { .. } => {}
            },
            libp2p::request_response::Event::OutboundFailure { peer, error, .. } => {
                tracing::debug!(%peer, ?error, "tx forward send failed");
            }
            libp2p::request_response::Event::InboundFailure { .. } => {}
            libp2p::request_response::Event::ResponseSent { .. } => {}
        }
    }

    fn handle_mdns_event(&mut self, event: libp2p::mdns::Event) {
        match event {
            libp2p::mdns::Event::Discovered(peers) => {
                for (peer_id, addr) in peers {
                    tracing::info!(%peer_id, %addr, "mDNS: discovered peer");
                    // Avoid add_explicit_peer() — explicit peers are excluded from
                    // GossipSub mesh formation. Dial instead and let heartbeat add them.
                    self.reconnection.register_peer(peer_id, vec![addr.clone()], false);
                    if let Err(e) = self.swarm.dial(addr.clone()) {
                        tracing::debug!(%peer_id, error = %e, "mDNS dial failed");
                    }
                }
            }
            libp2p::mdns::Event::Expired(peers) => {
                for (peer_id, _) in peers {
                    tracing::debug!(%peer_id, "mDNS: peer expired");
                }
            }
        }
    }

    fn handle_kademlia_event(&mut self, event: libp2p::kad::Event) {
        match event {
            libp2p::kad::Event::RoutingUpdated { peer, addresses, .. } => {
                tracing::debug!(%peer, addrs = addresses.len(), "kademlia routing updated");
            }
            libp2p::kad::Event::OutboundQueryProgressed { result, .. } => match result {
                libp2p::kad::QueryResult::GetClosestPeers(Ok(ok)) => {
                    tracing::debug!(peers = ok.peers.len(), "kademlia closest peers query done");
                    for peer_info in &ok.peers {
                        let peer_id = peer_info.peer_id;
                        if !self.swarm.is_connected(&peer_id) {
                            for addr in &peer_info.addrs {
                                if self.swarm.dial(addr.clone()).is_ok() {
                                    self.reconnection.register_peer(
                                        peer_id,
                                        vec![addr.clone()],
                                        false,
                                    );
                                    break;
                                }
                            }
                        }
                    }
                }
                libp2p::kad::QueryResult::Bootstrap(Ok(ok)) => {
                    tracing::debug!(num_remaining = ok.num_remaining, "kademlia bootstrap progress");
                }
                libp2p::kad::QueryResult::Bootstrap(Err(e)) => {
                    tracing::warn!(error = ?e, "kademlia bootstrap failed");
                }
                _ => {}
            },
            _ => {}
        }
    }

    /// Sends a network event to the node layer via the appropriate channel.
    ///
    /// Consensus messages AND block data go to the high-priority channel so they
    /// are never delayed by high-volume TX events in the data channel.
    /// BlockData is critical for follower import pipeline — deprioritizing it
    /// delays block import → delays voting → delays commit.
    fn emit_event(&self, event: NetworkEvent) {
        let tx = if matches!(event, NetworkEvent::ConsensusMessage { .. } | NetworkEvent::BlockAnnouncement { .. }) {
            &self.consensus_event_tx
        } else {
            &self.event_tx
        };
        match tx.try_send(event) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                metrics::counter!("n42_network_event_drops_total").increment(1);
                tracing::warn!("event channel full, dropping event");
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                tracing::warn!("event channel closed, consumer gone");
            }
        }
    }

    /// Publish raw data to a gossipsub topic.
    fn gossipsub_publish(&mut self, topic: gossipsub::IdentTopic, data: Vec<u8>, label: &str) {
        match self.swarm.behaviour_mut().gossipsub.publish(topic, data) {
            Ok(_) => {}
            Err(gossipsub::PublishError::InsufficientPeers) => {
                tracing::debug!("failed to publish {label}: InsufficientPeers");
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to publish {label}");
            }
        }
    }

    /// Encode a consensus message and publish it to the consensus topic.
    fn publish_consensus(&mut self, msg: &ConsensusMessage, label: &str) {
        match encode_consensus_message(msg) {
            Ok(data) => self.gossipsub_publish(consensus_topic(), data, label),
            Err(e) => tracing::error!(error = %e, "failed to encode {label}"),
        }
    }

    fn handle_command(&mut self, command: NetworkCommand) {
        match command {
            NetworkCommand::BroadcastConsensus(msg) => {
                metrics::counter!("n42_broadcast_messages_sent").increment(1);
                self.publish_consensus(&msg, "consensus message");
            }
            NetworkCommand::SendDirect { peer, message } => {
                metrics::counter!("n42_direct_messages_sent").increment(1);
                // Use request-response for true point-to-point delivery instead of gossipsub.
                match encode_consensus_message(&message) {
                    Ok(bytes) => {
                        let req = ConsensusDirectRequest { message_bytes: bytes };
                        self.swarm.behaviour_mut().consensus_direct.send_request(&peer, req);
                        tracing::debug!(%peer, "sending direct message via request-response");
                    }
                    Err(e) => {
                        tracing::warn!(%peer, error = %e, "failed to encode direct message, falling back to broadcast");
                        self.publish_consensus(&message, "direct fallback");
                    }
                }
            }
            NetworkCommand::AnnounceBlock(data) => {
                self.gossipsub_publish(block_announce_topic(), data, "block announcement");
            }
            NetworkCommand::BroadcastTransaction(data) => {
                if !self.tx_gossip_disabled {
                    self.gossipsub_publish(mempool_topic(), data, "transaction");
                }
            }
            NetworkCommand::BroadcastBlobSidecar(data) => {
                self.gossipsub_publish(blob_sidecar_topic(), data, "blob sidecar");
            }
            NetworkCommand::SendBlockDirect { peer, data } => {
                metrics::counter!("n42_block_direct_sent").increment(1);
                let req = BlockDirectRequest { data };
                self.swarm.behaviour_mut().block_direct.send_request(&peer, req);
            }
            NetworkCommand::ForwardTxBatch { peer, txs } => {
                let count = txs.len();
                metrics::counter!("n42_tx_forward_batches_sent").increment(1);
                metrics::counter!("n42_tx_forward_txs_sent").increment(count as u64);
                let req = TxForwardRequest { txs };
                self.swarm.behaviour_mut().tx_forward.send_request(&peer, req);
            }
            NetworkCommand::Dial(addr) => {
                if let Err(e) = self.swarm.dial(addr.clone()) {
                    tracing::warn!(%addr, error = %e, "failed to dial peer");
                }
            }
            NetworkCommand::RequestSync { peer, request } => {
                tracing::debug!(
                    %peer,
                    from_view = request.from_view,
                    to_view = request.to_view,
                    "sending sync request"
                );
                self.swarm.behaviour_mut().state_sync.send_request(&peer, request);
            }
            NetworkCommand::SendSyncResponse { request_id, response } => {
                if let Some(channel) = self.pending_sync_channels.remove(&request_id) {
                    let block_count = response.blocks.len();
                    if self
                        .swarm
                        .behaviour_mut()
                        .state_sync
                        .send_response(channel, response)
                        .is_err()
                    {
                        tracing::warn!(request_id, "failed to send sync response (channel closed)");
                    } else {
                        tracing::debug!(request_id, blocks = block_count, "sync response sent");
                    }
                } else {
                    tracing::warn!(request_id, "no pending channel for sync response");
                }
            }
            NetworkCommand::RegisterPeer { peer_id, addrs, trusted } => {
                tracing::debug!(%peer_id, trusted, addrs = addrs.len(), "registering peer");
                self.reconnection.register_peer(peer_id, addrs, trusted);
            }
            NetworkCommand::AddKademliaPeer { peer_id, addrs } => {
                if let Some(kad) = self.swarm.behaviour_mut().kademlia.as_mut() {
                    for addr in &addrs {
                        kad.add_address(&peer_id, addr.clone());
                    }
                    tracing::debug!(%peer_id, addrs = addrs.len(), "added peer to kademlia");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Creates a test handle with both normal and priority channels.
    /// Returns (handle, normal_rx, priority_rx).
    fn test_handle() -> (
        NetworkHandle,
        mpsc::UnboundedReceiver<NetworkCommand>,
        mpsc::UnboundedReceiver<NetworkCommand>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        let (ptx, prx) = mpsc::unbounded_channel();
        (NetworkHandle::new(tx, ptx), rx, prx)
    }

    #[test]
    fn test_handle_announce_block() {
        let (handle, _rx, mut prx) = test_handle();
        handle.announce_block(vec![1, 2, 3]).unwrap();
        match prx.try_recv().unwrap() {
            NetworkCommand::AnnounceBlock(data) => assert_eq!(data, vec![1, 2, 3]),
            other => panic!("expected AnnounceBlock, got {other:?}"),
        }
    }

    #[test]
    fn test_handle_broadcast_transaction() {
        let (handle, mut rx, _prx) = test_handle();
        handle.broadcast_transaction(vec![0xAA, 0xBB]).unwrap();
        match rx.try_recv().unwrap() {
            NetworkCommand::BroadcastTransaction(data) => assert_eq!(data, vec![0xAA, 0xBB]),
            other => panic!("expected BroadcastTransaction, got {other:?}"),
        }
    }

    #[test]
    fn test_handle_dial() {
        let (handle, mut rx, _prx) = test_handle();
        let addr: Multiaddr = "/ip4/127.0.0.1/udp/9400/quic-v1".parse().unwrap();
        handle.dial(addr.clone()).unwrap();
        match rx.try_recv().unwrap() {
            NetworkCommand::Dial(a) => assert_eq!(a, addr),
            other => panic!("expected Dial, got {other:?}"),
        }
    }

    #[test]
    fn test_handle_channel_closed_returns_error() {
        let (handle, _rx, prx) = test_handle();
        drop(prx);
        let result = handle.announce_block(vec![1]);
        assert!(matches!(result.unwrap_err(), NetworkError::ChannelClosed));
    }

    #[test]
    fn test_handle_validator_peer_map() {
        let (handle, _rx, _prx) = test_handle();
        assert!(handle.validator_peer(0).is_none());

        let peer = PeerId::random();
        handle.validator_peer_map.write().unwrap().insert(0, peer);
        assert_eq!(handle.validator_peer(0), Some(peer));
        assert!(handle.validator_peer(1).is_none());
    }

    #[test]
    fn test_handle_register_peer() {
        let (handle, mut rx, _prx) = test_handle();
        let peer = PeerId::random();
        let addr: Multiaddr = "/ip4/10.0.0.1/udp/9400/quic-v1".parse().unwrap();
        handle.register_peer(peer, vec![addr], true).unwrap();
        match rx.try_recv().unwrap() {
            NetworkCommand::RegisterPeer { peer_id, addrs, trusted } => {
                assert_eq!(peer_id, peer);
                assert_eq!(addrs.len(), 1);
                assert!(trusted);
            }
            other => panic!("expected RegisterPeer, got {other:?}"),
        }
    }

    #[test]
    fn test_handle_request_sync() {
        let (handle, mut rx, _prx) = test_handle();
        let peer = PeerId::random();
        let request = BlockSyncRequest { from_view: 10, to_view: 20, local_committed_view: 5 };
        handle.request_sync(peer, request).unwrap();
        match rx.try_recv().unwrap() {
            NetworkCommand::RequestSync { peer: p, request: r } => {
                assert_eq!(p, peer);
                assert_eq!(r.from_view, 10);
                assert_eq!(r.to_view, 20);
            }
            other => panic!("expected RequestSync, got {other:?}"),
        }
    }

    #[test]
    fn test_handle_broadcast_blob_sidecar() {
        let (handle, mut rx, _prx) = test_handle();
        handle.broadcast_blob_sidecar(vec![0xDE, 0xAD]).unwrap();
        match rx.try_recv().unwrap() {
            NetworkCommand::BroadcastBlobSidecar(data) => assert_eq!(data, vec![0xDE, 0xAD]),
            other => panic!("expected BroadcastBlobSidecar, got {other:?}"),
        }
    }

    #[test]
    fn test_handle_clone_shares_peer_map() {
        let (handle1, _rx, _prx) = test_handle();
        let handle2 = handle1.clone();
        let peer = PeerId::random();
        handle1.validator_peer_map.write().unwrap().insert(42, peer);
        assert_eq!(handle2.validator_peer(42), Some(peer));
    }

    #[test]
    fn test_block_direct_uses_priority_channel() {
        let (handle, _rx, mut prx) = test_handle();
        let peer = PeerId::random();
        handle.send_block_direct(peer, vec![0x42]).unwrap();
        match prx.try_recv().unwrap() {
            NetworkCommand::SendBlockDirect { peer: p, data } => {
                assert_eq!(p, peer);
                assert_eq!(data, vec![0x42]);
            }
            other => panic!("expected SendBlockDirect, got {other:?}"),
        }
    }

}
