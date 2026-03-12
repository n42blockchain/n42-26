mod keystore;

#[cfg(feature = "jemalloc")]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

use alloy_primitives::Address;
use clap::Parser;
use n42_chainspec::ConsensusConfig;
use n42_consensus::{ConsensusEngine, EpochManager, ValidatorSet};
use n42_network::{build_swarm_with_validator_index, ShardedStarHub, ShardedStarHubConfig, TransportConfig};
use n42_network::NetworkService;
use n42_node::mobile_bridge::MobileVerificationBridge;
use n42_node::mobile_packet::mobile_packet_loop;
use n42_node::attestation_store::AttestationStore;
use n42_node::mobile_reward::MobileRewardManager;
use n42_node::epoch_schedule::EpochSchedule;
use n42_node::persistence;
use n42_node::rpc::{N42ApiServer, N42RpcServer};
use n42_node::staking::StakingManager;
use n42_node::tx_bridge::TxPoolBridge;
use n42_node::{ConsensusOrchestrator, ObserverOrchestrator, N42Node, SharedConsensusState};
use n42_primitives::BlsSecretKey;
use reth_chainspec::ChainSpecProvider;
use reth_ethereum_cli::chainspec::EthereumChainSpecParser;
use reth_ethereum_cli::Cli;
use reth_node_core::args::DefaultRpcServerArgs;
use reth_storage_api::{BlockHashReader, BlockNumReader};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tracing::{info, warn};

fn env_bool(name: &str) -> bool {
    std::env::var(name)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

fn env_parse<T: std::str::FromStr>(name: &str) -> Option<T> {
    std::env::var(name).ok().and_then(|s| s.parse().ok())
}

fn override_timeout_from_env(name: &str, target: &mut u64) {
    if let Some(ms) = env_parse::<u64>(name) {
        info!(target: "n42::cli", env = name, value = ms, "overriding timeout from env");
        *target = ms;
    } else if std::env::var(name).is_ok() {
        warn!(target: "n42::cli", env = name, "env var is not a valid u64, ignoring");
    }
}

/// Connects to trusted peers specified in the `N42_TRUSTED_PEERS` environment variable.
///
/// The variable should contain a comma-separated list of libp2p multiaddrs.
/// Each peer is dialed and registered for automatic reconnection + Kademlia.
fn connect_trusted_peers(net_handle: &n42_network::NetworkHandle) {
    let trusted_peers_str = std::env::var("N42_TRUSTED_PEERS").unwrap_or_default();
    for peer_addr_str in trusted_peers_str.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        match peer_addr_str.parse::<libp2p::Multiaddr>() {
            Ok(addr) => {
                let maybe_peer_id = addr.iter().find_map(|proto| match proto {
                    libp2p::multiaddr::Protocol::P2p(peer_id) => Some(peer_id),
                    _ => None,
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
}

fn derive_ed25519_keypair(index: u32) -> libp2p::identity::Keypair {
    let seed = alloy_primitives::keccak256(format!("n42-p2p-key-{}", index).as_bytes());
    let mut seed_bytes: [u8; 32] = seed.0;
    let secret = libp2p::identity::ed25519::SecretKey::try_from_bytes(&mut seed_bytes)
        .expect("valid ed25519 seed from keccak256");
    libp2p::identity::Keypair::from(libp2p::identity::ed25519::Keypair::from(secret))
}

fn main() {
    // Install a global panic hook so that panics in ANY thread or tokio task
    // produce visible output on stderr.  Without this, panics in non-critical
    // `tokio::spawn` tasks silently disappear (the JoinHandle is dropped and
    // nobody observes the error).  This is the primary defence against the
    // "node crashed with no logs" failure mode.
    std::panic::set_hook(Box::new(|info| {
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("<unnamed>");
        // Capture backtrace regardless of RUST_BACKTRACE setting.
        let bt = std::backtrace::Backtrace::force_capture();
        eprintln!(
            "\n!!! PANIC in thread '{name}' !!!\n{info}\n\nBacktrace:\n{bt}\n"
        );
    }));

    if std::env::var_os("RUST_BACKTRACE").is_none() {
        // SAFETY: Called from main() before any threads are spawned.
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

        // Warn about benchmark/debug env vars that weaken security.
        for (var, desc) in [
            ("N42_SKIP_TX_VERIFY", "signature verification bypassed"),
            ("N42_SKIP_STATE_ROOT", "state root computation skipped"),
            ("N42_DEFER_STATE_ROOT", "state root computation deferred"),
        ] {
            if std::env::var(var).map_or(false, |v| v == "1") {
                warn!(target: "n42::cli", var, desc, "BENCHMARK MODE: {var}=1 — NOT FOR PRODUCTION");
            }
        }

        // Priority: N42_CONSENSUS_CONFIG file > N42_VALIDATOR_COUNT dev mode.
        let mut consensus_config = match std::env::var("N42_CONSENSUS_CONFIG") {
            Ok(path) => {
                info!(target: "n42::cli", path, "loading consensus config from file");
                ConsensusConfig::from_file(std::path::Path::new(&path))
                    .unwrap_or_else(|e| {
                        eprintln!("ERROR: Failed to load consensus config from {path}: {e}");
                        std::process::exit(1);
                    })
            }
            Err(_) => {
                let num_validators = env_parse("N42_VALIDATOR_COUNT").unwrap_or(1);
                if num_validators > 1 {
                    info!(target: "n42::cli", count = num_validators, "multi-validator dev mode");
                    ConsensusConfig::dev_multi(num_validators)
                } else {
                    info!(target: "n42::cli", "single-validator dev mode");
                    ConsensusConfig::dev()
                }
            }
        };

        override_timeout_from_env("N42_BASE_TIMEOUT_MS", &mut consensus_config.base_timeout_ms);
        override_timeout_from_env("N42_MAX_TIMEOUT_MS", &mut consensus_config.max_timeout_ms);

        if consensus_config.base_timeout_ms == 0 {
            eprintln!("ERROR: N42_BASE_TIMEOUT_MS must be > 0");
            std::process::exit(1);
        }
        if consensus_config.max_timeout_ms == 0 {
            eprintln!("ERROR: N42_MAX_TIMEOUT_MS must be > 0");
            std::process::exit(1);
        }
        if consensus_config.base_timeout_ms > consensus_config.max_timeout_ms {
            eprintln!(
                "ERROR: N42_BASE_TIMEOUT_MS ({}) must be <= N42_MAX_TIMEOUT_MS ({})",
                consensus_config.base_timeout_ms, consensus_config.max_timeout_ms
            );
            std::process::exit(1);
        }

        if let Err(e) = consensus_config.validate() {
            eprintln!("ERROR: consensus config validation failed: {e}");
            std::process::exit(1);
        }

        // Priority: N42_KEYSTORE_PATH (encrypted) > N42_VALIDATOR_KEY (plaintext) > random (dev).
        let secret_key = if let Ok(ks_path) = std::env::var("N42_KEYSTORE_PATH") {
            let password = std::env::var("N42_KEYSTORE_PASSWORD").unwrap_or_else(|_| {
                eprintln!("ERROR: N42_KEYSTORE_PASSWORD required when using N42_KEYSTORE_PATH");
                std::process::exit(1);
            });
            info!(target: "n42::cli", path = ks_path, "loading validator key from encrypted keystore");
            let ks = keystore::Keystore::load(std::path::Path::new(&ks_path))
                .unwrap_or_else(|e| {
                    eprintln!("ERROR: Failed to load keystore from {ks_path}: {e}");
                    std::process::exit(1);
                });
            let key_bytes = ks.decrypt(&password)
                .unwrap_or_else(|e| {
                    eprintln!("ERROR: Failed to decrypt keystore: {e}");
                    std::process::exit(1);
                });
            BlsSecretKey::from_bytes(&key_bytes).unwrap_or_else(|e| {
                eprintln!("ERROR: Invalid BLS secret key in keystore: {e}");
                std::process::exit(1);
            })
        } else if let Ok(hex_key) = std::env::var("N42_VALIDATOR_KEY") {
            warn!(target: "n42::cli", "using plaintext N42_VALIDATOR_KEY — use N42_KEYSTORE_PATH for production");
            let bytes = hex::decode(&hex_key).unwrap_or_else(|e| {
                eprintln!("ERROR: N42_VALIDATOR_KEY must be valid hex: {e}");
                std::process::exit(1);
            });
            let key_bytes: [u8; 32] = bytes.try_into().unwrap_or_else(|v: Vec<u8>| {
                eprintln!("ERROR: N42_VALIDATOR_KEY must be exactly 32 bytes, got {}", v.len());
                std::process::exit(1);
            });
            BlsSecretKey::from_bytes(&key_bytes).unwrap_or_else(|e| {
                eprintln!("ERROR: Invalid BLS secret key from N42_VALIDATOR_KEY: {e}");
                std::process::exit(1);
            })
        } else {
            warn!(target: "n42::cli", "No validator key configured, generating random key (dev mode)");
            BlsSecretKey::random().expect("Failed to generate random BLS key")
        };

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
            .unwrap_or_else(|| {
                if consensus_config.initial_validators.is_empty() {
                    // Dev mode with auto-generated validator set — index 0 is correct.
                    0
                } else {
                    warn!(
                        target: "n42::cli",
                        "this node's BLS public key not found in the validator set; \
                         defaulting to observer mode (index 0, no block rewards)"
                    );
                    0
                }
            });

        let fee_recipient = consensus_config
            .initial_validators
            .get(my_index as usize)
            .map(|v| v.address)
            .unwrap_or_else(|| {
                warn!(
                    target: "n42::cli",
                    "no fee recipient address found for validator index {my_index}, \
                     block rewards will be sent to Address::ZERO"
                );
                Address::ZERO
            });

        info!(
            target: "n42::cli",
            validator_index = my_index,
            validator_count = validator_set.len(),
            %fee_recipient,
            "validator identity resolved"
        );

        let consensus_state = Arc::new(SharedConsensusState::new(validator_set.clone()));
        let n42_node = N42Node::new(consensus_state.clone());

        // Create staking manager early so it can be shared with RPC.
        let data_dir: PathBuf = std::env::var("N42_DATA_DIR")
            .unwrap_or_else(|_| "./n42-data".to_string())
            .into();
        let staking_state_file = data_dir.join("staking_state.json");
        let staking_manager = Arc::new(Mutex::new(
            StakingManager::load_or_new(&staking_state_file),
        ));
        info!(target: "n42::cli", "StakingManager initialized");

        let rpc_consensus_state = consensus_state.clone();
        let rpc_staking_manager = staking_manager.clone();
        let handle = builder
            .node(n42_node)
            .extend_rpc_modules(move |ctx| {
                let rpc_server = N42RpcServer::new(rpc_consensus_state)
                    .with_staking_manager(rpc_staking_manager);
                ctx.modules.merge_configured(rpc_server.into_rpc())?;
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

                // ── Observer mode ──────────────────────────────────────
                if env_bool("N42_OBSERVER_MODE") {
                    info!(target: "n42::cli", "Starting in OBSERVER mode (no consensus participation)");

                    let keypair = libp2p::identity::Keypair::generate_ed25519();
                    let local_peer_id = keypair.public().to_peer_id();
                    info!(target: "n42::cli", %local_peer_id, "Observer P2P identity (random)");

                    let mut transport_config =
                        TransportConfig::for_network_size(validator_set.len() as usize);
                    transport_config.enable_mdns = env_bool("N42_ENABLE_MDNS");
                    transport_config.enable_kademlia = env_bool("N42_ENABLE_DHT");

                    let swarm =
                        build_swarm_with_validator_index(keypair, transport_config, None)
                            .expect("Failed to build libp2p swarm");

                    let (mut net_service, net_handle, _consensus_event_rx, net_event_rx) =
                        NetworkService::new(swarm).expect("Failed to create NetworkService");

                    let consensus_port: u16 = env_parse("N42_CONSENSUS_PORT").unwrap_or(9400);
                    let listen_addr: libp2p::Multiaddr =
                        format!("/ip4/0.0.0.0/udp/{}/quic-v1", consensus_port).parse().unwrap();

                    if let Err(e) = net_service.listen_on(listen_addr.clone()) {
                        warn!(target: "n42::cli", error = %e, %listen_addr, "Failed to listen on consensus P2P address");
                    } else {
                        info!(target: "n42::cli", %listen_addr, "Observer P2P listening");
                    }

                    task_executor.spawn_critical_task(
                        "n42-p2p-network",
                        Box::pin(net_service.run()),
                    );

                    connect_trusted_peers(&net_handle);

                    let observer = ObserverOrchestrator::new(
                        net_handle,
                        net_event_rx,
                        beacon_engine_handle,
                        head_block_hash,
                    ).with_validator_set(validator_set)
                     .with_blob_store(full_node.pool.blob_store().clone());

                    task_executor.spawn_critical_task(
                        "n42-observer-orchestrator",
                        Box::pin(observer.run()),
                    );

                    info!(target: "n42::cli", "Observer node started — EL sync via reth eth P2P, CL data via GossipSub");
                    return Ok(());
                }

                // ── Normal consensus mode ──────────────────────────────
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

                let (mut net_service, net_handle, consensus_event_rx, net_event_rx) =
                    NetworkService::new(swarm).expect("Failed to create NetworkService");

                let consensus_port: u16 = env_parse("N42_CONSENSUS_PORT").unwrap_or(9400);
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

                connect_trusted_peers(&net_handle);

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

                let starhub_port = env_parse("N42_STARHUB_PORT").unwrap_or(9443);
                let shard_count = env_parse("N42_STARHUB_SHARDS").unwrap_or(1);
                let low_mem = env_bool("N42_LOW_MEMORY");
                let max_conns_per_shard: usize = env_parse("N42_STARHUB_MAX_CONNS")
                    .unwrap_or(if low_mem { 100 } else { 10_000 });

                let (sharded_hub, star_hub_handle, hub_event_rx) =
                    ShardedStarHub::new(ShardedStarHubConfig {
                        base_port: starhub_port,
                        shard_count,
                        max_connections_per_shard: max_conns_per_shard,
                        cert_dir: Some(data_dir.join("certs")),
                        ..Default::default()
                    }).expect("Failed to create ShardedStarHub — check base_port + shard_count");

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

                let reward_epoch_blocks = env_parse("N42_REWARD_EPOCH_BLOCKS").unwrap_or(21600); // 24h at 4s
                let daily_base_reward_gwei = env_parse("N42_DAILY_BASE_REWARD_GWEI").unwrap_or(100_000_000); // 0.1 N42
                let reward_curve_k: f64 = env_parse("N42_REWARD_CURVE_K").unwrap_or(4.0);
                let max_rewards_per_block = env_parse("N42_MAX_REWARDS_PER_BLOCK").unwrap_or(32);
                let unstaked_reward_ratio: f64 = env_parse("N42_UNSTAKED_REWARD_RATIO").unwrap_or(0.1);

                if reward_epoch_blocks == 0 {
                    eprintln!("ERROR: N42_REWARD_EPOCH_BLOCKS must be > 0");
                    std::process::exit(1);
                }
                if reward_curve_k <= 0.0 || !reward_curve_k.is_finite() {
                    eprintln!("ERROR: N42_REWARD_CURVE_K must be a positive finite number, got {reward_curve_k}");
                    std::process::exit(1);
                }
                if max_rewards_per_block == 0 {
                    eprintln!("ERROR: N42_MAX_REWARDS_PER_BLOCK must be > 0");
                    std::process::exit(1);
                }
                if !(0.0..=1.0).contains(&unstaked_reward_ratio) || !unstaked_reward_ratio.is_finite() {
                    eprintln!("ERROR: N42_UNSTAKED_REWARD_RATIO must be between 0.0 and 1.0, got {unstaked_reward_ratio}");
                    std::process::exit(1);
                }

                let reward_manager = Arc::new(Mutex::new(MobileRewardManager::new(
                    reward_epoch_blocks,
                    daily_base_reward_gwei,
                    reward_curve_k,
                    max_rewards_per_block,
                    unstaked_reward_ratio,
                )));

                info!(
                    target: "n42::cli",
                    epoch_blocks = reward_epoch_blocks,
                    daily_base_reward_gwei,
                    curve_k = reward_curve_k,
                    max_per_block = max_rewards_per_block,
                    unstaked_reward_ratio,
                    "mobile reward manager configured (logarithmic curve, tiered rewards)"
                );

                let (attest_tx, mut attest_rx) = mpsc::channel(256);
                let (phone_connected_tx, phone_connected_rx) = mpsc::channel(128);
                let (reward_attest_tx, mut reward_attest_rx) = mpsc::unbounded_channel();

                let attestation_store_path = data_dir.join("attestation_store.json");
                let attestation_store = Arc::new(Mutex::new(
                    AttestationStore::new(attestation_store_path)
                        .expect("failed to initialize attestation store"),
                ));

                let mobile_bridge = MobileVerificationBridge::new(hub_event_rx, 10, 1000)
                    .with_attestation_tx(attest_tx)
                    .with_phone_connected_tx(phone_connected_tx)
                    .with_consensus_state(consensus_state.clone())
                    .with_reward_tx(reward_attest_tx)
                    .with_staking_manager(staking_manager.clone())
                    .with_attestation_store(attestation_store.clone());

                task_executor.spawn_critical_task("n42-mobile-bridge", Box::pin(mobile_bridge.run()));

                let reward_mgr_tracker = reward_manager.clone();
                task_executor.spawn_critical_task(
                    "n42-reward-tracker",
                    Box::pin(async move {
                        while let Some(pubkey) = reward_attest_rx.recv().await {
                            if let Ok(mut mgr) = reward_mgr_tracker.lock() {
                                mgr.record_attestation(&pubkey);
                            }
                        }
                    }),
                );

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

                let (output_tx, output_rx) = mpsc::channel(if low_mem { 64 } else { 1024 });
                let snapshot = match persistence::load_consensus_state(&state_file) {
                    Ok(snap) => snap,
                    Err(e) => {
                        warn!(target: "n42::cli", error = %e, "failed to load consensus snapshot, starting fresh");
                        None
                    }
                };

                let mut restored_block_count: u64 = 0;
                let consensus_engine = if let Some(snapshot) = snapshot {
                    restored_block_count = snapshot.committed_block_count;
                    info!(
                        target: "n42::cli",
                        view = snapshot.current_view,
                        locked_qc_view = snapshot.locked_qc.view,
                        last_committed_view = snapshot.last_committed_qc.view,
                        committed_block_count = snapshot.committed_block_count,
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
                    if !snapshot.authorized_verifiers.is_empty() {
                        consensus_state.restore_authorized_verifiers(&snapshot.authorized_verifiers);
                        info!(
                            target: "n42::cli",
                            count = snapshot.authorized_verifiers.len(),
                            "restored authorized verifiers from snapshot"
                        );
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

                let tx_chan_size = if low_mem { 256 } else { 4096 };
                let (tx_import_tx, tx_import_rx) = mpsc::channel::<Vec<u8>>(tx_chan_size);
                let (tx_broadcast_tx, tx_broadcast_rx) = mpsc::channel::<Vec<u8>>(tx_chan_size);

                // Run TxPoolBridge on a dedicated runtime to isolate TX pool
                // ingestion from the consensus event loop. Under high TPS, pool
                // operations (validation, nonce checks) consume significant CPU
                // and would otherwise compete with consensus message processing.
                let pool_for_bridge = full_node.pool.clone();
                let bridge_workers = if low_mem { 1 } else { 2 };
                std::thread::Builder::new()
                    .name("n42-tx-pool-bridge".into())
                    .spawn(move || {
                        let rt = tokio::runtime::Builder::new_multi_thread()
                            .worker_threads(bridge_workers)
                            .thread_name("tx-bridge-worker")
                            .enable_all()
                            .build()
                            .expect("failed to create tx-bridge runtime");
                        rt.block_on(
                            TxPoolBridge::new(pool_for_bridge, tx_import_rx, tx_broadcast_tx)
                                .run(),
                        );
                    })
                    .expect("failed to spawn tx-bridge thread");
                info!(target: "n42::cli", "TxPoolBridge started on dedicated runtime");

                // Binary TCP injection server for high-speed TX ingestion.
                // Bypasses JSON-RPC overhead. Enable with N42_INJECT_PORT=19900.
                if std::env::var("N42_INJECT_PORT").is_ok() {
                    let inject_pool = full_node.pool.clone();
                    task_executor.spawn_critical_task(
                        "n42-inject-server",
                        Box::pin(n42_node::inject::run_inject_server(inject_pool)),
                    );
                }

                // Load epoch schedule from $N42_DATA_DIR/epoch_schedule.json (optional).
                let epoch_schedule_path = data_dir.join("epoch_schedule.json");
                let epoch_schedule = match EpochSchedule::load(&epoch_schedule_path) {
                    Ok(Some(schedule)) => {
                        info!(
                            target: "n42::cli",
                            path = %epoch_schedule_path.display(),
                            epoch_count = schedule.len(),
                            "epoch schedule loaded"
                        );
                        Some(schedule)
                    }
                    Ok(None) => {
                        // File absent: dynamic validator rotation not configured (static set).
                        None
                    }
                    Err(e) => {
                        warn!(
                            target: "n42::cli",
                            error = %e,
                            "failed to load epoch schedule, proceeding without dynamic validator rotation"
                        );
                        None
                    }
                };

                let mut orchestrator = ConsensusOrchestrator::with_engine_api(
                    consensus_engine,
                    net_handle,
                    consensus_event_rx,
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
                .with_blob_store(full_node.pool.blob_store().clone())
                .with_mobile_reward_manager(reward_manager)
                .with_staking_manager(staking_manager.clone())
                .with_committed_block_count(restored_block_count);

                if let Some(schedule) = epoch_schedule {
                    orchestrator = orchestrator.with_epoch_schedule(schedule);
                }

                let orchestrator = orchestrator;

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
