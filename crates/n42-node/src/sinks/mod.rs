//! Node-side sink adapters. The `StateSink`/`ZkSink`/`StakingSink`/
//! `WithdrawalSource` port traits live in `n42-consensus-service`; this module
//! provides the in-process adapters over the node's state trees, ZK scheduler,
//! and staking/reward managers (Caplin stage 3 / 6).

use crate::mobile_reward::MobileRewardManager;
use crate::staking::StakingManager;
use alloy_eips::eip4895::Withdrawal;
use alloy_primitives::B256;
use n42_execution::state_diff::StateDiff;
use n42_jmt::{PersistentSbmt, PersistentTwig};
use std::sync::{Arc, Mutex};

pub use n42_consensus_service::sinks::{StakingSink, StateSink, WithdrawalSource, ZkSink};

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

/// Adapter over the ZK proof scheduler.
pub struct SchedulerZkSink(pub Arc<n42_zkproof::ProofScheduler>);

impl ZkSink for SchedulerZkSink {
    fn on_block_committed(&self, block_number: u64, input: n42_zkproof::BlockExecutionInput) {
        self.0.on_block_committed(block_number, input);
    }
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

/// In-process adapter combining the mobile reward + staking managers. Either may
/// be absent; with both absent it yields an empty withdrawal set (the prior
/// behavior when neither manager was wired).
pub struct NodeWithdrawalSource {
    pub reward: Option<Arc<Mutex<MobileRewardManager>>>,
    pub staking: Option<Arc<Mutex<StakingManager>>>,
}

impl WithdrawalSource for NodeWithdrawalSource {
    fn withdrawals_for_block(&self, next_block_number: u64) -> Vec<Withdrawal> {
        let staked_pubkeys = if let Some(ref staking_mgr) = self.staking {
            let mgr = staking_mgr.lock().unwrap_or_else(|e| {
                tracing::warn!("staking_mgr mutex poisoned, recovering");
                e.into_inner()
            });
            mgr.staked_bls_pubkeys()
        } else {
            std::collections::HashSet::new()
        };

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

        if let Some(ref staking_mgr) = self.staking {
            let mut staking = staking_mgr.lock().unwrap_or_else(|e| {
                tracing::warn!("staking_mgr mutex poisoned, recovering");
                e.into_inner()
            });

            for w in &mut withdrawals {
                if let Some(addr) = staking.staker_address_by_reward(w.address) {
                    w.address = addr;
                }
            }

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
        assert!(src.withdrawals_for_block(100).is_empty());
    }
}
