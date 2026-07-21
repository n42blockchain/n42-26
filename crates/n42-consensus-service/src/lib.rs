//! n42 consensus service — the Caplin-style "decoupled but in-process" consensus
//! driver. Holds the `ConsensusService` orchestrator (HotStuff-2 ↔ EL/network
//! bridge) plus the port traits it depends on (`ExecutionLayer`,
//! `ConsensusNetwork`, `BlobStorePort`, `ExecutionOutputCache`, the sink ports).
//! The concrete adapters over reth handles / libp2p / state trees live in
//! `n42-node`. This crate carries NO hard reth dependency (no reth-node-builder /
//! reth-evm / reth-payload-* / reth-transaction-pool / reth-execution-types); it
//! uses `revm` / `reth-ethereum-primitives::Receipt` / `n42-execution` only as
//! execution-domain type deps. See `docs/task-caplin-stage6-clean-extraction.md`.

pub mod blob_port;
pub mod consensus_state;
pub mod el;
pub mod epoch_schedule;
pub mod exec_cache;
pub mod h2_finality;
pub mod ingest;
pub mod net_port;
pub mod observer;
pub mod orchestrator;
pub mod persistence;
pub mod sinks;
pub mod validator_peers;

pub use observer::ObserverOrchestrator;
pub use orchestrator::ConsensusService;
pub use validator_peers::expected_validator_peer_ids_with_policy;

/// Returns the current wall-clock time in milliseconds since the Unix epoch.
/// Shared utility used by the orchestrator.
pub(crate) fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
