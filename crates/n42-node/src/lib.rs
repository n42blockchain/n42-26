pub mod attestation_store;
pub mod blob_port;
mod components;
pub mod el;
pub mod exec_cache;
pub mod ingest;
pub mod mobile_bridge;
pub mod mobile_evidence;
pub mod mobile_packet;
pub mod mobile_reward;
pub use n42_consensus_service::net_port;
mod node;
pub mod packet_builder;
pub mod payload;
pub mod pool;
pub mod rpc;
pub mod sinks;
pub mod staking;
pub mod tx_bridge;

// The consensus driver + its port traits + reth-free supporting types now live
// in the `n42-consensus-service` crate (Caplin stage 6). Re-export the names the
// node + binary still reference so callers compile unchanged.
pub use n42_consensus_service::consensus_state::{self, SharedConsensusState};
pub use n42_consensus_service::validator_peers::{
    self, configured_validator_peer_ids, expected_validator_peer_ids,
    expected_validator_peer_ids_with_policy,
};
pub use n42_consensus_service::{ConsensusService, epoch_schedule, orchestrator, persistence};

pub use components::{N42ConsensusBuilder, N42ExecutorBuilder};
/// Backwards-compatible alias retained one release while the Caplin EL-seam
/// refactor renames `ConsensusOrchestrator` → [`ConsensusService`] (stage 5).
pub use n42_consensus_service::ConsensusService as ConsensusOrchestrator;
pub use n42_consensus_service::observer::ObserverOrchestrator;
pub use node::N42Node;
pub use payload::N42PayloadBuilder;
pub use pool::N42PoolBuilder;

/// Wires node-side hooks the `n42-consensus-service` driver calls back into. Must
/// be invoked once at node startup. Currently registers the ingest virtual-block-
/// credit arming fn (whose shared state + consume side live in this crate's ingest
/// server) so the driver can arm credits without depending on the node ingest code.
pub fn register_consensus_service_hooks() {
    n42_consensus_service::ingest::set_block_credit_hook(ingest::note_virtual_block_credit);
}

/// Returns the current wall-clock time in milliseconds since the Unix epoch.
///
/// Shared utility used by the orchestrator and ingest subsystems.
pub(crate) fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Whether deferred state-root mode is enabled (env `N42_DEFER_STATE_ROOT`).
/// Re-exported so the node binary can read it once at startup and pass it to the
/// `ConsensusService` without depending on `reth_evm` directly.
pub fn defer_state_root_enabled() -> bool {
    reth_evm::n42_defer_state_root()
}
