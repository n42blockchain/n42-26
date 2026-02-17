use libp2p::gossipsub;
use libp2p::swarm::SwarmEvent;
use libp2p::{Multiaddr, PeerId, Swarm};
use n42_primitives::ConsensusMessage;
use std::collections::HashMap;
use tokio::sync::mpsc;

use crate::error::NetworkError;
use crate::gossipsub::handlers::{
    decode_consensus_message, encode_consensus_message, validate_message,
};
use crate::gossipsub::topics::{block_announce_topic, consensus_topic, mempool_topic, verification_receipts_topic};
use crate::state_sync::{BlockSyncRequest, BlockSyncResponse};
use crate::transport::{N42Behaviour, N42BehaviourEvent};

/// Commands sent to the network service from the node layer.
#[derive(Debug)]
pub enum NetworkCommand {
    /// Broadcast a consensus message to all peers via GossipSub.
    BroadcastConsensus(ConsensusMessage),
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
}

impl NetworkHandle {
    /// Creates a new `NetworkHandle` from a command channel sender.
    pub fn new(command_tx: mpsc::UnboundedSender<NetworkCommand>) -> Self {
        Self { command_tx }
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

        let handle = NetworkHandle { command_tx };

        let service = Self {
            swarm,
            command_rx,
            event_tx,
            consensus_topic_hash,
            block_announce_topic_hash,
            mempool_topic_hash,
            pending_sync_channels: HashMap::new(),
            next_sync_id: 0,
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
                    "identified peer"
                );
            }
            SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                tracing::info!(%peer_id, "peer connected");
                let _ = self.event_tx.send(NetworkEvent::PeerConnected(peer_id));
            }
            SwarmEvent::ConnectionClosed { peer_id, .. } => {
                tracing::info!(%peer_id, "peer disconnected");
                let _ = self.event_tx.send(NetworkEvent::PeerDisconnected(peer_id));
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
                        let id = self.next_sync_id;
                        self.next_sync_id += 1;
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

    /// Handles a command from the node layer.
    fn handle_command(&mut self, command: NetworkCommand) {
        match command {
            NetworkCommand::BroadcastConsensus(msg) => {
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
        }
    }
}

use futures::StreamExt;
