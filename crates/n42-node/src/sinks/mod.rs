//! Sink ports — trait boundaries over the node-side side-effect consumers the
//! consensus orchestrator drives (state trees, ZK proof sidecar, staking
//! scan/persist, reward/staking withdrawal computation). Mirrors the
//! `n42-consensus` `VoteLogWriter` pattern: the trait is the port, a concrete
//! adapter wraps the node type and locks internally, so the orchestrator's
//! runtime logic no longer references the concrete `PersistentSbmt` /
//! `PersistentTwig` / `MobileRewardManager` / `StakingManager` /
//! `ProofScheduler` types. Part of the Caplin EL-seam refactor (stage 3); see
//! `docs/task-caplin-cl-module.md`.

use crate::mobile_reward::MobileRewardManager;
use crate::staking::StakingManager;
use alloy_eips::eip4895::Withdrawal;
use alloy_primitives::B256;
use n42_execution::state_diff::StateDiff;
use n42_jmt::{PersistentSbmt, PersistentTwig};
use std::sync::{Arc, Mutex};

/// A sidecar state tree (SBMT or Twig) that applies a per-block `StateDiff`.
/// Implementations lock internally and return `(version, root)` so the
/// orchestrator keeps its existing metrics / logs / shared-state callbacks.
pub trait StateSink: Send + Sync {
    /// Apply a state diff, returning `(version, root)`. Locks internally.
    fn apply_diff(&self, diff: &StateDiff) -> Result<(u64, B256), String>;
}

/// Adapter over the persistent SBMT tree.
pub struct SbmtStateSink(pub Arc<Mutex<PersistentSbmt>>);

impl StateSink for SbmtStateSink {
    fn apply_diff(&self, diff: &StateDiff) -> Result<(u64, B256), String> {
        let mut tree = self.0.lock().unwrap_or_else(|e| {
            tracing::warn!("sbmt mutex poisoned, recovering");
            e.into_inner()
        });
        tree.apply_diff(diff).map_err(|e| e.to_string())
    }
}

/// Adapter over the persistent Twig tree.
pub struct TwigStateSink(pub Arc<Mutex<PersistentTwig>>);

impl StateSink for TwigStateSink {
    fn apply_diff(&self, diff: &StateDiff) -> Result<(u64, B256), String> {
        let mut tree = self.0.lock().unwrap_or_else(|e| {
            tracing::warn!("twig mutex poisoned, recovering");
            e.into_inner()
        });
        tree.apply_diff(diff).map_err(|e| e.to_string())
    }
}

/// The ZK proof sidecar: schedules asynchronous proof generation for a
/// committed block. Fire-and-forget.
pub trait ZkSink: Send + Sync {
    fn on_block_committed(&self, block_number: u64, input: n42_zkproof::BlockExecutionInput);
}

/// Adapter over the ZK proof scheduler.
pub struct SchedulerZkSink(pub Arc<n42_zkproof::ProofScheduler>);

impl ZkSink for SchedulerZkSink {
    fn on_block_committed(&self, block_number: u64, input: n42_zkproof::BlockExecutionInput) {
        self.0.on_block_committed(block_number, input);
    }
}

/// Staking sink: scans committed blocks for staking/unstaking txs and persists
/// staking state. Distinct from [`WithdrawalSource`] (the build-path query);
/// both may wrap the same `StakingManager`.
pub trait StakingSink: Send + Sync {
    fn scan_committed_block(&self, view: u64, payload: &[u8]);
    fn save(&self);
}

/// Adapter over the staking manager for the scan/persist role.
pub struct ManagerStakingSink(pub Arc<Mutex<StakingManager>>);

impl StakingSink for ManagerStakingSink {
    fn scan_committed_block(&self, view: u64, payload: &[u8]) {
        let mut mgr = self.0.lock().unwrap_or_else(|e| {
            tracing::warn!("staking_mgr mutex poisoned, recovering");
            e.into_inner()
        });
        mgr.scan_committed_block(view, payload);
    }

    fn save(&self) {
        let mgr = self.0.lock().unwrap_or_else(|e| {
            tracing::warn!("staking_mgr mutex poisoned, recovering");
            e.into_inner()
        });
        mgr.save();
    }
}

/// Computes the EIP-4895 withdrawals for the next block: mobile rewards at the
/// epoch boundary plus matured stake returns, with reward addresses resolved to
/// staker EVM addresses. Encapsulates the previous inline reward/staking block
/// in `build_payload_attributes`.
pub trait WithdrawalSource: Send + Sync {
    fn withdrawals_for_block(&self, next_block_number: u64) -> Vec<Withdrawal>;
}

/// In-process adapter combining the mobile reward + staking managers. Either may
/// be absent; with both absent it yields an empty withdrawal set (the prior
/// behavior when neither manager was wired).
pub struct NodeWithdrawalSource {
    pub reward: Option<Arc<Mutex<MobileRewardManager>>>,
    pub staking: Option<Arc<Mutex<StakingManager>>>,
}

impl WithdrawalSource for NodeWithdrawalSource {
    fn withdrawals_for_block(&self, next_block_number: u64) -> Vec<Withdrawal> {
        // 1. Pre-fetch staked BLS pubkeys (lock StakingManager, then release).
        //    This avoids holding both locks simultaneously and prevents deadlocks.
        let staked_pubkeys = if let Some(ref staking_mgr) = self.staking {
            let mgr = staking_mgr.lock().unwrap_or_else(|e| {
                tracing::warn!("staking_mgr mutex poisoned, recovering");
                e.into_inner()
            });
            mgr.staked_bls_pubkeys()
        } else {
            std::collections::HashSet::new()
        };

        // 2. Compute mobile rewards (lock MobileRewardManager).
        let mut withdrawals = vec![];
        if let Some(ref reward_mgr) = self.reward {
            let mut mgr = reward_mgr.lock().unwrap_or_else(|e| {
                tracing::error!(target: "n42::cl::exec_bridge", "mobile_reward_manager mutex poisoned: {e}");
                e.into_inner()
            });
            mgr.check_epoch_boundary(next_block_number, &staked_pubkeys);
            withdrawals = mgr.take_pending_rewards(next_block_number);
            if !withdrawals.is_empty() {
                tracing::info!(
                    target: "n42::cl::exec_bridge",
                    count = withdrawals.len(),
                    "injecting mobile rewards as withdrawals"
                );
            }
        }

        // 3. Staking integration: resolve reward addresses and add cooldown returns.
        if let Some(ref staking_mgr) = self.staking {
            let mut staking = staking_mgr.lock().unwrap_or_else(|e| {
                tracing::warn!("staking_mgr mutex poisoned, recovering");
                e.into_inner()
            });

            // Reward address resolution: BLS-derived keccak → staker's actual EVM address.
            for w in &mut withdrawals {
                if let Some(addr) = staking.staker_address_by_reward(w.address) {
                    w.address = addr;
                }
            }

            // Cooldown expiration checks + return withdrawals.
            staking.check_cooldowns(next_block_number);
            let returns = staking.take_pending_returns(next_block_number, 8);
            if !returns.is_empty() {
                tracing::info!(
                    target: "n42::cl::exec_bridge",
                    count = returns.len(),
                    "injecting staking returns as withdrawals"
                );
                withdrawals.extend(returns);
            }
        }

        withdrawals
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn withdrawal_source_empty_when_no_managers() {
        let src = NodeWithdrawalSource {
            reward: None,
            staking: None,
        };
        assert!(src.withdrawals_for_block(1).is_empty());
        assert!(src.withdrawals_for_block(21_600).is_empty());
    }

    #[test]
    fn staking_sink_scan_and_save_do_not_panic() {
        let mgr = Arc::new(Mutex::new(StakingManager::new()));
        let sink = ManagerStakingSink(Arc::clone(&mgr));
        // Empty / non-staking payload must be a no-op, not a panic.
        sink.scan_committed_block(1, b"{}");
        sink.save();
    }

    #[test]
    fn withdrawal_source_with_staking_only_resolves_without_panic() {
        let staking = Arc::new(Mutex::new(StakingManager::new()));
        let src = NodeWithdrawalSource {
            reward: None,
            staking: Some(staking),
        };
        // No rewards and no matured returns ⇒ empty.
        assert!(src.withdrawals_for_block(100).is_empty());
    }
}
