use alloy_primitives::Address;
use clap::Parser;
use n42_chainspec::ConsensusConfig;
use n42_consensus::{ConsensusEngine, ValidatorSet};
use n42_network::{build_swarm, StarHub, StarHubConfig, TransportConfig};
use n42_network::NetworkService;
use n42_node::mobile_bridge::MobileVerificationBridge;
use n42_node::rpc::{N42ApiServer, N42RpcServer};
use n42_node::{ConsensusOrchestrator, N42Node, SharedConsensusState};
use n42_primitives::BlsSecretKey;
use reth_ethereum_cli::chainspec::EthereumChainSpecParser;
use reth_ethereum_cli::Cli;
use reth_storage_api::BlockHashReader;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{info, warn};

fn main() {
    if std::env::var_os("RUST_BACKTRACE").is_none() {
        unsafe { std::env::set_var("RUST_BACKTRACE", "1") };
    }

    if let Err(err) = Cli::<EthereumChainSpecParser>::parse().run(async move |builder, _| {
        info!(target: "n42::cli", "Launching N42 node");

        // Determine validator count from environment (default: 1 for solo dev mode).
        let num_validators: usize = std::env::var("N42_VALIDATOR_COUNT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1);

        // Load consensus configuration.
        let consensus_config = if num_validators > 1 {
            info!(target: "n42::cli", count = num_validators, "multi-validator mode");
            ConsensusConfig::dev_multi(num_validators)
        } else {
            info!(target: "n42::cli", "single-validator dev mode");
            ConsensusConfig::dev()
        };

        // Load or generate validator identity.
        let secret_key = match std::env::var("N42_VALIDATOR_KEY") {
            Ok(hex_key) => {
                let bytes = hex::decode(&hex_key)
                    .expect("N42_VALIDATOR_KEY must be valid hex");
                let key_bytes: [u8; 32] = bytes.try_into()
                    .expect("N42_VALIDATOR_KEY must be exactly 32 bytes");
                BlsSecretKey::from_bytes(&key_bytes)
                    .expect("Invalid BLS secret key")
            }
            Err(_) => {
                warn!(target: "n42::cli", "No N42_VALIDATOR_KEY set, generating random key (dev mode)");
                BlsSecretKey::random().expect("Failed to generate random BLS key")
            }
        };

        // Build validator set from config.
        let validator_set = if consensus_config.initial_validators.is_empty() {
            // Single-validator dev mode: create a one-validator set from our own key.
            let pk = secret_key.public_key();
            let vi = n42_chainspec::ValidatorInfo {
                address: Address::ZERO,
                bls_public_key: pk,
            };
            ValidatorSet::new(&[vi], 0)
        } else {
            // Multi-validator mode: use the pre-configured validator set.
            ValidatorSet::new(
                &consensus_config.initial_validators,
                consensus_config.fault_tolerance,
            )
        };

        // Determine our validator index.
        let my_pubkey = secret_key.public_key();
        let my_index = validator_set
            .all_public_keys()
            .iter()
            .position(|pk| pk.to_bytes() == my_pubkey.to_bytes())
            .map(|i| i as u32)
            .unwrap_or(0);

        // Determine fee recipient address.
        let fee_recipient = if my_index < consensus_config.initial_validators.len() as u32 {
            consensus_config.initial_validators[my_index as usize].address
        } else {
            Address::ZERO
        };

        info!(
            target: "n42::cli",
            validator_index = my_index,
            validator_count = validator_set.len(),
            %fee_recipient,
            "validator identity resolved"
        );

        // Create shared consensus state.
        let consensus_state = Arc::new(SharedConsensusState::new(validator_set.clone()));
        let n42_node = N42Node::new(consensus_state.clone());

        let rpc_consensus_state = consensus_state.clone();
        let handle = builder
            .node(n42_node)
            .extend_rpc_modules(move |ctx| {
                let n42_rpc = N42RpcServer::new(rpc_consensus_state);
                ctx.modules.merge_configured(n42_rpc.into_rpc())?;
                info!(target: "n42::cli", "N42 RPC namespace registered");
                Ok(())
            })
            .on_node_started(move |full_node| {
                let task_executor = full_node.task_executor.clone();
                let beacon_engine_handle = full_node.add_ons_handle.beacon_engine_handle.clone();
                let payload_builder_handle = full_node.payload_builder_handle.clone();

                // Get the genesis (current head) block hash from the provider.
                let genesis_hash = full_node
                    .provider
                    .block_hash(0)
                    .ok()
                    .flatten()
                    .unwrap_or_default();

                info!(
                    target: "n42::cli",
                    validator_index = my_index,
                    validator_count = validator_set.len(),
                    %genesis_hash,
                    "starting N42 consensus subsystem"
                );

                // 1. Build libp2p swarm and start NetworkService.
                let keypair = libp2p::identity::Keypair::generate_ed25519();
                let transport_config = TransportConfig::default();
                let swarm = build_swarm(keypair, transport_config)
                    .expect("Failed to build libp2p swarm");

                let (mut net_service, net_handle, net_event_rx) =
                    NetworkService::new(swarm).expect("Failed to create NetworkService");

                let listen_addr: libp2p::Multiaddr =
                    "/ip4/0.0.0.0/udp/9400/quic-v1".parse().unwrap();
                if let Err(e) = net_service.listen_on(listen_addr.clone()) {
                    warn!(target: "n42::cli", error = %e, %listen_addr, "Failed to listen on consensus p2p address");
                } else {
                    info!(target: "n42::cli", %listen_addr, "Consensus P2P listening");
                }

                task_executor.spawn_critical_task(
                    "n42-p2p-network",
                    Box::pin(net_service.run()),
                );

                // 2. Start mobile verification StarHub.
                let star_hub_config = StarHubConfig::default();
                let (star_hub, _star_hub_handle, hub_event_rx) = StarHub::new(star_hub_config);

                info!(
                    target: "n42::cli",
                    bind_addr = %star_hub.bind_addr(),
                    "Starting mobile verification StarHub"
                );

                task_executor.spawn_critical_task(
                    "n42-starhub",
                    Box::pin(async move {
                        if let Err(e) = star_hub.run().await {
                            tracing::error!(error = %e, "StarHub exited with error");
                        }
                    }),
                );

                // 3. Start MobileVerificationBridge (receipt aggregation).
                let mobile_bridge = MobileVerificationBridge::new(
                    hub_event_rx,
                    10, // default threshold: 10 valid receipts
                    1000, // track up to 1000 blocks
                );

                task_executor.spawn_critical_task(
                    "n42-mobile-bridge",
                    Box::pin(mobile_bridge.run()),
                );

                // 4. Create ConsensusEngine.
                let (output_tx, output_rx) = mpsc::unbounded_channel();
                let consensus_engine = ConsensusEngine::new(
                    my_index,
                    secret_key,
                    validator_set,
                    consensus_config.base_timeout_ms,
                    consensus_config.max_timeout_ms,
                    output_tx,
                );

                // 5. Start ConsensusOrchestrator with Engine API bridge.
                //    Now passes genesis_hash and fee_recipient to enable
                //    payload building → BlockReady → consensus proposal flow.
                let orchestrator = ConsensusOrchestrator::with_engine_api(
                    consensus_engine,
                    net_handle,
                    net_event_rx,
                    output_rx,
                    beacon_engine_handle,
                    payload_builder_handle,
                    consensus_state,
                    genesis_hash,
                    fee_recipient,
                );

                task_executor.spawn_critical_task(
                    "n42-consensus-orchestrator",
                    Box::pin(orchestrator.run()),
                );

                info!(target: "n42::cli", "N42 consensus subsystem started");
                Ok(())
            })
            .launch()
            .await?;

        handle.wait_for_node_exit().await
    }) {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}
