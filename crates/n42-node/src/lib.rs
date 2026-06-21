pub mod attestation_store;
mod components;
pub mod consensus_state;
pub mod el;
pub mod epoch_schedule;
pub mod exec_cache;
pub mod ingest;
pub mod mobile_bridge;
pub mod mobile_packet;
pub mod mobile_reward;
pub mod net_port;
mod node;
pub mod orchestrator;
pub mod packet_builder;
pub mod payload;
pub mod persistence;
pub mod pool;
pub mod rpc;
pub mod sinks;
pub mod staking;
pub mod tx_bridge;
pub mod validator_peers;

pub use components::{N42ConsensusBuilder, N42ExecutorBuilder};
pub use consensus_state::SharedConsensusState;
pub use node::N42Node;
pub use orchestrator::ConsensusService;
/// Backwards-compatible alias retained one release while the Caplin EL-seam
/// refactor renames `ConsensusOrchestrator` → [`ConsensusService`] (stage 5).
/// External callers (`bin/n42-node`, RPC) keep compiling unchanged.
pub use orchestrator::ConsensusService as ConsensusOrchestrator;
pub use orchestrator::observer::ObserverOrchestrator;
pub use payload::N42PayloadBuilder;
pub use pool::N42PoolBuilder;
pub use validator_peers::{
    configured_validator_peer_ids, expected_validator_peer_ids,
    expected_validator_peer_ids_with_policy,
};

/// Returns the current wall-clock time in milliseconds since the Unix epoch.
///
/// Shared utility used by the orchestrator and ingest subsystems.
pub(crate) fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
