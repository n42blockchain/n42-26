mod keystore;

// jemalloc is unix-only; tikv-jemallocator is a target.'cfg(unix)' dependency,
// so the `feature = "jemalloc"` cfg is gated on `unix` to keep Windows builds happy.
#[cfg(all(feature = "jemalloc", unix))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

use alloy_primitives::{Address, B256, U256, keccak256};
use clap::Parser;
use n42_chainspec::{ConsensusConfig, ValidatorInfo};
use n42_consensus::{ConsensusEngine, EpochManager, ValidatorSet, ValidatorSetResolver};
use n42_jmt::{EMPTY_CODE_HASH, PersistentSbmt, PersistentTwig};
use n42_network::NetworkService;
use n42_network::{
    FinalizedRangeVerification, ShardedStarHub, ShardedStarHubConfig, TransportConfig,
    build_interop_observer_swarm, build_swarm_with_validator_index,
    deterministic_validator_keypair, verify_finalized_range_stream,
};
use n42_node::attestation_store::AttestationStore;
use n42_node::consensus_state::configured_min_mobile_verifiers;
use n42_node::epoch_schedule::EpochSchedule;
use n42_node::mobile_bridge::MobileVerificationBridge;
use n42_node::mobile_evidence::spawn_mobile_evidence_writeback;
use n42_node::mobile_packet::mobile_packet_loop;
use n42_node::mobile_reward::MobileRewardManager;
use n42_node::persistence;
use n42_node::rpc::{N42ApiServer, N42RpcServer};
use n42_node::staking::StakingManager;
use n42_node::tx_bridge::TxPoolBridge;
use n42_node::{ConsensusOrchestrator, N42Node, ObserverOrchestrator, SharedConsensusState};
use n42_node::{configured_validator_peer_ids, expected_validator_peer_ids_with_policy};
use n42_primitives::BlsSecretKey;
use n42_twig_core::qmdb_compat::{QmdbPortableVerification, verify_portable_stream};
use n42_zkproof::{MockProver, ProofScheduler, ProofStore};
use reth_chainspec::ChainSpecProvider;
use reth_ethereum_cli::Cli;
use reth_ethereum_cli::chainspec::EthereumChainSpecParser;
use reth_node_core::args::{DefaultEngineValues, DefaultRpcServerArgs};
use reth_storage_api::{BlockHashReader, BlockNumReader};
use reth_transaction_pool::TransactionPool;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Once, atomic::AtomicBool};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

fn env_bool(name: &str) -> bool {
    std::env::var(name)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

fn env_parse<T: std::str::FromStr>(name: &str) -> Option<T> {
    std::env::var(name).ok().and_then(|s| s.parse().ok())
}

fn resolve_observer_genesis_hash(
    local_genesis_hash: B256,
    configured_hash: Option<&str>,
) -> eyre::Result<B256> {
    let Some(configured_hash) = configured_hash else {
        return Ok(local_genesis_hash);
    };

    configured_hash.parse::<B256>().map_err(|error| {
        eyre::eyre!("N42_INTEROP_GENESIS_HASH must be a 32-byte 0x-prefixed hash: {error}")
    })
}

fn verify_observer_qmdb_bootstrap(
    path: &Path,
    chain_id: u64,
    genesis_hash: B256,
    expected_block: u64,
    expected_block_hash: B256,
    expected_root: B256,
) -> eyre::Result<QmdbPortableVerification> {
    let file = File::open(path).map_err(|error| {
        eyre::eyre!(
            "failed to open N42_QMDB_BOOTSTRAP {}: {error}",
            path.display()
        )
    })?;
    let expected_genesis: [u8; 32] = genesis_hash.into();
    let verified = verify_portable_stream(BufReader::new(file), chain_id, &expected_genesis)
        .map_err(|error| eyre::eyre!("invalid QMDB bootstrap {}: {error}", path.display()))?;
    if verified.block_number != expected_block {
        return Err(eyre::eyre!(
            "QMDB bootstrap block {} does not equal N42_QMDB_BOOTSTRAP_BLOCK {expected_block}",
            verified.block_number
        ));
    }
    if verified.block_hash.as_slice() != expected_block_hash.as_slice() {
        return Err(eyre::eyre!(
            "QMDB bootstrap block hash does not equal N42_QMDB_BOOTSTRAP_BLOCK_HASH"
        ));
    }
    if verified.root.as_slice() != expected_root.as_slice() {
        return Err(eyre::eyre!(
            "QMDB bootstrap root does not equal N42_QMDB_BOOTSTRAP_ROOT"
        ));
    }
    Ok(verified)
}

fn required_observer_env<T>(name: &str) -> eyre::Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    let value = std::env::var(name)
        .map_err(|_| eyre::eyre!("{name} is required when N42_QMDB_BOOTSTRAP is set"))?;
    value
        .parse::<T>()
        .map_err(|error| eyre::eyre!("invalid {name}: {error}"))
}

fn ensure_finalized_range_matches_qmdb(
    range: &FinalizedRangeVerification,
    checkpoint: &QmdbPortableVerification,
) -> eyre::Result<()> {
    if range.to_block != checkpoint.block_number
        || range.last_block_hash != B256::from(checkpoint.block_hash)
        || range.last_state_root != B256::from(checkpoint.root)
    {
        return Err(eyre::eyre!(
            "finalized range head does not match QMDB bootstrap checkpoint"
        ));
    }
    Ok(())
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
    for peer_addr_str in trusted_peers_str
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
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
                } else {
                    warn!(
                        target: "n42::cli",
                        addr = peer_addr_str,
                        "trusted peer multiaddr is missing /p2p/<peer_id>; dialing once but skipping reconnection and kademlia registration"
                    );
                }
            }
            Err(e) => {
                warn!(target: "n42::cli", error = %e, addr = peer_addr_str, "invalid trusted peer multiaddr");
            }
        }
    }
}

fn build_epoch_manager(
    initial_validator_set: &ValidatorSet,
    initial_validator_infos: &[ValidatorInfo],
    epoch_length: u64,
    max_historical_epochs: usize,
    recovery_epoch_schedule: Option<&EpochSchedule>,
    recovered_view: Option<u64>,
    snapshot_current_epoch: Option<&(u64, Vec<ValidatorInfo>, u32)>,
) -> eyre::Result<EpochManager> {
    if epoch_length == 0 {
        return Ok(EpochManager::new(initial_validator_set.clone()));
    }

    let current_epoch = recovered_view
        .map(|view| view.saturating_sub(1) / epoch_length)
        .unwrap_or(0);

    // If the snapshot recorded the live validator set, use it directly. This
    // ensures that a dynamic `proposeAddValidator` survives restart even when
    // `epoch_schedule.json` has not been updated. Older snapshots may have kept
    // `snap_epoch` at 0 across unchanged boundaries while `current_view` already
    // belongs to a later mathematical epoch; migrate the epoch number but never
    // discard the snapshot's authoritative active membership.
    if let Some((snap_epoch, snap_validators, snap_ft)) = snapshot_current_epoch
        && !snap_validators.is_empty()
    {
        let vs = ValidatorSet::try_new(snap_validators, *snap_ft)
            .map_err(|e| eyre::eyre!("snapshot current_epoch_validators invalid: {e}"))?;
        if *snap_epoch != current_epoch {
            tracing::warn!(
                snapshot_epoch = snap_epoch,
                recovered_view_epoch = current_epoch,
                "migrating legacy snapshot epoch number while preserving its validator set"
            );
        }
        tracing::info!(
            epoch = current_epoch,
            validators = snap_validators.len(),
            "restored current epoch validator set from snapshot"
        );
        return Ok(EpochManager::from_epoch_with_history(
            vs,
            epoch_length,
            current_epoch,
            max_historical_epochs,
        ));
    }

    if let Some(schedule) = recovery_epoch_schedule {
        let recovery_window = schedule.recovery_window(
            initial_validator_infos,
            initial_validator_set.fault_tolerance(),
            current_epoch,
            3,
        );
        EpochManager::from_schedule_with_history(
            epoch_length,
            &recovery_window,
            max_historical_epochs,
        )
        .map_err(|e| eyre::eyre!("failed to reconstruct epoch manager from schedule: {e}"))
    } else if current_epoch > 0 {
        Ok(EpochManager::from_epoch_with_history(
            initial_validator_set.clone(),
            epoch_length,
            current_epoch,
            max_historical_epochs,
        ))
    } else {
        Ok(EpochManager::with_epoch_length_and_history(
            initial_validator_set.clone(),
            epoch_length,
            max_historical_epochs,
        ))
    }
}

fn build_validator_set_resolver(
    initial_validator_infos: Vec<ValidatorInfo>,
    initial_fault_tolerance: u32,
    epoch_length: u64,
    epoch_schedule: Option<EpochSchedule>,
) -> ValidatorSetResolver {
    Arc::new(move |view: u64| {
        // `checked_div` returns None when epoch_length == 0 (epochs disabled).
        let epoch_opt = view.saturating_sub(1).checked_div(epoch_length);
        let (validators, fault_tolerance) = if let Some(epoch) = epoch_opt {
            epoch_schedule
                .as_ref()
                .map(|schedule| {
                    schedule.active_config_for_epoch(
                        epoch,
                        initial_validator_infos.as_slice(),
                        initial_fault_tolerance,
                    )
                })
                .unwrap_or((initial_validator_infos.as_slice(), initial_fault_tolerance))
        } else {
            (initial_validator_infos.as_slice(), initial_fault_tolerance)
        };

        ValidatorSet::try_new(validators, fault_tolerance)
            .ok()
            .map(Arc::new)
    })
}

fn derive_ed25519_keypair(index: u32) -> eyre::Result<libp2p::identity::Keypair> {
    static WARN_DETERMINISTIC_P2P_IDENTITY: Once = Once::new();
    WARN_DETERMINISTIC_P2P_IDENTITY.call_once(|| {
        warn!(
            target: "n42::cli",
            "validator libp2p identities are still deterministically derived from public validator indices; this is not sufficient for production-grade peer authentication"
        );
    });
    deterministic_validator_keypair(index)
}

fn load_explicit_p2p_keypair() -> eyre::Result<Option<libp2p::identity::Keypair>> {
    let Ok(hex_key) = std::env::var("N42_P2P_KEY") else {
        return Ok(None);
    };

    let bytes = hex::decode(&hex_key)
        .map_err(|error| eyre::eyre!("N42_P2P_KEY must be valid hex: {error}"))?;
    let mut key_bytes: [u8; 32] = bytes.try_into().map_err(|value: Vec<u8>| {
        eyre::eyre!("N42_P2P_KEY must be exactly 32 bytes, got {}", value.len())
    })?;
    let secret = libp2p::identity::ed25519::SecretKey::try_from_bytes(&mut key_bytes)
        .map_err(|error| eyre::eyre!("N42_P2P_KEY is not a valid ed25519 secret key: {error}"))?;

    Ok(Some(libp2p::identity::Keypair::from(
        libp2p::identity::ed25519::Keypair::from(secret),
    )))
}

fn resolve_validator_p2p_keypair(
    validator_index: u32,
    expected_peer_id: Option<libp2p::PeerId>,
) -> eyre::Result<libp2p::identity::Keypair> {
    let keypair = if let Some(keypair) = load_explicit_p2p_keypair()? {
        keypair
    } else if expected_peer_id.is_some() {
        return Err(eyre::eyre!(
            "validator {validator_index} has configured p2p_peer_id but N42_P2P_KEY is not set"
        ));
    } else {
        derive_ed25519_keypair(validator_index)?
    };

    let local_peer_id = keypair.public().to_peer_id();
    if let Some(expected_peer_id) = expected_peer_id
        && local_peer_id != expected_peer_id
    {
        return Err(eyre::eyre!(
            "local libp2p identity mismatch for validator {validator_index}: expected {expected_peer_id}, got {local_peer_id}"
        ));
    }

    Ok(keypair)
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
        eprintln!("\n!!! PANIC in thread '{name}' !!!\n{info}\n\nBacktrace:\n{bt}\n");
    }));

    if std::env::var_os("RUST_BACKTRACE").is_none() {
        // SAFETY: Called from main() before any threads are spawned.
        unsafe { std::env::set_var("RUST_BACKTRACE", "1") };
    }

    // Leave HTTP RPC disabled by default for release-oriented startup.
    //
    // NOTE: Do NOT call with_http_api()/with_ws_api() here — reth's
    // RpcModuleSelection::Display wraps output in brackets that its own
    // FromStr parser rejects. Use reth's built-in defaults (eth,net,web3).
    // Operators can still opt in via `--http` or `N42_ENABLE_HTTP_RPC=1`.
    // Leave CORS unset unless explicitly configured.
    let rpc_defaults = if env_bool("N42_ENABLE_HTTP_RPC") {
        DefaultRpcServerArgs::default().with_http(true)
    } else {
        DefaultRpcServerArgs::default()
    };
    if let Err(error) = rpc_defaults.try_init() {
        warn!(
            target: "n42::cli",
            error = ?error,
            "failed to apply RPC default server args"
        );
    }

    // Parallel state-root computation currently falls back repeatedly on this workload,
    // so make the synchronous path the default unless the operator overrides it explicitly.
    let engine_defaults = DefaultEngineValues::default().with_state_root_fallback(true);
    if let Err(error) = engine_defaults.try_init() {
        warn!(
            target: "n42::cli",
            error = ?error,
            "failed to apply engine default args"
        );
    }

    if let Err(err) = Cli::<EthereumChainSpecParser>::parse().run(async move |builder, _| {
        info!(target: "n42::cli", "Launching N42 node");

        // Warn about benchmark/debug env vars that weaken security.
        for (var, desc) in [
            ("N42_SKIP_TX_VERIFY", "signature verification bypassed"),
            ("N42_SKIP_STATE_ROOT", "state root computation skipped"),
            ("N42_DEFER_STATE_ROOT", "state root computation deferred"),
        ] {
            if std::env::var(var).is_ok_and(|v| v == "1") {
                warn!(target: "n42::cli", var, desc, "BENCHMARK MODE: {var}=1 — NOT FOR PRODUCTION");
            }
        }

        // Hard guard (RFC production-safe-deferred-state-root, Phase A): the
        // state-root bypass flags refuse to start outside an explicitly
        // allowed benchmark chain. A warn-only gate let "temporary" bench
        // flags reach long-lived deployments with verification silently off.
        let env_is_1 = |var: &str| std::env::var(var).is_ok_and(|v| v == "1");
        let skip_root = env_is_1("N42_SKIP_STATE_ROOT");
        let defer_root = env_is_1("N42_DEFER_STATE_ROOT");
        if let Err(msg) = n42_node::validate_state_root_bypass_flags(
            skip_root,
            defer_root,
            env_is_1("N42_ALLOW_BENCH_MODE"),
            builder.config().chain.chain.id(),
        ) {
            eprintln!("ERROR: {msg}");
            std::process::exit(1);
        }
        if skip_root || defer_root {
            warn!(
                target: "n42::cli",
                chain_id = builder.config().chain.chain.id(),
                skip_root,
                defer_root,
                "!!! UNSAFE BENCHMARK MODE ACTIVE: STATE-ROOT CONSENSUS CHECKS ARE BYPASSED !!!"
            );
        }
        metrics::gauge!("n42_deferred_state_root_enabled")
            .set(if skip_root || defer_root { 1.0 } else { 0.0 });

        // Priority: N42_CONSENSUS_CONFIG file > N42_VALIDATOR_COUNT dev mode.
        let consensus_config_path = std::env::var("N42_CONSENSUS_CONFIG").ok();
        let consensus_config_from_file = consensus_config_path.is_some();
        let mut consensus_config = match consensus_config_path {
            Some(path) => {
                info!(target: "n42::cli", path, "loading consensus config from file");
                ConsensusConfig::from_file(std::path::Path::new(&path))
                    .unwrap_or_else(|e| {
                        eprintln!("ERROR: Failed to load consensus config from {path}: {e}");
                        std::process::exit(1);
                    })
            }
            None => {
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
            BlsSecretKey::random().unwrap_or_else(|error| {
                eprintln!("ERROR: Failed to generate random BLS key: {error}");
                std::process::exit(1);
            })
        };

        let initial_validator_set = if consensus_config.initial_validators.is_empty() {
            ValidatorSet::try_new(
                &[n42_chainspec::ValidatorInfo {
                    address: Address::ZERO,
                    bls_public_key: secret_key.public_key(),
                    p2p_peer_id: None,
                }],
                0,
            )
        } else {
            ValidatorSet::try_new(
                &consensus_config.initial_validators,
                consensus_config.fault_tolerance,
            )
        }
        .map_err(|e| eyre::eyre!("invalid initial validator set: {e}"))?;

        let data_dir: PathBuf = std::env::var("N42_DATA_DIR")
            .unwrap_or_else(|_| "./n42-data".to_string())
            .into();
        let observer_mode = env_bool("N42_OBSERVER_MODE");
        let allow_deterministic_validator_peers =
            !consensus_config_from_file || env_bool("N42_ALLOW_DETERMINISTIC_P2P");
        if !observer_mode && consensus_config_from_file && allow_deterministic_validator_peers {
            warn!(
                target: "n42::cli",
                "N42_ALLOW_DETERMINISTIC_P2P=1 enables weak, publicly derivable validator PeerIds; this is not suitable for production"
            );
        }
        let epoch_schedule_path = data_dir.join("epoch_schedule.json");
        let epoch_schedule = match EpochSchedule::load(&epoch_schedule_path) {
            Ok(Some(schedule)) => {
                if !observer_mode && let Err(error) =
                    schedule.validate_peer_binding_policy(allow_deterministic_validator_peers)
                {
                    eprintln!(
                        "ERROR: invalid epoch schedule peer binding policy in {}: {error}",
                        epoch_schedule_path.display()
                    );
                    std::process::exit(1);
                }
                info!(
                    target: "n42::cli",
                    path = %epoch_schedule_path.display(),
                    epoch_count = schedule.len(),
                    "epoch schedule loaded"
                );
                Some(schedule)
            }
            Ok(None) => None,
            Err(e) => {
                warn!(
                    target: "n42::cli",
                    error = %e,
                    "failed to load epoch schedule, proceeding without dynamic validator rotation"
                );
                None
            }
        };

        let state_file = data_dir.join("consensus_state.json");
        let snapshot = match persistence::load_consensus_state(&state_file) {
            Ok(snap) => snap,
            Err(e) => {
                return Err(eyre::eyre!(
                    "failed to load consensus snapshot {}: {e}",
                    state_file.display()
                ));
            }
        };

        // Open the durable vote log. fsync'd before every R1/R2 vote so a crash
        // after signing cannot drop the recorded view (HotStuff-2 paper
        // safety invariant — see crates/n42-consensus/src/vote_log.rs).
        let vote_log_path = data_dir.join("vote_log.bin");
        let vote_log: std::sync::Arc<dyn n42_consensus::VoteLogWriter> = std::sync::Arc::new(
            persistence::FileVoteLog::open(vote_log_path.clone()).map_err(|e| {
                eyre::eyre!(
                    "failed to open crash-safe vote log {}: {e}",
                    vote_log_path.display()
                )
            })?,
        );
        let (last_voted_view_from_disk, last_commit_voted_view_from_disk) =
            persistence::load_last_vote_views(&vote_log_path).map_err(|e| {
                eyre::eyre!(
                    "failed to read crash-safe vote log {}: {e}",
                    vote_log_path.display()
                )
            })?;

        let initial_validator_infos = initial_validator_set.validator_infos();
        let startup_epoch_manager = build_epoch_manager(
            &initial_validator_set,
            &initial_validator_infos,
            consensus_config.epoch_length,
            consensus_config.max_historical_epochs,
            epoch_schedule.as_ref(),
            snapshot.as_ref().map(|snap| snap.current_view),
            snapshot.as_ref().and_then(|snap| snap.current_epoch_validators.as_ref()),
        )?;
        let startup_validator_set = startup_epoch_manager.current_validator_set().clone();
        let startup_validator_infos = startup_validator_set.validator_infos();
        let configured_validator_peer_ids =
            configured_validator_peer_ids(&startup_validator_infos)?;
        let expected_validator_peer_ids = if observer_mode {
            Default::default()
        } else {
            expected_validator_peer_ids_with_policy(
                &startup_validator_infos,
                allow_deterministic_validator_peers,
            )?
        };
        if !observer_mode
            && startup_validator_set.len() > 1
            && configured_validator_peer_ids.is_empty()
        {
            warn!(
                target: "n42::cli",
                validator_count = startup_validator_set.len(),
                "no validator p2p_peer_id bindings configured; falling back to deterministic libp2p identities"
            );
        }

        let my_pubkey = secret_key.public_key();
        let resolved_my_index = startup_validator_set.index_of_public_key(&my_pubkey);
        let my_index = resolved_my_index.unwrap_or_else(|| {
                if consensus_config.initial_validators.is_empty() {
                    // Dev mode with auto-generated validator set — index 0 is correct.
                    0
                } else {
                    // Joining validator: BLS key not in initial set.
                    // Use val_count as provisional index so the libp2p identity
                    // is unique and auto-connect port math stays correct.
                    let provisional = startup_validator_set.len();
                    warn!(
                        target: "n42::cli",
                        provisional_index = provisional,
                        "BLS key not in initial validator set — joining as provisional index {provisional}; \
                         will update after epoch sync"
                    );
                    provisional
                }
            });

        let fee_recipient = startup_validator_set
            .get_address(my_index)
            .copied()
            .unwrap_or_else(|_| {
                if resolved_my_index.is_none() {
                    info!(
                        target: "n42::cli",
                        "joining validator (provisional index {my_index}) — \
                         fee recipient will be resolved after epoch sync"
                    );
                } else {
                    warn!(
                        target: "n42::cli",
                        "no fee recipient address found for validator index {my_index}, \
                         block rewards will be sent to Address::ZERO"
                    );
                }
                Address::ZERO
            });

        info!(
            target: "n42::cli",
            validator_index = my_index,
            validator_count = startup_validator_set.len(),
            %fee_recipient,
            "validator identity resolved"
        );

        let consensus_state = Arc::new(SharedConsensusState::new(startup_validator_set.clone()));
        let (admin_tx, admin_rx) = tokio::sync::mpsc::channel(16);
        consensus_state.set_admin_channel(admin_tx);
        // admin_rx will be moved into the on_node_started closure below.
        let admin_rx = std::sync::Mutex::new(Some(admin_rx));
        let validator_set_resolver = build_validator_set_resolver(
            initial_validator_infos.clone(),
            startup_validator_set.fault_tolerance(),
            consensus_config.epoch_length,
            epoch_schedule.clone(),
        );
        // Consensus evidence store: MDBX table for per-block QC persistence.
        let evidence_store: Option<Arc<n42_jmt::EvidenceStore>> = {
            let evidence_path = data_dir.join("evidence");
            n42_jmt::open_jmt_env(&evidence_path)
                .and_then(n42_jmt::EvidenceStore::open)
                .map(|store| {
                    info!(target: "n42::cli", path = %evidence_path.display(), "EvidenceStore initialized");
                    Arc::new(store)
                })
                .map_err(|e| warn!(target: "n42::cli", error = %e, "EvidenceStore unavailable"))
                .ok()
        };

        let n42_node = N42Node::new(consensus_state.clone())
            .with_validator_set_resolver(validator_set_resolver);

        // Create staking manager early so it can be shared with RPC.
        let staking_state_file = data_dir.join("staking_state.json");
        let staking_manager = Arc::new(Mutex::new(
            StakingManager::load_or_new(&staking_state_file),
        ));
        info!(target: "n42::cli", "StakingManager initialized");

        // QMDB/twig state tree: default backend for production and test runs.
        // `N42_TWIG` is the primary selector; if unset, we keep QMDB enabled by
        // default unless `N42_JMT=1` explicitly requests the reserve path.
        let twig_enabled = match std::env::var("N42_TWIG") {
            Ok(v) => v == "1" || v.eq_ignore_ascii_case("true"),
            Err(_) => !env_bool("N42_JMT"),
        };
        let jmt_enabled = env_bool("N42_JMT") && !twig_enabled;
        let twig_sidecar_healthy = Arc::new(AtomicBool::new(true));
        metrics::gauge!("n42_twig_sidecar_healthy").set(1.0);

        let jmt: Option<Arc<Mutex<PersistentSbmt>>> = if jmt_enabled {
            let snapshot_path = data_dir.join("sbmt.snapshot");
            let interval = env_parse::<u64>("N42_SBMT_SNAPSHOT_INTERVAL").unwrap_or(1000);
            match PersistentSbmt::open(&snapshot_path, interval) {
                Ok(tree) => {
                    info!(
                        target: "n42::cli",
                        version = tree.version(),
                        interval,
                        "SBMT state tree enabled (16 shards, snapshot+WAL persistent)"
                    );
                    Some(Arc::new(Mutex::new(tree)))
                }
                Err(e) => {
                    warn!(target: "n42::cli", error = %e, "failed to open persistent SBMT; disabling state tree");
                    None
                }
            }
        } else {
            None
        };

        // Twig/QMDB state tree: the default live state commitment sidecar.
        // It remains consensus-breaking in the sense that it does not gate
        // HotStuff finality, but it is the default proof backend now.
        let twig: Option<Arc<Mutex<PersistentTwig>>> = if twig_enabled {
            let snapshot_path = data_dir.join("twig.snapshot");
            let interval = env_parse::<u64>("N42_TWIG_SNAPSHOT_INTERVAL").unwrap_or(1000);
            match PersistentTwig::open(&snapshot_path, interval) {
                Ok(tree) => {
                    warn!(
                        target: "n42::cli",
                        "twig backend enabled; use a fresh data dir/clean genesis when switching from another state tree"
                    );
                    info!(
                        target: "n42::cli",
                        version = tree.version(),
                        interval,
                        "Twig/QMDB state tree enabled (16 shards, snapshot+WAL persistent)"
                    );
                    Some(Arc::new(Mutex::new(tree)))
                }
                Err(e) => {
                    warn!(target: "n42::cli", error = %e, "failed to open persistent twig backend; disabling state tree");
                    None
                }
            }
        } else {
            None
        };

        // ZK proof sidecar: create scheduler if N42_ZK_PROOF=1.
        // N42_ZK_BACKEND: "mock" (default) or "sp1" (requires sp1 feature + guest ELF)
        // N42_ZK_MODE: "mock" (default), "cpu" (real proof, slow)
        let zk_scheduler = if env_bool("N42_ZK_PROOF") {
            let interval: u64 = env_parse("N42_ZK_INTERVAL").unwrap_or(300);
            let backend = std::env::var("N42_ZK_BACKEND").unwrap_or_else(|_| "mock".to_string());
            let prover: Arc<dyn n42_zkproof::ZkProver> = match backend.as_str() {
                #[cfg(feature = "sp1")]
                "sp1" => {
                    let mode_str = std::env::var("N42_ZK_MODE").unwrap_or_else(|_| "mock".to_string());
                    let mode = n42_zkproof::Sp1Mode::from_str_lossy(&mode_str);
                    match n42_zkproof::Sp1Prover::from_env(mode) {
                        Ok(p) => {
                            info!(target: "n42::cli", ?mode, "SP1 prover backend loaded");
                            Arc::new(p)
                        }
                        Err(e) => {
                            warn!(target: "n42::cli", error=%e, "SP1 prover init failed, falling back to mock");
                            Arc::new(MockProver::new())
                        }
                    }
                }
                #[cfg(not(feature = "sp1"))]
                "sp1" => {
                    warn!(
                        target: "n42::cli",
                        "N42_ZK_BACKEND=sp1 but binary was built without --features sp1, falling back to mock"
                    );
                    Arc::new(MockProver::new())
                }
                "mock" => Arc::new(MockProver::new()),
                other => {
                    warn!(
                        target: "n42::cli",
                        backend = other,
                        "unknown ZK backend, falling back to mock"
                    );
                    Arc::new(MockProver::new())
                }
            };
            let backend_name = prover.name().to_string();
            let store = Arc::new(ProofStore::new(1000));
            let zk_state = consensus_state.clone();
            let callback: n42_zkproof::ProofCallback = Arc::new(move |block_number, block_hash| {
                zk_state.update_zk_proof(block_number, block_hash);
            });
            let scheduler = Arc::new(
                ProofScheduler::new(prover, interval, store).with_callback(callback),
            );
            info!(target: "n42::cli", interval, backend = %backend_name, "ZK proof sidecar enabled");
            Some(scheduler)
        } else {
            None
        };

        let rpc_consensus_state = consensus_state.clone();
        let rpc_staking_manager = staking_manager.clone();
        let rpc_zk_scheduler = zk_scheduler.clone();
        let rpc_jmt = jmt.clone();
        let rpc_twig = twig.clone();
        let rpc_admin_token = std::env::var("N42_ADMIN_TOKEN").ok();
        let handle = builder
            .node(n42_node)
            .extend_rpc_modules(move |ctx| {
                let mut rpc_server = N42RpcServer::new(rpc_consensus_state)
                    .with_staking_manager(rpc_staking_manager);
                if let Some(scheduler) = rpc_zk_scheduler {
                    rpc_server = rpc_server.with_zk_scheduler(scheduler);
                }
                if let Some(ref jmt) = rpc_jmt {
                    rpc_server = rpc_server.with_jmt(Arc::clone(jmt));
                }
                if let Some(ref twig) = rpc_twig {
                    rpc_server = rpc_server.with_twig(Arc::clone(twig));
                }
                if let Some(token) = rpc_admin_token {
                    rpc_server = rpc_server.with_admin_token(token);
                }
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

                let best_block_number = full_node.provider.best_block_number().unwrap_or(0);
                let head_block_hash = {
                    if best_block_number > 0 {
                        let hash = full_node
                            .provider
                            .block_hash(best_block_number)
                            .ok()
                            .flatten()
                            .unwrap_or(genesis_hash);
                        info!(
                            target: "n42::cli",
                            best_block = best_block_number,
                            %hash,
                            "using canonical chain head as head_block_hash"
                        );
                        hash
                    } else {
                        genesis_hash
                    }
                };

                if best_block_number > 0 && snapshot.is_none() {
                    return Err(eyre::eyre!(
                        "refusing to start consensus on reth block {best_block_number} without a valid consensus snapshot"
                    ));
                }

                if let Some(ref jmt) = jmt {
                    let mut tree = jmt.lock().unwrap_or_else(|e| {
                        warn!(target: "n42::cli", "SBMT mutex poisoned during genesis seed, recovering state");
                        e.into_inner()
                    });

                    // F6: seed genesis only on a truly fresh start.
                    // `restored_from_disk` guards the append-slot idempotency
                    // trap — genesis seeding does not bump the version, so a
                    // restart before the first block commits would otherwise
                    // reseed on top of restored state and diverge the
                    // (append-ordered) root from every un-restarted node.
                    if tree.version() == 0 && !tree.restored_from_disk() {
                        // Fresh start (no snapshot/WAL restored it): seed genesis alloc.
                        let chain_spec = full_node.provider.chain_spec();
                        let alloc_len = chain_spec.genesis.alloc.len();
                        for (address, account) in &chain_spec.genesis.alloc {
                            let code_hash = account
                                .code
                                .as_ref()
                                .filter(|code| !code.is_empty())
                                .map(|code| keccak256(code.as_ref()))
                                .unwrap_or(EMPTY_CODE_HASH);
                            let storage = account
                                .storage_slots()
                                .map(|(slot, value)| (U256::from_be_bytes(slot.0), value));

                            tree.inner_mut().seed_genesis_account(
                                *address,
                                account.balance,
                                account.nonce.unwrap_or_default(),
                                code_hash,
                                storage,
                            );
                        }

                        tree.flush().map_err(|error| {
                            eyre::eyre!(
                                "failed to persist genesis SBMT snapshot; refusing to start: {error}"
                            )
                        })?;

                        let version = tree.version();
                        let root = tree.root_hash();
                        consensus_state.update_jmt_root(version, root);
                        info!(
                            target: "n42::cli",
                            version,
                            %root,
                            accounts = alloc_len,
                            "SBMT seeded from genesis alloc"
                        );
                    } else {
                        // Restored from snapshot + WAL — publish the recovered root.
                        let version = tree.version();
                        let root = tree.root_hash();
                        consensus_state.update_jmt_root(version, root);
                        info!(
                            target: "n42::cli",
                            version,
                            %root,
                            "SBMT restored from snapshot/WAL"
                        );
                    }
                }

                if let Some(ref twig) = twig {
                    let mut tree = twig.lock().unwrap_or_else(|e| {
                        warn!(target: "n42::cli", "Twig mutex poisoned during genesis seed, recovering state");
                        e.into_inner()
                    });

                    // F6: seed genesis only on a truly fresh start.
                    // `restored_from_disk` guards the append-slot idempotency
                    // trap — genesis seeding does not bump the version, so a
                    // restart before the first block commits would otherwise
                    // reseed on top of restored state and diverge the
                    // (append-ordered) root from every un-restarted node.
                    if tree.version() == 0 && !tree.restored_from_disk() {
                        let chain_spec = full_node.provider.chain_spec();
                        let alloc_len = chain_spec.genesis.alloc.len();
                        for (address, account) in &chain_spec.genesis.alloc {
                            let code_hash = account
                                .code
                                .as_ref()
                                .filter(|code| !code.is_empty())
                                .map(|code| keccak256(code.as_ref()))
                                .unwrap_or(EMPTY_CODE_HASH);
                            let storage = account
                                .storage_slots()
                                .map(|(slot, value)| (U256::from_be_bytes(slot.0), value));

                            tree.inner_mut().seed_genesis_account(
                                *address,
                                account.balance,
                                account.nonce.unwrap_or_default(),
                                code_hash,
                                storage,
                            );
                        }

                        tree.flush().map_err(|error| {
                            eyre::eyre!(
                                "failed to persist genesis Twig snapshot; refusing to start: {error}"
                            )
                        })?;

                        let version = tree.version();
                        let root = tree.root_hash();
                        consensus_state.update_twig_root(version, root);
                        info!(
                            target: "n42::cli",
                            version,
                            %root,
                            accounts = alloc_len,
                            "Twig seeded from genesis alloc"
                        );
                    } else {
                        let version = tree.version();
                        let root = tree.root_hash();
                        consensus_state.update_twig_root(version, root);
                        info!(
                            target: "n42::cli",
                            version,
                            %root,
                            "Twig restored from snapshot/WAL"
                        );
                    }
                }

                info!(
                    target: "n42::cli",
                    validator_index = my_index,
                    validator_count = startup_validator_set.len(),
                    %genesis_hash,
                    %head_block_hash,
                    "starting N42 consensus subsystem"
                );

                // ── Observer mode ──────────────────────────────────────
                if observer_mode {
                    info!(target: "n42::cli", "Starting in OBSERVER mode (no consensus participation)");

                    // gov5's QMDB-backed genesis state root can differ from the
                    // local Reth/MPT block-0 root even when both clients load
                    // the same custom-chain JSON. Keep the EL genesis local,
                    // but allow the read-only observer's authenticated H2
                    // domain and fork-digest topic to use gov5's chain identity.
                    let configured_interop_genesis_hash =
                        std::env::var("N42_INTEROP_GENESIS_HASH").ok();
                    let interop_genesis_hash = resolve_observer_genesis_hash(
                        genesis_hash,
                        configured_interop_genesis_hash.as_deref(),
                    )?;
                    if interop_genesis_hash != genesis_hash {
                        info!(
                            target: "n42::cli",
                            local_execution_genesis_hash = %genesis_hash,
                            gov5_interop_genesis_hash = %interop_genesis_hash,
                            "using explicit gov5 chain identity for observer topics and H2 domains"
                        );
                    }

                    let mut qmdb_checkpoint = None;
                    if let Ok(path) = std::env::var("N42_QMDB_BOOTSTRAP") {
                        let expected_block =
                            required_observer_env::<u64>("N42_QMDB_BOOTSTRAP_BLOCK")?;
                        let expected_block_hash = required_observer_env::<B256>(
                            "N42_QMDB_BOOTSTRAP_BLOCK_HASH",
                        )?;
                        let expected_root =
                            required_observer_env::<B256>("N42_QMDB_BOOTSTRAP_ROOT")?;
                        let checkpoint = verify_observer_qmdb_bootstrap(
                            Path::new(&path),
                            full_node.provider.chain_spec().chain().id(),
                            interop_genesis_hash,
                            expected_block,
                            expected_block_hash,
                            expected_root,
                        )?;
                        info!(
                            target: "n42::cli",
                            path,
                            block = checkpoint.block_number,
                            block_hash = %B256::from(checkpoint.block_hash),
                            root = %B256::from(checkpoint.root),
                            slots = checkpoint.next_slot,
                            live = checkpoint.live_count,
                            "verified gov5 replay-v2 QMDB observer bootstrap"
                        );
                        qmdb_checkpoint = Some(checkpoint);
                    }

                    if let Ok(path) = std::env::var("N42_FINALIZED_RANGE_BOOTSTRAP") {
                        let checkpoint = qmdb_checkpoint.as_ref().ok_or_else(|| {
                            eyre::eyre!(
                                "N42_QMDB_BOOTSTRAP is required with N42_FINALIZED_RANGE_BOOTSTRAP"
                            )
                        })?;
                        let file = File::open(&path).map_err(|error| {
                            eyre::eyre!("failed to open finalized range {path}: {error}")
                        })?;
                        let range = verify_finalized_range_stream(
                            BufReader::new(file),
                            full_node.provider.chain_spec().chain().id(),
                            interop_genesis_hash,
                        )?;
                        ensure_finalized_range_matches_qmdb(&range, checkpoint)?;
                        info!(
                            target: "n42::cli",
                            path,
                            from = range.from_block,
                            to = range.to_block,
                            blocks = range.block_count,
                            head = %range.last_block_hash,
                            state_root = %range.last_state_root,
                            receipts_root = %range.last_receipts_root,
                            "verified gov5 finalized range against QMDB bootstrap"
                        );
                    }

                    let keypair = libp2p::identity::Keypair::generate_ed25519();
                    let local_peer_id = keypair.public().to_peer_id();
                    info!(target: "n42::cli", %local_peer_id, "Observer P2P identity (random)");

                    let mut transport_config =
                        TransportConfig::for_network_size(startup_validator_set.len() as usize);
                    transport_config.enable_mdns = env_bool("N42_ENABLE_MDNS");
                    transport_config.enable_kademlia = env_bool("N42_ENABLE_DHT");

                    let swarm = build_interop_observer_swarm(keypair, transport_config)
                        .map_err(|e| eyre::eyre!("failed to build observer libp2p swarm: {e}"))?;

                    let (mut net_service, net_handle, _consensus_event_rx, net_event_rx) =
                        NetworkService::new_gov5_h2_observer(
                            swarm,
                            full_node.provider.chain_spec().chain().id(),
                            interop_genesis_hash,
                        )
                            .map_err(|e| eyre::eyre!("failed to create observer network service: {e}"))?;

                    let consensus_port: u16 = env_parse("N42_CONSENSUS_PORT").unwrap_or(9400);
                    let listen_addr: libp2p::Multiaddr = format!(
                        "/ip4/0.0.0.0/udp/{}/quic-v1",
                        consensus_port
                    )
                    .parse()
                    .map_err(|e| eyre::eyre!("failed to parse observer listen address: {e}"))?;

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

                    // Register node-side hooks the consensus-service driver calls
                    // back into (ingest virtual-block-credit arming).
                    n42_node::register_consensus_service_hooks();
                    let observer_el: std::sync::Arc<dyn n42_node::el::ExecutionLayer> =
                        std::sync::Arc::new(n42_node::el::RethExecutionLayer::engine_only(
                            beacon_engine_handle,
                        ));
                    let mut observer = ObserverOrchestrator::new(
                        net_handle,
                        net_event_rx,
                        observer_el,
                        head_block_hash,
                    )
                    .with_validator_set(initial_validator_set.clone())
                    .with_blob_store(std::sync::Arc::new(n42_node::blob_port::DiskBlobStorePort(
                        full_node.pool.blob_store().clone(),
                    )))
                    .with_exec_output_cache(std::sync::Arc::new(
                        n42_node::exec_cache::RethExecutionOutputCache,
                    ));
                    if let Some(schedule) = epoch_schedule.clone() {
                        observer =
                            observer.with_epoch_schedule(consensus_config.epoch_length, schedule);
                    }

                    task_executor.spawn_critical_task(
                        "n42-observer-orchestrator",
                        Box::pin(observer.run()),
                    );

                    info!(target: "n42::cli", "Observer node started — EL sync via reth eth P2P, CL data via GossipSub");
                    return Ok(());
                }

                // ── Normal consensus mode ──────────────────────────────
                let local_expected_peer_id = resolved_my_index
                    .and_then(|index| configured_validator_peer_ids.get(&index).copied());
                let keypair = resolve_validator_p2p_keypair(my_index, local_expected_peer_id)?;
                let local_peer_id = keypair.public().to_peer_id();
                if local_expected_peer_id.is_some() {
                    info!(target: "n42::cli", %local_peer_id, "P2P identity (configured)");
                } else {
                    info!(target: "n42::cli", %local_peer_id, "P2P identity (legacy deterministic fallback)");
                }

                let mut transport_config =
                    TransportConfig::for_network_size(startup_validator_set.len() as usize);
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
                        .map_err(|e| eyre::eyre!("failed to build consensus libp2p swarm: {e}"))?;

                let (mut net_service, net_handle, consensus_event_rx, net_event_rx) =
                    NetworkService::new_with_expected_validator_peer_ids(
                        swarm,
                        expected_validator_peer_ids.clone(),
                        allow_deterministic_validator_peers,
                    )
                        .map_err(|e| eyre::eyre!("failed to create consensus network service: {e}"))?;

                let consensus_port: u16 = env_parse("N42_CONSENSUS_PORT").unwrap_or(9400);
                let listen_addr: libp2p::Multiaddr = format!(
                    "/ip4/0.0.0.0/udp/{}/quic-v1",
                    consensus_port
                )
                .parse()
                .map_err(|e| eyre::eyre!("failed to parse consensus listen address: {e}"))?;

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

                // Normal mode: connect to higher-index only (i < j avoids duplicate connections).
                // Joining mode (key not in initial set): connect to ALL existing validators
                // since no existing node will initiate a connection to us.
                let val_count = startup_validator_set.len() as u32;
                if !env_bool("N42_NO_AUTO_CONNECT") && val_count > 1 {
                    let base_port = consensus_port.saturating_sub(my_index as u16);
                    let connect_range = if resolved_my_index.is_none() {
                        0..val_count
                    } else {
                        (my_index + 1)..val_count
                    };
                    let mode = if resolved_my_index.is_none() { "all (joining)" } else { "higher-index" };
                    info!(
                        target: "n42::cli",
                        base_port, val_count, my_index, mode,
                        "auto-connecting to validators"
                    );
                    for j in connect_range {
                        let peer_id = if let Some(peer_id) = expected_validator_peer_ids.get(&j) {
                            *peer_id
                        } else {
                            n42_network::deterministic_validator_peer_id(j)?
                        };
                        let peer_port = base_port + j as u16;
                        let peer_addr: libp2p::Multiaddr = match format!(
                            "/ip4/127.0.0.1/udp/{}/quic-v1/p2p/{}",
                            peer_port, peer_id
                        )
                        .parse()
                        {
                            Ok(addr) => addr,
                            Err(error) => {
                                warn!(target: "n42::cli", error = %error, validator = j, "failed to build auto-connect multiaddr");
                                continue;
                            }
                        };

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
                    }).map_err(|e| eyre::eyre!("failed to create ShardedStarHub: {e}"))?;

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
                let (attest_progress_tx, mut attest_progress_rx) = mpsc::channel(256);
                let (phone_connected_tx, phone_connected_rx) = mpsc::channel(128);
                let (reward_attest_tx, mut reward_attest_rx) = mpsc::channel(1024);
                let (dispatched_block_tx, dispatched_block_rx) = mpsc::channel(128);

                let attestation_store_path = data_dir.join("attestation_store.json");
                let attestation_store = Arc::new(Mutex::new(
                    AttestationStore::new(attestation_store_path)
                        .map_err(|e| eyre::eyre!("failed to initialize attestation store: {e}"))?,
                ));

                let mobile_bridge = MobileVerificationBridge::new(
                    hub_event_rx,
                    configured_min_mobile_verifiers(),
                    1000,
                )
                    .with_attestation_tx(attest_tx)
                    .with_attestation_progress_tx(attest_progress_tx)
                    .with_phone_connected_tx(phone_connected_tx)
                    .with_consensus_state(consensus_state.clone())
                    .with_reward_tx(reward_attest_tx)
                    .with_staking_manager(staking_manager.clone())
                    .with_attestation_store(attestation_store.clone())
                    .with_dispatched_block_rx(dispatched_block_rx);

                task_executor.spawn_critical_task("n42-mobile-bridge", Box::pin(mobile_bridge.run()));

                let reward_mgr_tracker = reward_manager.clone();
                task_executor.spawn_critical_task(
                    "n42-reward-tracker",
                    Box::pin(async move {
                        while let Some(pubkey) = reward_attest_rx.recv().await {
                            let mut mgr = reward_mgr_tracker.lock().unwrap_or_else(|e| {
                                warn!(target: "n42::mobile", "reward manager mutex poisoned, recovering state");
                                e.into_inner()
                            });
                            mgr.record_attestation(&pubkey);
                        }
                    }),
                );

                let attestation_state = consensus_state.clone();
                let evidence_store_for_mobile = evidence_store.clone();
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
                            if let Some(ref evidence_store) = evidence_store_for_mobile {
                                spawn_mobile_evidence_writeback(
                                    Arc::clone(evidence_store),
                                    event,
                                );
                            }
                        }
                    }),
                );

                task_executor.spawn_critical_task(
                    "n42-attestation-progress",
                    Box::pin(async move {
                        while let Some(event) = attest_progress_rx.recv().await {
                            debug!(
                                target: "n42::mobile",
                                block_number = event.block_number,
                                %event.block_hash,
                                valid_count = event.valid_count,
                                threshold = event.threshold,
                                reached = event.valid_count >= event.threshold,
                                receipts_root = %alloy_primitives::B256::from(event.receipts_root),
                                "mobile attestation progress"
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
                        Arc::clone(&twig_sidecar_healthy),
                        phone_connected_rx,
                        Some(dispatched_block_tx),
                    )),
                );
                info!(target: "n42::cli", "Mobile packet generation loop started");

                let (output_tx, output_rx) = mpsc::channel(if low_mem { 64 } else { 1024 });

                let mut restored_block_count: u64 = 0;
                let consensus_engine = if let Some(snapshot) = snapshot.clone() {
                    restored_block_count = snapshot.committed_block_count;
                    info!(
                        target: "n42::cli",
                        view = snapshot.current_view,
                        locked_qc_view = snapshot.locked_qc.view,
                        last_committed_view = snapshot.last_committed_qc.view,
                        committed_block_count = snapshot.committed_block_count,
                        "recovered consensus state from snapshot"
                    );
                    let mut epoch_manager = build_epoch_manager(
                        &initial_validator_set,
                        &initial_validator_infos,
                        consensus_config.epoch_length,
                        consensus_config.max_historical_epochs,
                        epoch_schedule.as_ref(),
                        Some(snapshot.current_view),
                        snapshot.current_epoch_validators.as_ref(),
                    )?;
                    if let Some((target_epoch, ref validators, f)) =
                        snapshot.scheduled_epoch_transition
                    {
                        let expected_epoch = epoch_manager.current_epoch() + 1;
                        let legacy_snapshot_epoch = snapshot
                            .current_epoch_validators
                            .as_ref()
                            .map(|(epoch, _, _)| *epoch);
                        let legacy_retarget = target_epoch != expected_epoch
                            && legacy_snapshot_epoch.is_some_and(|epoch| {
                                target_epoch == epoch + 1
                                    && epoch < epoch_manager.current_epoch()
                            });
                        if target_epoch != expected_epoch && !legacy_retarget {
                            warn!(
                                target: "n42::cli",
                                target_epoch,
                                expected_epoch,
                                "discarding staged epoch transition from snapshot with mismatched target epoch"
                            );
                        } else if let Err(error) = epoch_manager.stage_next_epoch(validators, f) {
                            warn!(
                                target: "n42::cli",
                                target_epoch,
                                error = %error,
                                "discarding invalid staged epoch transition from snapshot"
                            );
                        } else {
                            if legacy_retarget {
                                warn!(
                                    target: "n42::cli",
                                    snapshot_target_epoch = target_epoch,
                                    migrated_target_epoch = expected_epoch,
                                    "retargeted legacy staged epoch transition to the next view-range epoch"
                                );
                            }
                            info!(
                                target: "n42::cli",
                                target_epoch = expected_epoch,
                                new_validators = validators.len(),
                                "restored staged epoch transition from snapshot"
                            );
                        }
                    }
                    // Take the maximum of snapshot and disk: the disk vote_log
                    // may be ahead if the snapshot was written before the latest
                    // vote, while the snapshot may be ahead on a fresh datadir.
                    let lvv = snapshot.last_voted_view.max(last_voted_view_from_disk);
                    let lcvv = snapshot
                        .last_commit_voted_view
                        .max(last_commit_voted_view_from_disk);
                    ConsensusEngine::with_recovered_state_and_vote_log(
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
                        lvv,
                        lcvv,
                        vote_log.clone(),
                    )
                } else {
                    if last_voted_view_from_disk != 0 || last_commit_voted_view_from_disk != 0 {
                        return Err(eyre::eyre!(
                            "vote log records R1 view {} / R2 view {} but no consensus snapshot exists; refusing unsafe fresh start",
                            last_voted_view_from_disk,
                            last_commit_voted_view_from_disk
                        ));
                    }
                    info!(target: "n42::cli", "no consensus snapshot found, starting fresh");
                    ConsensusEngine::with_epoch_manager_and_vote_log(
                        my_index,
                        secret_key,
                        build_epoch_manager(
                            &initial_validator_set,
                            &initial_validator_infos,
                            consensus_config.epoch_length,
                            consensus_config.max_historical_epochs,
                            epoch_schedule.as_ref(),
                            None,
                            None,
                        )?,
                        consensus_config.base_timeout_ms,
                        consensus_config.max_timeout_ms,
                        output_tx,
                        vote_log.clone(),
                    )
                };

                let tx_chan_size = if low_mem { 256 } else { 4096 };
                let (tx_import_tx, tx_import_rx) =
                    mpsc::channel::<n42_node::tx_bridge::TxImportBatch>(tx_chan_size);
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
                        // Keep the bridge runtime alive for the process lifetime. Dropping a
                        // Tokio runtime from this shutdown path can panic because runtime drop
                        // performs blocking teardown.
                        let rt: &'static _ = match tokio::runtime::Builder::new_multi_thread()
                            .worker_threads(bridge_workers)
                            .thread_name("tx-bridge-worker")
                            .enable_all()
                            .build()
                        {
                            Ok(rt) => Box::leak(Box::new(rt)),
                            Err(error) => {
                                tracing::error!(error = %error, "failed to create tx-bridge runtime");
                                return;
                            }
                        };
                        rt.block_on(
                            TxPoolBridge::new(pool_for_bridge, tx_import_rx, tx_broadcast_tx)
                                .run(),
                        );
                    })
                    .map_err(|e| eyre::eyre!("failed to spawn tx-bridge thread: {e}"))?;
                info!(target: "n42::cli", "TxPoolBridge started on dedicated runtime");

                // Sample txpool depth into SharedConsensusState so commit/build_start
                // cadence logs can show whether the leader was actually fed.
                let pool_for_depth = full_node.pool.clone();
                let pool_depth_state = consensus_state.clone();
                let pool_depth_poll_ms = env_parse::<u64>("N42_POOL_DEPTH_POLL_MS")
                    .unwrap_or(100)
                    .max(10);
                task_executor.spawn_critical_task(
                    "n42-pool-depth-sampler",
                    Box::pin(async move {
                        let mut interval =
                            tokio::time::interval(Duration::from_millis(pool_depth_poll_ms));
                        loop {
                            let size = pool_for_depth.pool_size();
                            pool_depth_state.update_pool_depth(size.pending, size.queued);
                            interval.tick().await;
                        }
                    }),
                );

                // Binary TCP ingest server for high-speed local TX submission.
                // Enable with N42_INGEST_PORT=19900. Legacy fallback: N42_INJECT_PORT.
                if std::env::var("N42_INGEST_PORT").is_ok() || std::env::var("N42_INJECT_PORT").is_ok() {
                    let inject_pool = full_node.pool.clone();
                    task_executor.spawn_critical_task(
                        "n42-ingest-server",
                        Box::pin(n42_node::ingest::run_ingest_server(inject_pool)),
                    );
                }

                // Register node-side hooks the consensus-service driver calls back
                // into (ingest virtual-block-credit arming).
                n42_node::register_consensus_service_hooks();

                let el: std::sync::Arc<dyn n42_node::el::ExecutionLayer> = std::sync::Arc::new(
                    n42_node::el::RethExecutionLayer::new(
                        beacon_engine_handle,
                        payload_builder_handle,
                    ),
                );
                let mut orchestrator = ConsensusOrchestrator::with_execution_layer(
                    consensus_engine,
                    std::sync::Arc::new(net_handle),
                    consensus_event_rx,
                    net_event_rx,
                    output_rx,
                    el,
                    consensus_state,
                    head_block_hash,
                    fee_recipient,
                )
                .with_tx_pool_bridge(tx_import_tx, tx_broadcast_rx)
                .with_mobile_packet_tx(mobile_packet_tx)
                .with_state_persistence(state_file)
                .with_validator_set(startup_validator_set)
                .with_blob_store(std::sync::Arc::new(n42_node::blob_port::DiskBlobStorePort(
                    full_node.pool.blob_store().clone(),
                )))
                .with_defer_state_root(n42_node::defer_state_root_enabled())
                .with_exec_output_cache(std::sync::Arc::new(
                    n42_node::exec_cache::RethExecutionOutputCache,
                ))
                .with_staking_sink(std::sync::Arc::new(
                    n42_node::sinks::ManagerStakingSink(staking_manager.clone()),
                ))
                .with_withdrawal_source(std::sync::Arc::new(
                    n42_node::sinks::NodeWithdrawalSource {
                        reward: Some(reward_manager),
                        staking: Some(staking_manager.clone()),
                    },
                ))
                .with_committed_block_count(restored_block_count);

                // Restore prev_randao derivation from snapshot's last committed QC.
                // Without this, first payload after crash recovery uses B256::ZERO.
                if let Some(ref snap) = snapshot {
                    orchestrator = orchestrator
                        .with_recovered_commit_qc(snap.last_committed_qc.clone());

                    // T1: restore a view that is cryptographically/structurally
                    // tied to reth's canonical hash. Never pair a guessed lower
                    // view with a newer canonical hash: doing so would re-open
                    // the backward-FCU window during the first sync response.
                    let evidence_head = evidence_store.as_ref().and_then(|store| {
                        match store.get(best_block_number) {
                            Ok(Some(evidence)) => Some((
                                evidence.view,
                                alloy_primitives::B256::from(evidence.block_hash),
                            )),
                            Ok(None) => None,
                            Err(error) => {
                                warn!(
                                    target: "n42::cli",
                                    best_block = best_block_number,
                                    %error,
                                    "failed to read canonical-head consensus evidence"
                                );
                                None
                            }
                        }
                    });
                    let (seeded_view, recovery_source) =
                        persistence::recover_execution_validated_head_view(
                            snap,
                            head_block_hash,
                            best_block_number,
                            evidence_head,
                        )
                        .map_err(|error| {
                            eyre::eyre!(
                                "refusing to start consensus with an unproven execution-head view: {error}"
                            )
                        })?;
                    info!(
                        target: "n42::cli",
                        seeded_view,
                        recovery_source,
                        snapshot_validated_view = snap.execution_validated_head_view,
                        snapshot_validated_hash = %snap.execution_validated_head_hash,
                        %head_block_hash,
                        "restored execution-validated head guard from snapshot"
                    );
                    orchestrator =
                        orchestrator.with_recovered_execution_validated_head(seeded_view);
                }

                if let Some(schedule) = epoch_schedule {
                    orchestrator = orchestrator.with_epoch_schedule(schedule);
                }
                orchestrator = orchestrator
                    .with_allow_deterministic_validator_peers(allow_deterministic_validator_peers);
                if let Some(ref jmt) = jmt {
                    orchestrator = orchestrator.with_jmt(std::sync::Arc::new(
                        n42_node::sinks::SbmtStateSink(Arc::clone(jmt)),
                    ));
                }
                if let Some(ref twig) = twig {
                    let probe_config = n42_node::sinks::TwigProbeConfig::from_env();
                    info!(
                        target: "n42::cli",
                        interval = probe_config.interval,
                        accounts = probe_config.accounts_per_sample,
                        "Twig/reth exact-block consistency sampling enabled"
                    );
                    orchestrator = orchestrator.with_twig(std::sync::Arc::new(
                        n42_node::sinks::TwigStateSink::with_reth_probe(
                            Arc::clone(twig),
                            full_node.provider.clone(),
                            Arc::clone(&twig_sidecar_healthy),
                            probe_config,
                        ),
                    ));
                }
                if let Some(scheduler) = zk_scheduler {
                    orchestrator = orchestrator.with_zk_scheduler(std::sync::Arc::new(
                        n42_node::sinks::SchedulerZkSink(scheduler),
                    ));
                }
                if let Some(rx) = admin_rx.lock().unwrap_or_else(|e| e.into_inner()).take() {
                    orchestrator = orchestrator.with_admin_rx(rx);
                }
                if let Some(ref store) = evidence_store {
                    orchestrator = orchestrator.with_evidence_store(Arc::clone(store));
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

#[cfg(test)]
mod observer_identity_tests {
    use super::*;
    use std::io::Write;

    fn qmdb_portable_fixture() -> tempfile::NamedTempFile {
        let vector: serde_json::Value = serde_json::from_str(include_str!(
            "../../../crates/n42-twig-core/testdata/cross_client_v1.json"
        ))
        .unwrap();
        let encoded = vector["portable"]["hex"].as_str().unwrap();
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&hex::decode(encoded).unwrap()).unwrap();
        file.flush().unwrap();
        file
    }

    #[test]
    fn observer_genesis_hash_defaults_to_local_execution_genesis() {
        let local = B256::repeat_byte(0x11);
        assert_eq!(resolve_observer_genesis_hash(local, None).unwrap(), local);
    }

    #[test]
    fn observer_genesis_hash_accepts_explicit_gov5_identity() {
        let configured = B256::repeat_byte(0x22);
        let encoded = format!("{configured:#x}");

        assert_eq!(
            resolve_observer_genesis_hash(B256::ZERO, Some(&encoded)).unwrap(),
            configured
        );
    }

    #[test]
    fn observer_genesis_hash_rejects_malformed_identity() {
        let error = resolve_observer_genesis_hash(B256::ZERO, Some("0x1234")).unwrap_err();
        assert!(error.to_string().contains("N42_INTEROP_GENESIS_HASH"));
    }

    #[test]
    fn observer_qmdb_bootstrap_accepts_exact_checkpoint_identity() {
        let fixture = qmdb_portable_fixture();
        let genesis_hash = "0x1100000000000000000000000000000000000000000000000000000000000000"
            .parse()
            .unwrap();
        let block_hash = "0x2200000000000000000000000000000000000000000000000000000000000000"
            .parse()
            .unwrap();
        let root = "0xbd6c73b724bc0a38c7efa81c2088bfc805d12faa6d12f188b714ebd1e717c646"
            .parse()
            .unwrap();

        let checkpoint = verify_observer_qmdb_bootstrap(
            fixture.path(),
            1143,
            genesis_hash,
            42,
            block_hash,
            root,
        )
        .unwrap();

        assert_eq!(checkpoint.block_number, 42);
        assert_eq!(checkpoint.next_slot, 3);
        assert_eq!(checkpoint.live_count, 1);
    }

    #[test]
    fn observer_qmdb_bootstrap_rejects_mismatched_checkpoint_metadata() {
        let fixture = qmdb_portable_fixture();
        let genesis_hash = "0x1100000000000000000000000000000000000000000000000000000000000000"
            .parse()
            .unwrap();
        let block_hash = "0x2200000000000000000000000000000000000000000000000000000000000000"
            .parse()
            .unwrap();

        let error = verify_observer_qmdb_bootstrap(
            fixture.path(),
            1143,
            genesis_hash,
            42,
            block_hash,
            B256::repeat_byte(0xff),
        )
        .unwrap_err();

        assert!(error.to_string().contains("N42_QMDB_BOOTSTRAP_ROOT"));
    }

    #[test]
    fn finalized_range_must_end_at_qmdb_checkpoint() {
        let hash = B256::repeat_byte(0x22);
        let root = B256::repeat_byte(0x33);
        let range = FinalizedRangeVerification {
            chain_id: 1143,
            genesis_hash: B256::repeat_byte(0x11),
            from_block: 7,
            to_block: 9,
            block_count: 3,
            first_parent_hash: B256::repeat_byte(0x06),
            last_block_hash: hash,
            last_state_root: root,
            last_receipts_root: B256::repeat_byte(0x44),
        };
        let checkpoint = QmdbPortableVerification {
            chain_id: 1143,
            genesis_hash: [0x11; 32],
            block_number: 9,
            block_hash: hash.into(),
            root: root.into(),
            next_slot: 10,
            live_count: 8,
        };
        ensure_finalized_range_matches_qmdb(&range, &checkpoint).unwrap();

        let mut wrong = range;
        wrong.last_state_root = B256::ZERO;
        assert!(ensure_finalized_range_matches_qmdb(&wrong, &checkpoint).is_err());
    }
}
