use n42_consensus::{ConsensusEngine, ConsensusEvent, EngineOutput};
use n42_network::{NetworkEvent, NetworkHandle};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

/// Bridges the consensus engine with the P2P network layer.
///
/// The orchestrator runs as a background task (`spawn_critical`) and drives
/// three concurrent event sources via `tokio::select!`:
///
/// 1. **Network events** → translated to `ConsensusEvent::Message` → fed into the engine
/// 2. **Engine outputs** → translated to network commands (broadcast, etc.)
/// 3. **Pacemaker timeout** → triggers `engine.on_timeout()` for view change recovery
///
/// ## Note on SendToValidator
///
/// GossipSub does not support point-to-point messaging. All `SendToValidator`
/// outputs are downgraded to broadcasts. The receiving end filters by
/// view number and leader index.
pub struct ConsensusOrchestrator {
    /// The HotStuff-2 consensus engine (event-driven state machine).
    engine: ConsensusEngine,
    /// Handle for sending messages to the P2P network.
    network: NetworkHandle,
    /// Receiver for events from the network service.
    net_event_rx: mpsc::UnboundedReceiver<NetworkEvent>,
    /// Receiver for outputs from the consensus engine.
    output_rx: mpsc::UnboundedReceiver<EngineOutput>,
}

impl ConsensusOrchestrator {
    /// Creates a new orchestrator.
    ///
    /// # Arguments
    ///
    /// * `engine` - The consensus engine instance
    /// * `network` - Network handle for broadcasting messages
    /// * `net_event_rx` - Channel receiving events from NetworkService
    /// * `output_rx` - Channel receiving outputs from ConsensusEngine
    pub fn new(
        engine: ConsensusEngine,
        network: NetworkHandle,
        net_event_rx: mpsc::UnboundedReceiver<NetworkEvent>,
        output_rx: mpsc::UnboundedReceiver<EngineOutput>,
    ) -> Self {
        Self {
            engine,
            network,
            net_event_rx,
            output_rx,
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
            "consensus orchestrator started"
        );

        loop {
            // Create the timeout future before entering select! to avoid
            // borrow conflicts with &mut self.engine in other branches.
            let timeout = self.engine.pacemaker().timeout_sleep();
            tokio::pin!(timeout);

            tokio::select! {
                // Branch 1: Pacemaker timeout → trigger view change
                _ = &mut timeout => {
                    let view = self.engine.current_view();
                    warn!(view, "pacemaker timeout, initiating view change");
                    if let Err(e) = self.engine.on_timeout() {
                        error!(view, error = %e, "error handling timeout");
                    }
                }

                // Branch 2: Network event → feed to consensus engine
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
                        Some(_) => {
                            // Block announcements and other events are handled
                            // by the node layer, not the consensus orchestrator.
                        }
                        None => {
                            info!("network event channel closed, shutting down orchestrator");
                            break;
                        }
                    }
                }

                // Branch 3: Engine output → dispatch to network / node
                output = self.output_rx.recv() => {
                    match output {
                        Some(engine_output) => {
                            self.handle_engine_output(engine_output);
                        }
                        None => {
                            info!("engine output channel closed, shutting down orchestrator");
                            break;
                        }
                    }
                }
            }
        }
    }

    /// Dispatches an engine output to the appropriate destination.
    fn handle_engine_output(&self, output: EngineOutput) {
        match output {
            EngineOutput::BroadcastMessage(msg) => {
                if let Err(e) = self.network.broadcast_consensus(msg) {
                    error!(error = %e, "failed to broadcast consensus message");
                }
            }
            EngineOutput::SendToValidator(_target, msg) => {
                // GossipSub doesn't support point-to-point; broadcast instead.
                // Recipients filter by view/leader index.
                if let Err(e) = self.network.broadcast_consensus(msg) {
                    error!(error = %e, "failed to send message to validator (broadcast fallback)");
                }
            }
            EngineOutput::ExecuteBlock(block_hash) => {
                // Block execution is handled by the reth execution pipeline.
                // The consensus engine requests execution; the node layer
                // processes this asynchronously and calls engine.process_event(BlockReady)
                // when execution completes.
                info!(%block_hash, "block execution requested by consensus engine");
            }
            EngineOutput::BlockCommitted { view, block_hash, commit_qc: _ } => {
                info!(
                    view,
                    %block_hash,
                    "block committed by consensus"
                );
                // The committed block will be finalized by the reth execution pipeline.
                // Mobile verification packets are generated and pushed asynchronously.
            }
            EngineOutput::ViewChanged { new_view } => {
                info!(new_view, "view changed");
            }
        }
    }
}
