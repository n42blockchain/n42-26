//! Standalone consensus binary — runs the `n42-consensus-service` `ConsensusService`
//! against a REMOTE execution layer over the Engine API ([`EngineApiRpcExecutionLayer`]),
//! instead of the in-process reth handles `bin/n42-node` uses. This is the
//! standalone half of Caplin's dual-mode: the SAME `ConsensusService` driver, wired
//! to an EL in a separate process.
//!
//! In standalone mode all in-process sinks (state-tree / ZK / blob / exec-output
//! cache) are absent — those are the in-process node's job; the standalone consensus
//! only drives consensus + the remote EL.
//!
//! Status: the EL adapter + service composition are real and unit/smoke-tested. The
//! full libp2p swarm bring-up + chainspec/validator-set loading + `service.run()`
//! against a live `n42-node` Engine API is the **post-datc cross-process E2E** step
//! (TODO below) — deliberately left as wiring so this compiles and is testable now.

use std::sync::Arc;

use alloy_primitives::{Address, B256};
use n42_chainspec::ValidatorInfo;
use n42_consensus::{ConsensusEngine, EngineOutput, ValidatorSet};
use n42_consensus_service::ConsensusService;
use n42_consensus_service::consensus_state::SharedConsensusState;
use n42_consensus_service::el::ExecutionLayer;
use n42_consensus_service::net_port::ConsensusNetwork;
use n42_el_rpc::EngineApiRpcExecutionLayer;
use n42_network::{NetworkEvent, NetworkHandle};
use n42_primitives::BlsSecretKey;
use tokio::sync::mpsc;

/// Minimal CLI config for the standalone consensus binary.
struct Config {
    engine_url: String,
    jwt_secret_path: String,
}

impl Config {
    /// Parses `--engine-url <URL>` and `--jwt-secret <PATH>` (with env fallbacks
    /// `N42_ENGINE_URL` / `N42_JWT_SECRET`).
    fn from_args() -> Result<Self, String> {
        let mut engine_url = std::env::var("N42_ENGINE_URL").ok();
        let mut jwt_secret_path = std::env::var("N42_JWT_SECRET").ok();
        let mut args = std::env::args().skip(1);
        while let Some(a) = args.next() {
            match a.as_str() {
                "--engine-url" => engine_url = args.next(),
                "--jwt-secret" => jwt_secret_path = args.next(),
                "-h" | "--help" => {
                    println!(
                        "n42-consensus-standalone --engine-url <http://host:8551> --jwt-secret <path>\n\
                         \n  Runs ConsensusService against a remote Engine-API EL.\n\
                         \n  Env fallbacks: N42_ENGINE_URL, N42_JWT_SECRET."
                    );
                    std::process::exit(0);
                }
                other => return Err(format!("unknown arg: {other}")),
            }
        }
        Ok(Self {
            engine_url: engine_url.ok_or("missing --engine-url / N42_ENGINE_URL")?,
            jwt_secret_path: jwt_secret_path.ok_or("missing --jwt-secret / N42_JWT_SECRET")?,
        })
    }
}

/// All the channels + handles the standalone runtime owns alongside the service.
/// Fields are held to keep the service + its network channels alive for the
/// (TODO) `service.run()` path; nothing reads them in the current skeleton.
#[allow(dead_code)]
struct ServiceParts {
    service: ConsensusService,
    // Held so the channels stay open; a real run wires these to a libp2p swarm.
    _consensus_event_tx: mpsc::Sender<NetworkEvent>,
    _net_event_tx: mpsc::Sender<NetworkEvent>,
    _net_cmd_rx: mpsc::Receiver<n42_network::NetworkCommand>,
    _priority_cmd_rx: mpsc::Receiver<n42_network::NetworkCommand>,
}

/// Wires a [`ConsensusService`] against the remote EL with all sinks = None. The
/// network is a bare `NetworkHandle` over command channels (a real run hands the
/// receivers to a libp2p `NetworkService`); here they are returned so the caller
/// owns them.
fn build_service(
    my_index: u32,
    secret_key: BlsSecretKey,
    validators: Vec<ValidatorInfo>,
    fault_tolerance: u32,
    el: Arc<dyn ExecutionLayer>,
    head_block_hash: B256,
    fee_recipient: Address,
) -> ServiceParts {
    let validator_set = ValidatorSet::new(&validators, fault_tolerance);

    let (output_tx, output_rx) = mpsc::channel::<EngineOutput>(1024);
    let engine = ConsensusEngine::new(
        my_index,
        secret_key,
        validator_set.clone(),
        60_000,
        120_000,
        output_tx,
    );

    let (cmd_tx, net_cmd_rx) = mpsc::channel(1024);
    let (priority_tx, priority_cmd_rx) = mpsc::channel(1024);
    let network: Arc<dyn ConsensusNetwork> = Arc::new(NetworkHandle::new(cmd_tx, priority_tx));

    let (consensus_event_tx, consensus_event_rx) = mpsc::channel::<NetworkEvent>(1024);
    let (net_event_tx, net_event_rx) = mpsc::channel::<NetworkEvent>(8192);

    let consensus_state = Arc::new(SharedConsensusState::new(validator_set));

    let service = ConsensusService::with_execution_layer(
        engine,
        network,
        consensus_event_rx,
        net_event_rx,
        output_rx,
        el,
        consensus_state,
        head_block_hash,
        0,
        fee_recipient,
    );

    ServiceParts {
        service,
        _consensus_event_tx: consensus_event_tx,
        _net_event_tx: net_event_tx,
        _net_cmd_rx: net_cmd_rx,
        _priority_cmd_rx: priority_cmd_rx,
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cfg = Config::from_args().map_err(|e| {
        eprintln!("config error: {e}\n  try --help");
        e
    })?;

    let el = Arc::new(EngineApiRpcExecutionLayer::from_secret_file(
        cfg.engine_url.clone(),
        &cfg.jwt_secret_path,
    )?);

    tracing::info!(
        engine_url = %cfg.engine_url,
        "standalone consensus: remote Engine-API ExecutionLayer ready"
    );

    // Dev single-validator wiring that exercises the real construction path: build
    // the `ConsensusService` against the remote EL with all sinks = None. A real
    // deployment loads the BLS key + validator set + chainspec from config.
    // TODO(post-datc E2E): replace the dev key/validator set with config loading,
    // bring up the libp2p `NetworkService` swarm, hand it the command receivers +
    // feed `NetworkEvent`s into the event senders, then `service.run().await`.
    // Left as wiring because it cannot be validated without a live peer set + EL —
    // the cross-process E2E step against `n42-node`'s Engine API.
    let secret_key = BlsSecretKey::key_gen(&[0x42; 32]).expect("dev key");
    let self_info = ValidatorInfo {
        address: Address::ZERO,
        bls_public_key: secret_key.public_key(),
        p2p_peer_id: None,
    };
    let parts = build_service(
        0,
        secret_key,
        vec![self_info],
        0,
        el as Arc<dyn ExecutionLayer>,
        B256::ZERO,
        Address::ZERO,
    );
    tracing::warn!(
        "standalone consensus skeleton: ConsensusService constructed against the remote \
         EL; full swarm bring-up + service.run() is the post-datc cross-process E2E step"
    );
    drop(parts);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_rpc_types_engine::JwtSecret;

    fn test_validator(seed: u8) -> (BlsSecretKey, ValidatorInfo) {
        let sk = BlsSecretKey::key_gen(&[seed; 32]).expect("deterministic key");
        let info = ValidatorInfo {
            address: Address::with_last_byte(seed),
            bls_public_key: sk.public_key(),
            p2p_peer_id: None,
        };
        (sk, info)
    }

    #[test]
    fn build_service_composes_with_remote_el() {
        // The remote EL adapter satisfies `Arc<dyn ExecutionLayer>` and the
        // standalone wiring builds a `ConsensusService` from it — proving the
        // dual-mode composition compiles + constructs (no network/EL I/O).
        let (sk, info) = test_validator(0x42);
        let el: Arc<dyn ExecutionLayer> = Arc::new(EngineApiRpcExecutionLayer::new(
            "http://127.0.0.1:8551",
            JwtSecret::random(),
        ));
        let parts = build_service(0, sk, vec![info], 0, el, B256::ZERO, Address::ZERO);
        // Holding ServiceParts keeps the channels open; dropping it is the clean
        // shutdown. Construction succeeding is the assertion.
        drop(parts);
    }
}
