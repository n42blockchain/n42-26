//! Sink ports — trait boundaries over the node-side side-effect consumers the
//! consensus orchestrator drives (state trees, ZK proof sidecar, staking
//! scan/persist, reward/staking withdrawal computation). The concrete adapters
//! (`SbmtStateSink`/`TwigStateSink`/`SchedulerZkSink`/`ManagerStakingSink`/
//! `NodeWithdrawalSource`) live node-side; this crate holds only the traits.
//! Caplin EL-seam refactor (stage 3 / 6c).

use alloy_eips::eip4895::Withdrawal;
use alloy_primitives::B256;
use n42_execution::state_diff::StateDiff;

/// A sidecar state tree (SBMT or Twig) that applies a per-block `StateDiff`.
/// Implementations lock internally and return `(version, root)` so the
/// orchestrator keeps its existing metrics / logs / shared-state callbacks.
pub trait StateSink: Send + Sync {
    /// Apply a state diff, returning `(version, root)`. Locks internally.
    fn apply_diff(&self, diff: &StateDiff) -> Result<(u64, B256), String>;
}

/// The ZK proof sidecar: schedules asynchronous proof generation for a
/// committed block. Fire-and-forget.
pub trait ZkSink: Send + Sync {
    fn on_block_committed(&self, block_number: u64, input: n42_zkproof::BlockExecutionInput);
}

/// Staking sink: scans committed blocks for staking/unstaking txs and persists
/// staking state. Distinct from [`WithdrawalSource`] (the build-path query);
/// both may wrap the same `StakingManager`.
pub trait StakingSink: Send + Sync {
    fn scan_committed_block(&self, view: u64, payload: &[u8]);
    fn save(&self);
}

/// Computes the EIP-4895 withdrawals for the next block: mobile rewards at the
/// epoch boundary plus matured stake returns, with reward addresses resolved to
/// staker EVM addresses. Encapsulates the previous inline reward/staking block
/// in `build_payload_attributes`.
pub trait WithdrawalSource: Send + Sync {
    fn withdrawals_for_block(&self, next_block_number: u64) -> Vec<Withdrawal>;
}
