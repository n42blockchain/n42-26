pub mod attestation_store;
pub mod blob_port;
mod components;
pub mod el;
pub mod engine_validator;
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
pub mod qmdb_state;
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

/// Reserved benchmark chain-id range in which the state-root bypass flags
/// (`N42_SKIP_STATE_ROOT` / `N42_DEFER_STATE_ROOT`) may run at all
/// (RFC production-safe-deferred-state-root, Phase A).
pub const BENCH_CHAIN_ID_MIN: u64 = 4_242_400;
/// Exclusive upper bound of the reserved benchmark chain-id range.
pub const BENCH_CHAIN_ID_MAX: u64 = 4_242_500;

/// Phase-A hard guard for the state-root bypass flags: a bypass may run ONLY
/// when the operator opted in explicitly (`N42_ALLOW_BENCH_MODE=1`) AND the
/// chain id sits inside the reserved benchmark range — a production chain can
/// never start with root verification silently disabled (the Go stack ran
/// half a year of "stability" on exactly that silence). Returns the refusal
/// message for the caller to print before exiting non-zero.
pub fn validate_state_root_bypass_flags(
    skip: bool,
    defer: bool,
    allow_bench: bool,
    chain_id: u64,
) -> Result<(), String> {
    if !skip && !defer {
        return Ok(());
    }
    let flags = match (skip, defer) {
        (true, true) => "N42_SKIP_STATE_ROOT+N42_DEFER_STATE_ROOT",
        (true, false) => "N42_SKIP_STATE_ROOT",
        _ => "N42_DEFER_STATE_ROOT",
    };
    if !allow_bench {
        return Err(format!(
            "{flags}=1 disables state-root verification and is refused without N42_ALLOW_BENCH_MODE=1"
        ));
    }
    if !(BENCH_CHAIN_ID_MIN..BENCH_CHAIN_ID_MAX).contains(&chain_id) {
        return Err(format!(
            "{flags}=1 is only allowed on benchmark chain ids [{BENCH_CHAIN_ID_MIN},{BENCH_CHAIN_ID_MAX}); got {chain_id}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod state_root_flag_tests {
    use super::*;

    #[test]
    fn no_flags_always_pass() {
        assert!(validate_state_root_bypass_flags(false, false, false, 1).is_ok());
        assert!(validate_state_root_bypass_flags(false, false, true, 1).is_ok());
    }

    #[test]
    fn bypass_without_allow_refused_everywhere() {
        for chain_id in [1, BENCH_CHAIN_ID_MIN, 94] {
            assert!(validate_state_root_bypass_flags(true, false, false, chain_id).is_err());
            assert!(validate_state_root_bypass_flags(false, true, false, chain_id).is_err());
        }
    }

    #[test]
    fn bypass_with_allow_only_inside_bench_range() {
        assert!(validate_state_root_bypass_flags(false, true, true, BENCH_CHAIN_ID_MIN).is_ok());
        assert!(
            validate_state_root_bypass_flags(true, true, true, BENCH_CHAIN_ID_MAX - 1).is_ok()
        );
        assert!(validate_state_root_bypass_flags(false, true, true, BENCH_CHAIN_ID_MAX).is_err());
        assert!(validate_state_root_bypass_flags(true, false, true, 1).is_err());
        assert!(validate_state_root_bypass_flags(true, false, true, 94).is_err());
    }

    #[test]
    fn refusal_names_the_flags() {
        let err = validate_state_root_bypass_flags(true, true, false, 1).unwrap_err();
        assert!(err.contains("N42_SKIP_STATE_ROOT+N42_DEFER_STATE_ROOT"));
    }
}
