use crate::consensus_state::SharedConsensusState;
use alloy_primitives::{Address, B256};
use alloy_rpc_types_engine::{ForkchoiceState, PayloadAttributes};
use n42_consensus::{ConsensusEngine, ConsensusEvent, EngineOutput};
use n42_network::{NetworkEvent, NetworkHandle};
use reth_ethereum_engine_primitives::EthEngineTypes;
use reth_node_builder::ConsensusEngineHandle;
use reth_payload_builder::PayloadBuilderHandle;
use reth_payload_primitives::{EngineApiMessageVersion, PayloadKind};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

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

        // On startup, if this node is the leader for view 1, trigger first payload build.
        // This kicks off the genesis block production cycle.
        if self.engine.is_current_leader() && self.beacon_engine.is_some() {
            info!("this node is leader for view 1, triggering genesis payload build");
            self.trigger_payload_build().await;
        }

        loop {
            let timeout = self.engine.pacemaker().timeout_sleep();
            tokio::pin!(timeout);

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
                        Some(_) => {
                            // Block announcements and verification receipts are handled
                            // by dedicated subsystems, not the consensus orchestrator.
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
            }
        }
    }

    /// Triggers payload building by calling fork_choice_updated with PayloadAttributes.
    ///
    /// When the payload is ready, a spawned task sends the block hash to
    /// `block_ready_rx`, which feeds it to the consensus engine as BlockReady.
    async fn trigger_payload_build(&self) {
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

        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

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
                    // then sends the block hash through the channel.
                    let tx = self.block_ready_tx.clone();
                    tokio::spawn(async move {
                        // Give the builder time to pack transactions from the pool.
                        tokio::time::sleep(Duration::from_millis(500)).await;

                        match payload_builder
                            .resolve_kind(payload_id, PayloadKind::WaitForPending)
                            .await
                        {
                            Some(Ok(payload)) => {
                                let hash = payload.block().hash();
                                info!(%hash, "payload resolved, sending BlockReady");
                                let _ = tx.send(hash);
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
                debug!(%block_hash, "block execution requested (non-leader trusts leader's proposal)");
                // Non-leader validators trust the leader's block hash during consensus.
                // The block data will be synced via reth's devp2p after commit.
                // Mobile phones perform independent verification.
            }
            EngineOutput::BlockCommitted { view, block_hash, commit_qc } => {
                info!(view, %block_hash, "block committed by consensus");

                // 1. Update shared consensus state so PayloadBuilder uses this QC
                //    in the next block's extra_data.
                if let Some(ref state) = self.consensus_state {
                    state.update_committed_qc(commit_qc);
                }

                // 2. Update our head block hash.
                self.head_block_hash = block_hash;

                // 3. Notify reth Engine to finalize the block via fork_choice_updated.
                if let Some(ref engine_handle) = self.beacon_engine {
                    let fcu_state = ForkchoiceState {
                        head_block_hash: block_hash,
                        safe_block_hash: block_hash,
                        finalized_block_hash: block_hash,
                    };

                    match engine_handle
                        .fork_choice_updated(
                            fcu_state,
                            None,
                            EngineApiMessageVersion::default(),
                        )
                        .await
                    {
                        Ok(result) => {
                            debug!(
                                view,
                                %block_hash,
                                status = ?result.payload_status.status,
                                "fork choice updated for committed block"
                            );
                        }
                        Err(e) => {
                            error!(
                                view,
                                %block_hash,
                                error = %e,
                                "failed to update fork choice for committed block"
                            );
                        }
                    }
                }

                // 4. If this node is the leader for the next view, trigger
                //    the next payload build immediately. The engine has already
                //    advanced to the next view in try_form_commit_qc.
                if self.engine.is_current_leader() {
                    info!(
                        next_view = self.engine.current_view(),
                        "this node is leader for next view, triggering payload build"
                    );
                    self.trigger_payload_build().await;
                }
            }
            EngineOutput::ViewChanged { new_view } => {
                info!(new_view, "view changed");

                // After a view change (timeout recovery), if this node becomes
                // the new leader, it should trigger payload building.
                if self.engine.is_current_leader() {
                    info!(new_view, "became leader after view change, triggering payload build");
                    self.trigger_payload_build().await;
                }
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
