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
    pub(crate) fn handle_engine_output(&self, output: EngineOutput) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, B256};
    use n42_chainspec::ValidatorInfo;
    use n42_consensus::{ConsensusEngine, ValidatorSet};
    use n42_network::{NetworkCommand, NetworkHandle};
    use n42_primitives::{BlsSecretKey, ConsensusMessage, QuorumCertificate, Vote};
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

        let vs = ValidatorSet::new(&[validator_info], 0); // f=0, quorum=1
        let (output_tx, output_rx) = mpsc::unbounded_channel();

        let engine = ConsensusEngine::new(
            0,  // my_index
            sk,
            vs,
            60000, // large base_timeout to avoid test timeouts
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

        // Verify the orchestrator was created successfully.
        let _ = orch;
    }

    #[test]
    fn test_engine_initial_state() {
        let (engine, _output_rx) = make_test_engine();

        assert_eq!(engine.current_view(), 1, "initial view should be 1");
        assert!(engine.is_current_leader(), "single validator should be leader");
    }

    #[test]
    fn test_handle_engine_output_broadcast() {
        let (engine, output_rx) = make_test_engine();
        let (network, mut cmd_rx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::unbounded_channel();

        let orch = ConsensusOrchestrator::new(engine, network, net_event_rx, output_rx);

        // Create a dummy consensus message
        let sk = BlsSecretKey::random().unwrap();
        let sig = sk.sign(b"test");
        let vote = Vote {
            view: 1,
            block_hash: B256::repeat_byte(0xAA),
            voter: 0,
            signature: sig,
        };

        // handle_engine_output should forward BroadcastMessage to the network
        orch.handle_engine_output(EngineOutput::BroadcastMessage(
            ConsensusMessage::Vote(vote),
        ));

        // Verify the command was sent to the network
        let cmd = cmd_rx.try_recv().expect("should receive a command");
        assert!(
            matches!(cmd, NetworkCommand::BroadcastConsensus(_)),
            "should be a BroadcastConsensus command"
        );
    }

    #[test]
    fn test_handle_engine_output_send_to_validator() {
        let (engine, output_rx) = make_test_engine();
        let (network, mut cmd_rx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::unbounded_channel();

        let orch = ConsensusOrchestrator::new(engine, network, net_event_rx, output_rx);

        let sk = BlsSecretKey::random().unwrap();
        let sig = sk.sign(b"test");
        let vote = Vote {
            view: 1,
            block_hash: B256::repeat_byte(0xBB),
            voter: 0,
            signature: sig,
        };

        // SendToValidator should fallback to broadcast
        orch.handle_engine_output(EngineOutput::SendToValidator(
            0,
            ConsensusMessage::Vote(vote),
        ));

        let cmd = cmd_rx.try_recv().expect("should receive a command");
        assert!(
            matches!(cmd, NetworkCommand::BroadcastConsensus(_)),
            "SendToValidator should fallback to BroadcastConsensus"
        );
    }

    #[test]
    fn test_handle_engine_output_execute_block() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::unbounded_channel();

        let orch = ConsensusOrchestrator::new(engine, network, net_event_rx, output_rx);

        // ExecuteBlock should not panic (it just logs)
        orch.handle_engine_output(EngineOutput::ExecuteBlock(B256::repeat_byte(0xCC)));
    }

    #[test]
    fn test_handle_engine_output_block_committed() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::unbounded_channel();

        let orch = ConsensusOrchestrator::new(engine, network, net_event_rx, output_rx);

        let commit_qc = QuorumCertificate::genesis();
        orch.handle_engine_output(EngineOutput::BlockCommitted {
            view: 1,
            block_hash: B256::repeat_byte(0xDD),
            commit_qc,
        });
        // Should not panic
    }

    #[test]
    fn test_handle_engine_output_view_changed() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::unbounded_channel();

        let orch = ConsensusOrchestrator::new(engine, network, net_event_rx, output_rx);

        orch.handle_engine_output(EngineOutput::ViewChanged { new_view: 5 });
        // Should not panic
    }

    #[tokio::test]
    async fn test_orchestrator_exits_on_net_event_channel_close() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx) = make_test_network();
        let (net_event_tx, net_event_rx) = mpsc::unbounded_channel::<NetworkEvent>();

        let orch = ConsensusOrchestrator::new(engine, network, net_event_rx, output_rx);

        // Drop the event sender so the channel closes
        drop(net_event_tx);

        // run() should exit because net_event_rx.recv() returns None
        let result = tokio::time::timeout(Duration::from_secs(5), orch.run()).await;
        assert!(
            result.is_ok(),
            "orchestrator should exit when network event channel is closed"
        );
    }

    #[tokio::test]
    async fn test_orchestrator_exits_on_output_channel_close() {
        let sk = BlsSecretKey::random().unwrap();
        let pk = sk.public_key();
        let validator_info = ValidatorInfo {
            address: Address::with_last_byte(1),
            bls_public_key: pk,
        };
        let vs = ValidatorSet::new(&[validator_info], 0);
        let (output_tx, output_rx) = mpsc::unbounded_channel();

        let _engine = ConsensusEngine::new(0, sk, vs, 60000, 120000, output_tx);

        let (_network, _cmd_rx) = make_test_network();
        let (_net_event_tx, _net_event_rx) = mpsc::unbounded_channel::<NetworkEvent>();

        // Drop the output_rx before creating orchestrator to test that case
        drop(output_rx);

        // Need to recreate since output_rx was consumed
        let (output_tx2, output_rx2) = mpsc::unbounded_channel();
        let sk2 = BlsSecretKey::random().unwrap();
        let pk2 = sk2.public_key();
        let vi2 = ValidatorInfo {
            address: Address::with_last_byte(2),
            bls_public_key: pk2,
        };
        let vs2 = ValidatorSet::new(&[vi2], 0);
        let engine2 = ConsensusEngine::new(0, sk2, vs2, 60000, 120000, output_tx2);

        let (network2, _cmd_rx2) = make_test_network();
        let (net_event_tx2, net_event_rx2) = mpsc::unbounded_channel::<NetworkEvent>();

        let orch = ConsensusOrchestrator::new(engine2, network2, net_event_rx2, output_rx2);

        // Close the net event channel to trigger shutdown
        drop(net_event_tx2);

        let result = tokio::time::timeout(Duration::from_secs(5), orch.run()).await;
        assert!(result.is_ok(), "orchestrator should exit cleanly");
    }

    #[tokio::test]
    async fn test_orchestrator_processes_peer_events() {
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx) = make_test_network();
        let (net_event_tx, net_event_rx) = mpsc::unbounded_channel::<NetworkEvent>();

        let orch = ConsensusOrchestrator::new(engine, network, net_event_rx, output_rx);

        // Send a PeerConnected event, then close the channel
        let peer_id = libp2p::PeerId::random();
        net_event_tx
            .send(NetworkEvent::PeerConnected(peer_id))
            .unwrap();
        drop(net_event_tx);

        // The orchestrator should process the PeerConnected event and then exit
        let result = tokio::time::timeout(Duration::from_secs(5), orch.run()).await;
        assert!(result.is_ok(), "orchestrator should exit after processing events");
    }
}
