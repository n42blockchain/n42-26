use libp2p::gossipsub;
use libp2p::swarm::SwarmEvent;
use libp2p::{Multiaddr, PeerId, Swarm};
use n42_primitives::ConsensusMessage;
use tokio::sync::mpsc;

use crate::error::NetworkError;
use crate::gossipsub::handlers::{
    decode_consensus_message, encode_consensus_message, validate_message,
};
use crate::gossipsub::topics::{block_announce_topic, consensus_topic, verification_receipts_topic};
use crate::transport::{N42Behaviour, N42BehaviourEvent};

/// Commands sent to the network service from the node layer.
#[derive(Debug)]
pub enum NetworkCommand {
    /// Broadcast a consensus message to all peers via GossipSub.
    BroadcastConsensus(ConsensusMessage),
    /// Publish raw bytes to the block announcement topic.
    AnnounceBlock(Vec<u8>),
    /// Connect to a peer at the given multiaddr.
    Dial(Multiaddr),
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
    /// A new peer connected.
    PeerConnected(PeerId),
    /// A peer disconnected.
    PeerDisconnected(PeerId),
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

    /// Dials a peer at the given multiaddr.
    pub fn dial(&self, addr: Multiaddr) -> Result<(), NetworkError> {
        self.command_tx
            .send(NetworkCommand::Dial(addr))
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

        let consensus_topic_hash = consensus.hash();
        let block_announce_topic_hash = block_announce.hash();

        let handle = NetworkHandle { command_tx };

        let service = Self {
            swarm,
            command_rx,
            event_tx,
            consensus_topic_hash,
            block_announce_topic_hash,
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
        } else {
            // Verification receipts or unknown topic
            let _ = self.event_tx.send(NetworkEvent::VerificationReceipt {
                source,
                data: message.data,
            });
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
            NetworkCommand::Dial(addr) => {
                if let Err(e) = self.swarm.dial(addr.clone()) {
                    tracing::warn!(%addr, error = %e, "failed to dial peer");
                }
            }
        }
    }
}

use futures::StreamExt;
