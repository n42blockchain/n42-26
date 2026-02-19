use futures::StreamExt;
use libp2p::gossipsub;
use libp2p::swarm::SwarmEvent;
use libp2p::{Multiaddr, PeerId, Swarm};
use n42_primitives::ConsensusMessage;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::interval;

use crate::error::NetworkError;
use crate::gossipsub::handlers::{
    decode_consensus_message, encode_consensus_message, validate_message,
};
use crate::gossipsub::topics::{block_announce_topic, consensus_topic, mempool_topic, verification_receipts_topic};
use crate::reconnection::ReconnectionManager;
use crate::state_sync::{BlockSyncRequest, BlockSyncResponse};
use crate::transport::{N42Behaviour, N42BehaviourEvent};

/// Commands sent to the network service from the node layer.
#[derive(Debug)]
pub enum NetworkCommand {
    /// Broadcast a consensus message to all peers via GossipSub.
    BroadcastConsensus(ConsensusMessage),
    /// Send a consensus message directly to a specific peer via request-response.
    SendDirect { peer: PeerId, message: ConsensusMessage },
    /// Publish raw bytes to the block announcement topic.
    AnnounceBlock(Vec<u8>),
    /// Broadcast a raw transaction (RLP-encoded) to the mempool topic.
    BroadcastTransaction(Vec<u8>),
    /// Connect to a peer at the given multiaddr.
    Dial(Multiaddr),
    /// Request block sync from a specific peer.
    RequestSync { peer: PeerId, request: BlockSyncRequest },
    /// Send a sync response back to a requesting peer.
    SendSyncResponse { request_id: u64, response: BlockSyncResponse },
    /// Register a peer for automatic reconnection.
    RegisterPeer { peer_id: PeerId, addrs: Vec<Multiaddr>, trusted: bool },
    /// Add a peer to the Kademlia routing table.
    AddKademliaPeer { peer_id: PeerId, addrs: Vec<Multiaddr> },
}

/// Events produced by the network service for the node layer.
#[derive(Debug)]
pub enum NetworkEvent {
    /// A consensus message received from a peer.
    ConsensusMessage {
        source: PeerId,
        message: ConsensusMessage,
    },
    /// A block announcement received from a peer.
    BlockAnnouncement {
        source: PeerId,
        data: Vec<u8>,
    },
    /// A verification receipt received from a peer.
    VerificationReceipt {
        source: PeerId,
        data: Vec<u8>,
    },
    /// A transaction received from the mempool topic.
    TransactionReceived {
        source: PeerId,
        data: Vec<u8>,
    },
    /// A new peer connected.
    PeerConnected(PeerId),
    /// A peer disconnected.
    PeerDisconnected(PeerId),
    /// An inbound block sync request from a peer.
    SyncRequest {
        peer: PeerId,
        request_id: u64,
        request: BlockSyncRequest,
    },
    /// A block sync response from a peer (reply to our RequestSync).
    SyncResponse {
        peer: PeerId,
        response: BlockSyncResponse,
    },
    /// A sync request to a peer failed.
    SyncRequestFailed {
        peer: PeerId,
        error: String,
    },
}

/// Handle for sending commands to the running NetworkService.
///
/// This is the main interface used by the node layer to interact
/// with the network. It is cheaply cloneable.
#[derive(Clone, Debug)]
pub struct NetworkHandle {
    command_tx: mpsc::UnboundedSender<NetworkCommand>,
    /// Mapping from validator index to PeerId, populated from Identify events.
    /// Protected by a shared lock for cross-thread updates.
    validator_peer_map: std::sync::Arc<std::sync::RwLock<HashMap<u32, PeerId>>>,
}

impl NetworkHandle {
    /// Creates a new `NetworkHandle` from a command channel sender.
    pub fn new(command_tx: mpsc::UnboundedSender<NetworkCommand>) -> Self {
        Self {
            command_tx,
            validator_peer_map: std::sync::Arc::new(std::sync::RwLock::new(HashMap::new())),
        }
    }

    /// Returns the PeerId for a given validator index, if known.
    pub fn validator_peer(&self, index: u32) -> Option<PeerId> {
        self.validator_peer_map.read().ok()?.get(&index).copied()
    }

    /// Sends a consensus message directly to a specific peer.
    pub fn send_direct(&self, peer: PeerId, msg: ConsensusMessage) -> Result<(), NetworkError> {
        self.command_tx
            .send(NetworkCommand::SendDirect { peer, message: msg })
            .map_err(|_| NetworkError::ChannelClosed)
    }

    /// Broadcasts a consensus message to the GossipSub network.
    pub fn broadcast_consensus(&self, msg: ConsensusMessage) -> Result<(), NetworkError> {
        self.command_tx
            .send(NetworkCommand::BroadcastConsensus(msg))
            .map_err(|_| NetworkError::ChannelClosed)
    }

    /// Publishes a block announcement.
    pub fn announce_block(&self, data: Vec<u8>) -> Result<(), NetworkError> {
        self.command_tx
            .send(NetworkCommand::AnnounceBlock(data))
            .map_err(|_| NetworkError::ChannelClosed)
    }

    /// Broadcasts a raw transaction to the mempool topic.
    pub fn broadcast_transaction(&self, data: Vec<u8>) -> Result<(), NetworkError> {
        self.command_tx
            .send(NetworkCommand::BroadcastTransaction(data))
            .map_err(|_| NetworkError::ChannelClosed)
    }

    /// Dials a peer at the given multiaddr.
    pub fn dial(&self, addr: Multiaddr) -> Result<(), NetworkError> {
        self.command_tx
            .send(NetworkCommand::Dial(addr))
            .map_err(|_| NetworkError::ChannelClosed)
    }

    /// Sends a block sync request to a specific peer.
    pub fn request_sync(&self, peer: PeerId, request: BlockSyncRequest) -> Result<(), NetworkError> {
        self.command_tx
            .send(NetworkCommand::RequestSync { peer, request })
            .map_err(|_| NetworkError::ChannelClosed)
    }

    /// Sends a sync response back to a peer that requested blocks.
    pub fn send_sync_response(&self, request_id: u64, response: BlockSyncResponse) -> Result<(), NetworkError> {
        self.command_tx
            .send(NetworkCommand::SendSyncResponse { request_id, response })
            .map_err(|_| NetworkError::ChannelClosed)
    }

    /// Registers a peer for automatic reconnection management.
    ///
    /// Trusted peers are retried indefinitely with exponential backoff.
    /// Discovered peers are retried up to a limited number of times.
    pub fn register_peer(&self, peer_id: PeerId, addrs: Vec<Multiaddr>, trusted: bool) -> Result<(), NetworkError> {
        self.command_tx
            .send(NetworkCommand::RegisterPeer { peer_id, addrs, trusted })
            .map_err(|_| NetworkError::ChannelClosed)
    }

    /// Adds a peer to the Kademlia DHT routing table.
    pub fn add_kademlia_peer(&self, peer_id: PeerId, addrs: Vec<Multiaddr>) -> Result<(), NetworkError> {
        self.command_tx
            .send(NetworkCommand::AddKademliaPeer { peer_id, addrs })
            .map_err(|_| NetworkError::ChannelClosed)
    }
}

/// The network service manages the libp2p swarm and bridges
/// consensus messages between the P2P network and the node layer.
///
/// It runs as a background task, processing swarm events and
/// commands from the node. The service is driven by a `select!`
/// loop over:
/// 1. Swarm events (incoming messages, peer connections)
/// 2. Commands from the node layer (broadcast, dial)
pub struct NetworkService {
    /// The libp2p swarm.
    swarm: Swarm<N42Behaviour>,
    /// Receiver for commands from the node layer.
    command_rx: mpsc::UnboundedReceiver<NetworkCommand>,
    /// Sender for events to the node layer.
    event_tx: mpsc::UnboundedSender<NetworkEvent>,
    /// Topic hash for the consensus topic (cached for validation).
    consensus_topic_hash: gossipsub::TopicHash,
    /// Topic hash for the block announcement topic (cached for routing).
    block_announce_topic_hash: gossipsub::TopicHash,
    /// Topic hash for the mempool topic (cached for routing).
    mempool_topic_hash: gossipsub::TopicHash,
    /// Pending sync response channels, keyed by internal request ID.
    /// Maps our internal u64 ID to the libp2p ResponseChannel for sending replies.
    pending_sync_channels: HashMap<u64, libp2p::request_response::ResponseChannel<BlockSyncResponse>>,
    /// Next available sync request ID counter.
    next_sync_id: u64,
    /// Shared validator-to-peer mapping, updated from Identify events.
    validator_peer_map: std::sync::Arc<std::sync::RwLock<HashMap<u32, PeerId>>>,
    /// Automatic reconnection manager with exponential backoff.
    reconnection: ReconnectionManager,
}

impl NetworkService {
    /// Creates a new network service and returns the service + handle + event receiver.
    ///
    /// The caller should:
    /// 1. Use `NetworkHandle` to send commands (broadcast, dial)
    /// 2. Read from `event_rx` to receive network events
    /// 3. Spawn the service with `service.run()`
    pub fn new(
        mut swarm: Swarm<N42Behaviour>,
    ) -> Result<(Self, NetworkHandle, mpsc::UnboundedReceiver<NetworkEvent>), NetworkError> {
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        // Subscribe to GossipSub topics
        let consensus = consensus_topic();
        let block_announce = block_announce_topic();
        let verification = verification_receipts_topic();
        let mempool = mempool_topic();

        swarm
            .behaviour_mut()
            .gossipsub
            .subscribe(&consensus)
            .map_err(|e| NetworkError::Subscribe(e.to_string()))?;
        swarm
            .behaviour_mut()
            .gossipsub
            .subscribe(&block_announce)
            .map_err(|e| NetworkError::Subscribe(e.to_string()))?;
        swarm
            .behaviour_mut()
            .gossipsub
            .subscribe(&verification)
            .map_err(|e| NetworkError::Subscribe(e.to_string()))?;
        swarm
            .behaviour_mut()
            .gossipsub
            .subscribe(&mempool)
            .map_err(|e| NetworkError::Subscribe(e.to_string()))?;

        let consensus_topic_hash = consensus.hash();
        let block_announce_topic_hash = block_announce.hash();
        let mempool_topic_hash = mempool.hash();

        let validator_peer_map = std::sync::Arc::new(std::sync::RwLock::new(HashMap::new()));

        let handle = NetworkHandle {
            command_tx,
            validator_peer_map: validator_peer_map.clone(),
        };

        let service = Self {
            swarm,
            command_rx,
            event_tx,
            consensus_topic_hash,
            block_announce_topic_hash,
            mempool_topic_hash,
            pending_sync_channels: HashMap::new(),
            next_sync_id: 0,
            validator_peer_map,
            reconnection: ReconnectionManager::new(),
        };

        Ok((service, handle, event_rx))
    }

    /// Starts listening on the given address.
    pub fn listen_on(&mut self, addr: Multiaddr) -> Result<(), NetworkError> {
        self.swarm
            .listen_on(addr)
            .map_err(|e| NetworkError::Listen(e.to_string()))?;
        Ok(())
    }

    /// Dials a peer at the given multiaddr.
    pub fn dial(&mut self, addr: Multiaddr) -> Result<(), NetworkError> {
        self.swarm
            .dial(addr)
            .map_err(|e| NetworkError::Dial(e.to_string()))?;
        Ok(())
    }

    /// Runs the network service event loop.
    ///
    /// This is the main loop that processes both swarm events and
    /// commands from the node layer. It should be spawned as a
    /// background task via `tokio::spawn` or `spawn_critical`.
    pub async fn run(mut self) {
        tracing::info!("network service started");

        // Reconnection check interval: every 5 seconds.
        let mut reconnect_interval = interval(Duration::from_secs(5));
        // Don't fire immediately on start.
        reconnect_interval.tick().await;

        // Kademlia DHT refresh interval: every 5 minutes.
        let mut dht_refresh_interval = interval(Duration::from_secs(300));
        dht_refresh_interval.tick().await;

        // Bootstrap Kademlia on startup if enabled.
        if let Some(kad) = self.swarm.behaviour_mut().kademlia.as_mut() {
            if let Err(e) = kad.bootstrap() {
                tracing::debug!(error = ?e, "kademlia bootstrap failed (no known peers yet)");
            }
        }

        loop {
            tokio::select! {
                // Process swarm events
                event = self.swarm.select_next_some() => {
                    self.handle_swarm_event(event);
                }
                // Process commands from the node layer
                cmd = self.command_rx.recv() => {
                    match cmd {
                        Some(command) => self.handle_command(command),
                        None => {
                            tracing::info!("command channel closed, shutting down");
                            break;
                        }
                    }
                }
                // Periodic reconnection attempts
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
                // Periodic DHT refresh: query random peer ID to discover new peers.
                _ = dht_refresh_interval.tick() => {
                    if let Some(kad) = self.swarm.behaviour_mut().kademlia.as_mut() {
                        let random_peer = PeerId::random();
                        kad.get_closest_peers(random_peer);
                        tracing::debug!("kademlia DHT refresh: querying closest peers");
                    }
                }
            }
        }
    }

    /// Handles a swarm event.
    fn handle_swarm_event(&mut self, event: SwarmEvent<N42BehaviourEvent>) {
        match event {
            SwarmEvent::Behaviour(N42BehaviourEvent::Gossipsub(
                gossipsub::Event::Message {
                    propagation_source,
                    message,
                    message_id: _,
                },
            )) => {
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
            SwarmEvent::Behaviour(N42BehaviourEvent::Identify(
                libp2p::identify::Event::Received { peer_id, info, .. },
            )) => {
                tracing::debug!(
                    %peer_id,
                    protocol = ?info.protocol_version,
                    agent = ?info.agent_version,
                    "identified peer"
                );
                // Extract validator index from agent_version (format: "n42/1.0.0/v{index}").
                if let Some(idx_str) = info.agent_version.strip_prefix("n42/1.0.0/v") {
                    if let Ok(idx) = idx_str.parse::<u32>() {
                        tracing::info!(
                            %peer_id,
                            validator_index = idx,
                            "mapped peer to validator index"
                        );
                        if let Ok(mut map) = self.validator_peer_map.write() {
                            map.insert(idx, peer_id);
                        }
                    }
                }
                // Register identified peer's addresses into Kademlia DHT.
                if let Some(kad) = self.swarm.behaviour_mut().kademlia.as_mut() {
                    for addr in &info.listen_addrs {
                        kad.add_address(&peer_id, addr.clone());
                    }
                }
            }
            SwarmEvent::Behaviour(N42BehaviourEvent::Kademlia(event)) => {
                self.handle_kademlia_event(event);
            }
            SwarmEvent::Behaviour(N42BehaviourEvent::ConnectionLimits(event)) => {
                // connection_limits::Behaviour emits void::Void — unreachable at runtime
                // but required by the exhaustive match on N42BehaviourEvent.
                match event {}
            }
            SwarmEvent::Behaviour(N42BehaviourEvent::Mdns(event)) => {
                self.handle_mdns_event(event);
            }
            SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                tracing::info!(%peer_id, "peer connected");
                metrics::gauge!("n42_active_peer_connections").increment(1.0);
                self.reconnection.on_connected(&peer_id);
                let _ = self.event_tx.send(NetworkEvent::PeerConnected(peer_id));
            }
            SwarmEvent::ConnectionClosed { peer_id, .. } => {
                tracing::info!(%peer_id, "peer disconnected");
                metrics::gauge!("n42_active_peer_connections").decrement(1.0);
                self.reconnection.on_disconnected(&peer_id);
                let _ = self.event_tx.send(NetworkEvent::PeerDisconnected(peer_id));
            }
            SwarmEvent::OutgoingConnectionError { peer_id, .. } => {
                if let Some(peer_id) = peer_id {
                    tracing::debug!(%peer_id, "outgoing connection error");
                    self.reconnection.on_dial_failure(&peer_id);
                }
            }
            SwarmEvent::NewListenAddr { address, .. } => {
                let local_peer_id = *self.swarm.local_peer_id();
                tracing::info!(%local_peer_id, %address, "listening on address");
            }
            _ => {}
        }
    }

    /// Handles an incoming GossipSub message.
    fn handle_gossipsub_message(&self, source: PeerId, message: gossipsub::Message) {
        metrics::counter!("n42_gossipsub_messages_received").increment(1);

        // Validate the message
        let acceptance = validate_message(
            &message.topic,
            &message.data,
            &self.consensus_topic_hash,
            &self.block_announce_topic_hash,
            &self.mempool_topic_hash,
        );

        if !matches!(acceptance, gossipsub::MessageAcceptance::Accept) {
            tracing::warn!(%source, "rejected invalid gossipsub message");
            return;
        }

        if message.topic == self.consensus_topic_hash {
            // Consensus message
            match decode_consensus_message(&message.data) {
                Ok(msg) => {
                    tracing::trace!(%source, "received consensus message");
                    let _ = self.event_tx.send(NetworkEvent::ConsensusMessage {
                        source,
                        message: msg,
                    });
                }
                Err(e) => {
                    tracing::warn!(%source, error = %e, "failed to decode consensus message");
                }
            }
        } else if message.topic == self.block_announce_topic_hash {
            // Block announcement
            let _ = self.event_tx.send(NetworkEvent::BlockAnnouncement {
                source,
                data: message.data,
            });
        } else if message.topic == self.mempool_topic_hash {
            // Transaction from mempool topic
            let _ = self.event_tx.send(NetworkEvent::TransactionReceived {
                source,
                data: message.data,
            });
        } else {
            // Verification receipts or unknown topic
            let _ = self.event_tx.send(NetworkEvent::VerificationReceipt {
                source,
                data: message.data,
            });
        }
    }

    /// Handles a state sync (request-response) event.
    fn handle_state_sync_event(
        &mut self,
        event: libp2p::request_response::Event<BlockSyncRequest, BlockSyncResponse>,
    ) {
        match event {
            libp2p::request_response::Event::Message { peer, message, .. } => {
                match message {
                    libp2p::request_response::Message::Request { request, channel, .. } => {
                        metrics::counter!("n42_state_sync_requests").increment(1);
                        let id = self.next_sync_id;
                        self.next_sync_id += 1;

                        // Bound pending channels to prevent unbounded growth if
                        // upper layers fail to send responses. Evict the oldest
                        // (lowest ID) entries first — they are likely stale.
                        const MAX_PENDING_SYNC_CHANNELS: usize = 64;
                        if self.pending_sync_channels.len() >= MAX_PENDING_SYNC_CHANNELS {
                            let stale_count = self.pending_sync_channels.len() - MAX_PENDING_SYNC_CHANNELS + 1;
                            let mut stale_ids: Vec<u64> = self.pending_sync_channels.keys().copied().collect();
                            stale_ids.sort_unstable();
                            for stale_id in stale_ids.into_iter().take(stale_count) {
                                tracing::warn!(request_id = stale_id, "evicting stale sync channel");
                                self.pending_sync_channels.remove(&stale_id);
                            }
                        }

                        self.pending_sync_channels.insert(id, channel);
                        tracing::debug!(%peer, request_id = id, "received sync request");
                        let _ = self.event_tx.send(NetworkEvent::SyncRequest {
                            peer,
                            request_id: id,
                            request,
                        });
                    }
                    libp2p::request_response::Message::Response { response, .. } => {
                        tracing::debug!(%peer, blocks = response.blocks.len(), "received sync response");
                        let _ = self.event_tx.send(NetworkEvent::SyncResponse {
                            peer,
                            response,
                        });
                    }
                }
            }
            libp2p::request_response::Event::OutboundFailure { peer, error, .. } => {
                tracing::warn!(%peer, %error, "sync request failed");
                let _ = self.event_tx.send(NetworkEvent::SyncRequestFailed {
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

    /// Handles an mDNS event (LAN peer discovery).
    fn handle_mdns_event(&mut self, event: libp2p::mdns::Event) {
        match event {
            libp2p::mdns::Event::Discovered(peers) => {
                for (peer_id, addr) in peers {
                    tracing::info!(%peer_id, %addr, "mDNS: discovered peer on LAN");
                    // Add the peer's address and dial them.
                    self.swarm.behaviour_mut().gossipsub.add_explicit_peer(&peer_id);
                    // Register for reconnection (non-trusted, mDNS may re-discover).
                    self.reconnection.register_peer(peer_id, vec![addr.clone()], false);
                    if let Err(e) = self.swarm.dial(addr.clone()) {
                        tracing::debug!(%peer_id, %addr, error = %e, "mDNS: dial failed (may already be connected)");
                    }
                }
            }
            libp2p::mdns::Event::Expired(peers) => {
                for (peer_id, _addr) in peers {
                    tracing::debug!(%peer_id, "mDNS: peer expired");
                }
            }
        }
    }

    /// Handles a Kademlia DHT event.
    fn handle_kademlia_event(&mut self, event: libp2p::kad::Event) {
        match event {
            libp2p::kad::Event::RoutingUpdated { peer, addresses, .. } => {
                tracing::debug!(
                    %peer,
                    addrs = addresses.len(),
                    "kademlia routing table updated"
                );
            }
            libp2p::kad::Event::OutboundQueryProgressed { result, .. } => {
                match result {
                    libp2p::kad::QueryResult::GetClosestPeers(Ok(ok)) => {
                        tracing::debug!(
                            peers = ok.peers.len(),
                            "kademlia closest peers query completed"
                        );
                        // Dial newly discovered peers.
                        for peer_info in &ok.peers {
                            let peer_id = peer_info.peer_id;
                            if !self.swarm.is_connected(&peer_id) {
                                for addr in &peer_info.addrs {
                                    if let Err(e) = self.swarm.dial(addr.clone()) {
                                        tracing::trace!(%peer_id, error = %e, "kad: dial discovered peer failed");
                                    } else {
                                        // Register discovered peer for reconnection (non-trusted).
                                        self.reconnection.register_peer(peer_id, vec![addr.clone()], false);
                                        break; // One dial attempt per peer is enough.
                                    }
                                }
                            }
                        }
                    }
                    libp2p::kad::QueryResult::Bootstrap(Ok(ok)) => {
                        tracing::debug!(
                            num_remaining = ok.num_remaining,
                            "kademlia bootstrap progress"
                        );
                    }
                    libp2p::kad::QueryResult::Bootstrap(Err(e)) => {
                        tracing::warn!(error = ?e, "kademlia bootstrap failed");
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    /// Handles a command from the node layer.
    fn handle_command(&mut self, command: NetworkCommand) {
        match command {
            NetworkCommand::BroadcastConsensus(msg) => {
                metrics::counter!("n42_broadcast_messages_sent").increment(1);
                match encode_consensus_message(&msg) {
                    Ok(data) => {
                        let topic = consensus_topic();
                        if let Err(e) = self.swarm.behaviour_mut().gossipsub.publish(topic, data) {
                            tracing::warn!(error = %e, "failed to publish consensus message");
                        }
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "failed to encode consensus message");
                    }
                }
            }
            NetworkCommand::SendDirect { peer, message } => {
                // Direct messaging: encode as consensus message and publish via GossipSub.
                // The message is broadcast to the mesh but the intent is point-to-point.
                // In a future optimization, this can use request-response for true P2P delivery.
                metrics::counter!("n42_direct_messages_sent").increment(1);
                match encode_consensus_message(&message) {
                    Ok(data) => {
                        let topic = consensus_topic();
                        tracing::debug!(%peer, "sending direct message (via GossipSub broadcast)");
                        if let Err(e) = self.swarm.behaviour_mut().gossipsub.publish(topic, data) {
                            tracing::warn!(%peer, error = %e, "failed to send direct message");
                        }
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "failed to encode direct consensus message");
                    }
                }
            }
            NetworkCommand::AnnounceBlock(data) => {
                let topic = block_announce_topic();
                if let Err(e) = self.swarm.behaviour_mut().gossipsub.publish(topic, data) {
                    tracing::warn!(error = %e, "failed to publish block announcement");
                }
            }
            NetworkCommand::BroadcastTransaction(data) => {
                let topic = mempool_topic();
                if let Err(e) = self.swarm.behaviour_mut().gossipsub.publish(topic, data) {
                    tracing::trace!(error = %e, "failed to publish transaction (no peers yet?)");
                }
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
                let _req_id = self.swarm.behaviour_mut().state_sync.send_request(&peer, request);
            }
            NetworkCommand::SendSyncResponse { request_id, response } => {
                if let Some(channel) = self.pending_sync_channels.remove(&request_id) {
                    let block_count = response.blocks.len();
                    if self.swarm.behaviour_mut().state_sync.send_response(channel, response).is_err() {
                        tracing::warn!(request_id, "failed to send sync response (channel closed)");
                    } else {
                        tracing::debug!(request_id, blocks = block_count, "sync response sent");
                    }
                } else {
                    tracing::warn!(request_id, "no pending channel for sync response");
                }
            }
            NetworkCommand::RegisterPeer { peer_id, addrs, trusted } => {
                tracing::debug!(
                    %peer_id,
                    trusted,
                    addrs = addrs.len(),
                    "registering peer for reconnection"
                );
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

    #[test]
    fn test_handle_announce_block() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let handle = NetworkHandle::new(tx);

        handle.announce_block(vec![1, 2, 3]).unwrap();

        match rx.try_recv().unwrap() {
            NetworkCommand::AnnounceBlock(data) => assert_eq!(data, vec![1, 2, 3]),
            other => panic!("expected AnnounceBlock, got {:?}", other),
        }
    }

    #[test]
    fn test_handle_broadcast_transaction() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let handle = NetworkHandle::new(tx);

        handle.broadcast_transaction(vec![0xAA, 0xBB]).unwrap();

        match rx.try_recv().unwrap() {
            NetworkCommand::BroadcastTransaction(data) => assert_eq!(data, vec![0xAA, 0xBB]),
            other => panic!("expected BroadcastTransaction, got {:?}", other),
        }
    }

    #[test]
    fn test_handle_dial() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let handle = NetworkHandle::new(tx);

        let addr: Multiaddr = "/ip4/127.0.0.1/udp/9400/quic-v1".parse().unwrap();
        handle.dial(addr.clone()).unwrap();

        match rx.try_recv().unwrap() {
            NetworkCommand::Dial(a) => assert_eq!(a, addr),
            other => panic!("expected Dial, got {:?}", other),
        }
    }

    #[test]
    fn test_handle_channel_closed_returns_error() {
        let (tx, rx) = mpsc::unbounded_channel();
        let handle = NetworkHandle::new(tx);
        drop(rx);

        let result = handle.announce_block(vec![1]);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), NetworkError::ChannelClosed));
    }

    #[test]
    fn test_handle_validator_peer_map() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let handle = NetworkHandle::new(tx);

        // Initially empty.
        assert!(handle.validator_peer(0).is_none());
        assert!(handle.validator_peer(99).is_none());

        // Insert a mapping.
        let peer = PeerId::random();
        handle.validator_peer_map.write().unwrap().insert(0, peer);

        assert_eq!(handle.validator_peer(0), Some(peer));
        assert!(handle.validator_peer(1).is_none());
    }

    #[test]
    fn test_handle_register_peer() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let handle = NetworkHandle::new(tx);

        let peer = PeerId::random();
        let addr: Multiaddr = "/ip4/10.0.0.1/udp/9400/quic-v1".parse().unwrap();
        handle.register_peer(peer, vec![addr], true).unwrap();

        match rx.try_recv().unwrap() {
            NetworkCommand::RegisterPeer { peer_id, addrs, trusted } => {
                assert_eq!(peer_id, peer);
                assert_eq!(addrs.len(), 1);
                assert!(trusted);
            }
            other => panic!("expected RegisterPeer, got {:?}", other),
        }
    }

    #[test]
    fn test_handle_request_sync() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let handle = NetworkHandle::new(tx);

        let peer = PeerId::random();
        let request = BlockSyncRequest {
            from_view: 10,
            to_view: 20,
            local_committed_view: 5,
        };
        handle.request_sync(peer, request).unwrap();

        match rx.try_recv().unwrap() {
            NetworkCommand::RequestSync { peer: p, request: r } => {
                assert_eq!(p, peer);
                assert_eq!(r.from_view, 10);
                assert_eq!(r.to_view, 20);
            }
            other => panic!("expected RequestSync, got {:?}", other),
        }
    }

    #[test]
    fn test_handle_clone_shares_peer_map() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let handle1 = NetworkHandle::new(tx);
        let handle2 = handle1.clone();

        let peer = PeerId::random();
        handle1.validator_peer_map.write().unwrap().insert(42, peer);

        // Cloned handle should see the same mapping.
        assert_eq!(handle2.validator_peer(42), Some(peer));
    }
}
