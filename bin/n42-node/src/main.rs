mod keystore;

use alloy_primitives::Address;
use clap::Parser;
use n42_chainspec::ConsensusConfig;
use n42_consensus::{ConsensusEngine, EpochManager, ValidatorSet};
use n42_network::{build_swarm_with_validator_index, ShardedStarHub, ShardedStarHubConfig, TransportConfig};
use n42_network::NetworkService;
use n42_node::mobile_bridge::MobileVerificationBridge;
use n42_node::mobile_packet::mobile_packet_loop;
use n42_node::persistence;
use n42_node::rpc::{N42ApiServer, N42RpcServer};
use n42_node::tx_bridge::TxPoolBridge;
use n42_node::{ConsensusOrchestrator, N42Node, SharedConsensusState};
use n42_primitives::BlsSecretKey;
use reth_chainspec::ChainSpecProvider;
use reth_ethereum_cli::chainspec::EthereumChainSpecParser;
use reth_ethereum_cli::Cli;
use reth_node_core::args::DefaultRpcServerArgs;
use reth_storage_api::{BlockHashReader, BlockNumReader};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{info, warn};

fn env_bool(name: &str) -> bool {
    std::env::var(name)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name).ok().and_then(|s| s.parse().ok())
}

fn env_u16(name: &str) -> Option<u16> {
    std::env::var(name).ok().and_then(|s| s.parse().ok())
}

fn env_usize(name: &str) -> Option<usize> {
    std::env::var(name).ok().and_then(|s| s.parse().ok())
}

/// Derives a deterministic Ed25519 keypair from a validator index.
fn derive_ed25519_keypair(index: u32) -> libp2p::identity::Keypair {
    let seed = alloy_primitives::keccak256(format!("n42-p2p-key-{}", index).as_bytes());
    let mut seed_bytes: [u8; 32] = seed.0;
    let secret = libp2p::identity::ed25519::SecretKey::try_from_bytes(&mut seed_bytes)
        .expect("valid ed25519 seed from keccak256");
    libp2p::identity::Keypair::from(libp2p::identity::ed25519::Keypair::from(secret))
}

fn main() {
    if std::env::var_os("RUST_BACKTRACE").is_none() {
        unsafe { std::env::set_var("RUST_BACKTRACE", "1") };
    }

    // Enable HTTP RPC and permissive CORS by default.
    //
    // NOTE: Do NOT call with_http_api()/with_ws_api() here — reth's
    // RpcModuleSelection::Display wraps output in brackets that its own
    // FromStr parser rejects. Use reth's built-in defaults (eth,net,web3).
    // WS is not enabled by default to avoid port conflicts in multi-node setups.
    let _ = DefaultRpcServerArgs::default()
        .with_http(true)
        .with_http_corsdomain(Some("*".to_string()))
        .try_init();

    if let Err(err) = Cli::<EthereumChainSpecParser>::parse().run(async move |builder, _| {
        info!(target: "n42::cli", "Launching N42 node");

        // Load consensus configuration.
        // Priority: N42_CONSENSUS_CONFIG file > N42_VALIDATOR_COUNT dev mode.
        let mut consensus_config = match std::env::var("N42_CONSENSUS_CONFIG") {
            Ok(path) => {
                info!(target: "n42::cli", path, "loading consensus config from file");
                ConsensusConfig::from_file(std::path::Path::new(&path))
                    .unwrap_or_else(|e| panic!("Failed to load consensus config: {e}"))
            }
            Err(_) => {
                let num_validators = env_usize("N42_VALIDATOR_COUNT").unwrap_or(1);
                if num_validators > 1 {
                    info!(target: "n42::cli", count = num_validators, "multi-validator dev mode");
                    ConsensusConfig::dev_multi(num_validators)
                } else {
                    info!(target: "n42::cli", "single-validator dev mode");
                    ConsensusConfig::dev()
                }
            }
        };

        // Override pacemaker timeouts from environment if set.
        if let Some(ms) = env_u64("N42_BASE_TIMEOUT_MS") {
            info!(target: "n42::cli", base_timeout_ms = ms, "overriding base timeout from env");
            consensus_config.base_timeout_ms = ms;
        } else if std::env::var("N42_BASE_TIMEOUT_MS").is_ok() {
            warn!(target: "n42::cli", "N42_BASE_TIMEOUT_MS is not a valid u64, ignoring");
        }
        if let Some(ms) = env_u64("N42_MAX_TIMEOUT_MS") {
            info!(target: "n42::cli", max_timeout_ms = ms, "overriding max timeout from env");
            consensus_config.max_timeout_ms = ms;
        } else if std::env::var("N42_MAX_TIMEOUT_MS").is_ok() {
            warn!(target: "n42::cli", "N42_MAX_TIMEOUT_MS is not a valid u64, ignoring");
        }

        // Load or generate validator identity.
        // Priority: N42_KEYSTORE_PATH (encrypted) > N42_VALIDATOR_KEY (plaintext) > random (dev).
        let secret_key = if let Ok(ks_path) = std::env::var("N42_KEYSTORE_PATH") {
            let password = std::env::var("N42_KEYSTORE_PASSWORD")
                .expect("N42_KEYSTORE_PASSWORD required when using N42_KEYSTORE_PATH");
            info!(target: "n42::cli", path = ks_path, "loading validator key from encrypted keystore");
            let ks = keystore::Keystore::load(std::path::Path::new(&ks_path))
                .unwrap_or_else(|e| panic!("Failed to load keystore: {e}"));
            let key_bytes = ks.decrypt(&password)
                .unwrap_or_else(|e| panic!("Failed to decrypt keystore: {e}"));
            BlsSecretKey::from_bytes(&key_bytes).expect("Invalid BLS secret key in keystore")
        } else if let Ok(hex_key) = std::env::var("N42_VALIDATOR_KEY") {
            warn!(target: "n42::cli", "using plaintext N42_VALIDATOR_KEY — use N42_KEYSTORE_PATH for production");
            let bytes = hex::decode(&hex_key).expect("N42_VALIDATOR_KEY must be valid hex");
            let key_bytes: [u8; 32] = bytes.try_into().expect("N42_VALIDATOR_KEY must be exactly 32 bytes");
            BlsSecretKey::from_bytes(&key_bytes).expect("Invalid BLS secret key")
        } else {
            warn!(target: "n42::cli", "No validator key configured, generating random key (dev mode)");
            BlsSecretKey::random().expect("Failed to generate random BLS key")
        };

        // Build validator set.
        let validator_set = if consensus_config.initial_validators.is_empty() {
            ValidatorSet::new(
                &[n42_chainspec::ValidatorInfo {
                    address: Address::ZERO,
                    bls_public_key: secret_key.public_key(),
                }],
                0,
            )
        } else {
            ValidatorSet::new(
                &consensus_config.initial_validators,
                consensus_config.fault_tolerance,
            )
        };

        let my_pubkey = secret_key.public_key();
        let my_index = validator_set
            .all_public_keys()
            .iter()
            .position(|pk| pk.to_bytes() == my_pubkey.to_bytes())
            .map(|i| i as u32)
            .unwrap_or(0);

        let fee_recipient = consensus_config
            .initial_validators
            .get(my_index as usize)
            .map(|v| v.address)
            .unwrap_or(Address::ZERO);

        info!(
            target: "n42::cli",
            validator_index = my_index,
            validator_count = validator_set.len(),
            %fee_recipient,
            "validator identity resolved"
        );

        let consensus_state = Arc::new(SharedConsensusState::new(validator_set.clone()));
        let n42_node = N42Node::new(consensus_state.clone());

        let rpc_consensus_state = consensus_state.clone();
        let handle = builder
            .node(n42_node)
            .extend_rpc_modules(move |ctx| {
                ctx.modules.merge_configured(N42RpcServer::new(rpc_consensus_state).into_rpc())?;
                info!(target: "n42::cli", "N42 RPC namespace registered");
                Ok(())
            })
            .on_node_started(move |full_node| {
                let task_executor = full_node.task_executor.clone();
                let beacon_engine_handle = full_node.add_ons_handle.beacon_engine_handle.clone();
                let payload_builder_handle = full_node.payload_builder_handle.clone();

                // Determine canonical chain head for fork_choice_updated.
                // On first start this is genesis; after restart it's the latest committed block.
                let genesis_hash = full_node
                    .provider
                    .block_hash(0)
                    .ok()
                    .flatten()
                    .unwrap_or_default();

                let head_block_hash = {
                    let best_num = full_node.provider.best_block_number().unwrap_or(0);
                    if best_num > 0 {
                        let hash = full_node
                            .provider
                            .block_hash(best_num)
                            .ok()
                            .flatten()
                            .unwrap_or(genesis_hash);
                        info!(
                            target: "n42::cli",
                            best_block = best_num,
                            %hash,
                            "using canonical chain head as head_block_hash"
                        );
                        hash
                    } else {
                        genesis_hash
                    }
                };

                info!(
                    target: "n42::cli",
                    validator_index = my_index,
                    validator_count = validator_set.len(),
                    %genesis_hash,
                    %head_block_hash,
                    "starting N42 consensus subsystem"
                );

                // Build libp2p swarm and start NetworkService.
                let keypair = derive_ed25519_keypair(my_index);
                let local_peer_id = keypair.public().to_peer_id();
                info!(target: "n42::cli", %local_peer_id, "P2P identity (deterministic)");

                let mut transport_config =
                    TransportConfig::for_network_size(validator_set.len() as usize);
                transport_config.enable_mdns = env_bool("N42_ENABLE_MDNS");
                transport_config.enable_kademlia = env_bool("N42_ENABLE_DHT");
                if transport_config.enable_mdns {
                    info!(target: "n42::cli", "mDNS peer discovery enabled");
                }
                if transport_config.enable_kademlia {
                    info!(target: "n42::cli", "Kademlia DHT peer discovery enabled");
                }

                let swarm =
                    build_swarm_with_validator_index(keypair, transport_config, Some(my_index))
                        .expect("Failed to build libp2p swarm");

                let (mut net_service, net_handle, net_event_rx) =
                    NetworkService::new(swarm).expect("Failed to create NetworkService");

                let consensus_port = env_u16("N42_CONSENSUS_PORT").unwrap_or(9400);
                let listen_addr: libp2p::Multiaddr =
                    format!("/ip4/0.0.0.0/udp/{}/quic-v1", consensus_port).parse().unwrap();

                if let Err(e) = net_service.listen_on(listen_addr.clone()) {
                    warn!(target: "n42::cli", error = %e, %listen_addr, "Failed to listen on consensus p2p address");
                } else {
                    info!(target: "n42::cli", %listen_addr, "Consensus P2P listening");
                }

                task_executor.spawn_critical_task(
                    "n42-p2p-network",
                    Box::pin(net_service.run()),
                );

                // Dial trusted peers from N42_TRUSTED_PEERS (comma-separated multiaddrs).
                let trusted_peers_str = std::env::var("N42_TRUSTED_PEERS").unwrap_or_default();
                for peer_addr_str in trusted_peers_str.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                    match peer_addr_str.parse::<libp2p::Multiaddr>() {
                        Ok(addr) => {
                            let maybe_peer_id = addr.iter().find_map(|proto| {
                                if let libp2p::multiaddr::Protocol::P2p(peer_id) = proto {
                                    Some(peer_id)
                                } else {
                                    None
                                }
                            });

                            if let Err(e) = net_handle.dial(addr.clone()) {
                                warn!(target: "n42::cli", error = %e, %addr, "failed to dial trusted peer");
                            } else {
                                info!(target: "n42::cli", %addr, "dialing trusted peer");
                            }

                            if let Some(peer_id) = maybe_peer_id {
                                if let Err(e) = net_handle.register_peer(peer_id, vec![addr.clone()], true) {
                                    warn!(target: "n42::cli", error = %e, "failed to register trusted peer for reconnection");
                                }
                                if let Err(e) = net_handle.add_kademlia_peer(peer_id, vec![addr]) {
                                    warn!(target: "n42::cli", error = %e, "failed to add trusted peer to kademlia");
                                }
                            }
                        }
                        Err(e) => {
                            warn!(target: "n42::cli", error = %e, addr = peer_addr_str, "invalid trusted peer multiaddr");
                        }
                    }
                }

                // Auto-connect to higher-index validators only (i < j initiates the connection),
                // avoiding duplicate connections. Port convention: base_port + validator_index.
                let val_count = validator_set.len() as u32;
                if !env_bool("N42_NO_AUTO_CONNECT") && val_count > 1 {
                    let base_port = consensus_port.saturating_sub(my_index as u16);
                    info!(
                        target: "n42::cli",
                        base_port, val_count, my_index,
                        "auto-connecting to higher-index validators"
                    );
                    for j in (my_index + 1)..val_count {
                        let peer_keypair = derive_ed25519_keypair(j);
                        let peer_id = peer_keypair.public().to_peer_id();
                        let peer_port = base_port + j as u16;
                        let peer_addr: libp2p::Multiaddr = format!(
                            "/ip4/127.0.0.1/udp/{}/quic-v1/p2p/{}",
                            peer_port, peer_id
                        )
                        .parse()
                        .expect("valid multiaddr");

                        if let Err(e) = net_handle.dial(peer_addr.clone()) {
                            warn!(target: "n42::cli", error = %e, validator = j, "failed to auto-dial validator");
                        } else {
                            info!(target: "n42::cli", validator = j, %peer_id, port = peer_port, "auto-dialing validator");
                        }
                        if let Err(e) = net_handle.register_peer(peer_id, vec![peer_addr], true) {
                            warn!(target: "n42::cli", error = %e, validator = j, "failed to register for reconnection");
                        }
                    }
                }

                let data_dir: PathBuf = std::env::var("N42_DATA_DIR")
                    .unwrap_or_else(|_| "./n42-data".to_string())
                    .into();

                // Start mobile verification StarHub (sharded for 50K+ connections).
                let starhub_port = env_u16("N42_STARHUB_PORT").unwrap_or(9443);
                let shard_count = env_usize("N42_STARHUB_SHARDS").unwrap_or(1);

                let (sharded_hub, star_hub_handle, hub_event_rx) =
                    ShardedStarHub::new(ShardedStarHubConfig {
                        base_port: starhub_port,
                        shard_count,
                        max_connections_per_shard: 10_000,
                        cert_dir: Some(data_dir.join("certs")),
                        ..Default::default()
                    });

                info!(
                    target: "n42::cli",
                    base_port = starhub_port,
                    shard_count,
                    ports = ?sharded_hub.ports(),
                    "Starting mobile verification StarHub (sharded)"
                );

                task_executor.spawn_critical_task(
                    "n42-starhub",
                    Box::pin(async move {
                        if let Err(e) = sharded_hub.run().await {
                            tracing::error!(error = %e, "ShardedStarHub exited with error");
                        }
                    }),
                );

                // Start MobileVerificationBridge (receipt aggregation).
                let (attest_tx, mut attest_rx) = mpsc::channel(256);
                let (phone_connected_tx, phone_connected_rx) = mpsc::channel(128);

                let mobile_bridge = MobileVerificationBridge::new(hub_event_rx, 10, 1000)
                    .with_attestation_tx(attest_tx)
                    .with_phone_connected_tx(phone_connected_tx);

                task_executor.spawn_critical_task("n42-mobile-bridge", Box::pin(mobile_bridge.run()));

                // Log attestation events and record them in shared state.
                let attestation_state = consensus_state.clone();
                task_executor.spawn_critical_task(
                    "n42-attestation-logger",
                    Box::pin(async move {
                        while let Some(event) = attest_rx.recv().await {
                            info!(
                                target: "n42::mobile",
                                block_number = event.block_number,
                                %event.block_hash,
                                valid_count = event.valid_count,
                                "block reached mobile attestation threshold"
                            );
                            attestation_state.record_attestation(
                                event.block_hash,
                                event.block_number,
                                event.valid_count,
                            );
                        }
                    }),
                );

                // Start mobile packet generation loop (V2 stream format).
                let (mobile_packet_tx, mobile_packet_rx) = mpsc::channel(128);
                task_executor.spawn_critical_task(
                    "n42-mobile-packet",
                    Box::pin(mobile_packet_loop(
                        mobile_packet_rx,
                        full_node.provider.clone(),
                        full_node.provider.chain_spec(),
                        star_hub_handle.clone(),
                        phone_connected_rx,
                    )),
                );
                info!(target: "n42::cli", "Mobile packet generation loop started");

                // Create ConsensusEngine with optional state recovery.
                let state_file = data_dir.join("consensus_state.json");

                let make_epoch_manager = || -> EpochManager {
                    if consensus_config.epoch_length > 0 {
                        EpochManager::with_epoch_length(
                            validator_set.clone(),
                            consensus_config.epoch_length,
                        )
                    } else {
                        EpochManager::new(validator_set.clone())
                    }
                };

                let (output_tx, output_rx) = mpsc::channel(1024);
                let snapshot = match persistence::load_consensus_state(&state_file) {
                    Ok(snap) => snap,
                    Err(e) => {
                        warn!(target: "n42::cli", error = %e, "failed to load consensus snapshot, starting fresh");
                        None
                    }
                };

                let consensus_engine = if let Some(snapshot) = snapshot {
                    info!(
                        target: "n42::cli",
                        view = snapshot.current_view,
                        locked_qc_view = snapshot.locked_qc.view,
                        last_committed_view = snapshot.last_committed_qc.view,
                        "recovered consensus state from snapshot"
                    );
                    let mut epoch_manager = make_epoch_manager();
                    if let Some((_, ref validators, f)) = snapshot.scheduled_epoch_transition {
                        info!(
                            target: "n42::cli",
                            new_validators = validators.len(),
                            "restoring staged epoch transition from snapshot"
                        );
                        epoch_manager.stage_next_epoch(validators, f);
                    }
                    ConsensusEngine::with_recovered_state(
                        my_index,
                        secret_key,
                        epoch_manager,
                        consensus_config.base_timeout_ms,
                        consensus_config.max_timeout_ms,
                        output_tx,
                        snapshot.current_view,
                        snapshot.locked_qc,
                        snapshot.last_committed_qc,
                        snapshot.consecutive_timeouts,
                    )
                } else {
                    info!(target: "n42::cli", "no consensus snapshot found, starting fresh");
                    ConsensusEngine::with_epoch_manager(
                        my_index,
                        secret_key,
                        make_epoch_manager(),
                        consensus_config.base_timeout_ms,
                        consensus_config.max_timeout_ms,
                        output_tx,
                    )
                };

                // Start TxPoolBridge for P2P mempool sync.
                let (tx_import_tx, tx_import_rx) = mpsc::channel::<Vec<u8>>(4096);
                let (tx_broadcast_tx, tx_broadcast_rx) = mpsc::channel::<Vec<u8>>(4096);

                task_executor.spawn_critical_task(
                    "n42-tx-pool-bridge",
                    Box::pin(
                        TxPoolBridge::new(full_node.pool.clone(), tx_import_rx, tx_broadcast_tx)
                            .run(),
                    ),
                );
                info!(target: "n42::cli", "TxPoolBridge started for P2P mempool sync");

                // Start ConsensusOrchestrator with Engine API bridge.
                let orchestrator = ConsensusOrchestrator::with_engine_api(
                    consensus_engine,
                    net_handle,
                    net_event_rx,
                    output_rx,
                    beacon_engine_handle,
                    payload_builder_handle,
                    consensus_state,
                    head_block_hash,
                    fee_recipient,
                )
                .with_tx_pool_bridge(tx_import_tx, tx_broadcast_rx)
                .with_mobile_packet_tx(mobile_packet_tx)
                .with_state_persistence(state_file)
                .with_validator_set(validator_set)
                .with_blob_store(full_node.pool.blob_store().clone());

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
