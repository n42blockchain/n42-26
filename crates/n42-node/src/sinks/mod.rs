//! Node-side sink adapters. The `StateSink`/`ZkSink`/`StakingSink`/
//! `WithdrawalSource` port traits live in `n42-consensus-service`; this module
//! provides the in-process adapters over the node's state trees, ZK scheduler,
//! and staking/reward managers (Caplin stage 3 / 6).

use crate::mobile_reward::MobileRewardManager;
use crate::staking::StakingManager;
use alloy_eips::eip4895::Withdrawal;
use alloy_primitives::{Address, B256, U256};
use n42_execution::state_diff::StateDiff;
use n42_jmt::{EMPTY_CODE_HASH, PersistentSbmt, PersistentTwig, account_key, storage_key};
use reth_storage_api::{AccountReader, StateProviderFactory};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

pub use n42_consensus_service::sinks::{StakingSink, StateSink, WithdrawalSource, ZkSink};

/// Bound database reads per sampled account. A hot contract can touch tens of
/// thousands of slots inside one probe interval; sampling remains useful
/// without letting that fan out into an unbounded exact-state query.
const MAX_TWIG_PROBE_SLOTS_PER_ACCOUNT: usize = 256;

/// Adapter over the persistent SBMT tree.
pub struct SbmtStateSink(pub Arc<Mutex<PersistentSbmt>>);

impl StateSink for SbmtStateSink {
    fn apply_diff(&self, _block_hash: B256, diff: &StateDiff) -> Result<(u64, B256), String> {
        let mut tree = self.0.lock().unwrap_or_else(|e| {
            tracing::warn!("sbmt mutex poisoned, recovering");
            e.into_inner()
        });
        tree.apply_diff(diff).map_err(|e| e.to_string())
    }
}

/// One deterministic account target for the Twig↔reth post-state probe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TwigProbeTarget {
    pub address: Address,
    /// Changed slots for this account. Account data is always sampled; these
    /// slots add cheap coverage for the unified Twig key/value namespace.
    pub storage_slots: Vec<U256>,
}

/// Normalized account value used by the cross-tree probe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TwigProbeBasicAccount {
    pub balance: U256,
    pub nonce: u64,
    pub code_hash: B256,
}

/// A normalized reth/Twig sample. Missing storage is represented as zero,
/// matching EVM semantics and Twig's deletion of zero-valued leaves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TwigProbeAccount {
    pub account: Option<TwigProbeBasicAccount>,
    pub storage: Vec<U256>,
}

/// Read the authoritative reth post-state for an exact imported block hash.
/// A trait boundary keeps the probe deterministic and fault-injectable in tests.
pub trait TwigProbeReader: Send + Sync {
    fn read_at(
        &self,
        block_hash: B256,
        targets: &[TwigProbeTarget],
    ) -> Result<Vec<TwigProbeAccount>, String>;
}

/// Production probe reader backed by reth's `StateProviderFactory`.
pub struct RethTwigProbeReader<P>(P);

impl<P> RethTwigProbeReader<P> {
    pub fn new(provider: P) -> Self {
        Self(provider)
    }
}

impl<P> TwigProbeReader for RethTwigProbeReader<P>
where
    P: StateProviderFactory + Send + Sync,
{
    fn read_at(
        &self,
        block_hash: B256,
        targets: &[TwigProbeTarget],
    ) -> Result<Vec<TwigProbeAccount>, String> {
        let state = self
            .0
            .state_by_block_hash(block_hash)
            .map_err(|error| format!("reth state unavailable for {block_hash}: {error}"))?;
        targets
            .iter()
            .map(|target| {
                let account = state
                    .basic_account(&target.address)
                    .map_err(|error| {
                        format!("reth account read failed for {}: {error}", target.address)
                    })?
                    .map(|account| TwigProbeBasicAccount {
                        balance: account.balance,
                        nonce: account.nonce,
                        code_hash: account.bytecode_hash.unwrap_or(EMPTY_CODE_HASH),
                    });
                let storage = target
                    .storage_slots
                    .iter()
                    .map(|slot| {
                        state
                            .storage(target.address, B256::from(slot.to_be_bytes::<32>()))
                            .map(|value| value.unwrap_or_default())
                            .map_err(|error| {
                                format!(
                                    "reth storage read failed for {} slot {slot}: {error}",
                                    target.address
                                )
                            })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(TwigProbeAccount { account, storage })
            })
            .collect()
    }
}

/// Low-overhead deterministic sampling policy. Defaults to eight changed
/// accounts every 32 sidecar versions; both values are operator-tunable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TwigProbeConfig {
    pub interval: u64,
    pub accounts_per_sample: usize,
}

impl Default for TwigProbeConfig {
    fn default() -> Self {
        Self {
            interval: 32,
            accounts_per_sample: 8,
        }
    }
}

impl TwigProbeConfig {
    pub fn from_env() -> Self {
        let default = Self::default();
        Self {
            interval: std::env::var("N42_TWIG_PROBE_INTERVAL")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(default.interval)
                .max(1),
            accounts_per_sample: std::env::var("N42_TWIG_PROBE_ACCOUNTS")
                .ok()
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(default.accounts_per_sample)
                .clamp(1, 256),
        }
    }
}

/// Adapter over the persistent Twig tree with a sampled, exact-block reth
/// consistency probe. `healthy` is shared with the mobile packet loop: once a
/// value mismatch is observed, packets stay disabled until the sidecar is
/// rebuilt/restarted from a known-good state.
pub struct TwigStateSink {
    tree: Arc<Mutex<PersistentTwig>>,
    reader: Arc<dyn TwigProbeReader>,
    healthy: Arc<AtomicBool>,
    config: TwigProbeConfig,
    /// Serializes the complete apply + probe cycle. The production sidecar
    /// worker is FIFO already, but keeping this invariant inside the sink
    /// prevents a second caller from advancing Twig while the first probe is
    /// reading the matching reth post-state.
    apply_lock: Mutex<()>,
    /// Deterministic top-M reservoir of accounts changed since the last
    /// successful probe. Keeping the whole interval is essential: sampling
    /// only the boundary block would give an earlier bad write zero chance of
    /// detection.
    probe_candidates: Mutex<Vec<([u8; 32], TwigProbeTarget)>>,
}

impl TwigStateSink {
    pub fn with_reth_probe<P>(
        tree: Arc<Mutex<PersistentTwig>>,
        provider: P,
        healthy: Arc<AtomicBool>,
        config: TwigProbeConfig,
    ) -> Self
    where
        P: StateProviderFactory + Send + Sync + 'static,
    {
        Self::with_probe_reader(
            tree,
            Arc::new(RethTwigProbeReader::new(provider)),
            healthy,
            config,
        )
    }

    pub fn with_probe_reader(
        tree: Arc<Mutex<PersistentTwig>>,
        reader: Arc<dyn TwigProbeReader>,
        healthy: Arc<AtomicBool>,
        config: TwigProbeConfig,
    ) -> Self {
        Self {
            tree,
            reader,
            healthy,
            config: TwigProbeConfig {
                interval: config.interval.max(1),
                accounts_per_sample: config.accounts_per_sample.clamp(1, 256),
            },
            apply_lock: Mutex::new(()),
            probe_candidates: Mutex::new(Vec::new()),
        }
    }

    fn record_probe_candidates(&self, block_hash: B256, diff: &StateDiff) {
        let mut candidates = self
            .probe_candidates
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for (address, account) in &diff.accounts {
            let mut bytes = [0u8; 52];
            bytes[..32].copy_from_slice(block_hash.as_slice());
            bytes[32..].copy_from_slice(address.as_slice());
            let rank = *blake3::hash(&bytes).as_bytes();
            let mut storage_slots = account.storage.keys().copied().collect::<Vec<_>>();
            if storage_slots.len() > MAX_TWIG_PROBE_SLOTS_PER_ACCOUNT {
                let dropped = storage_slots.len() - MAX_TWIG_PROBE_SLOTS_PER_ACCOUNT;
                storage_slots.truncate(MAX_TWIG_PROBE_SLOTS_PER_ACCOUNT);
                metrics::counter!("n42_twig_probe_storage_slots_dropped_total")
                    .increment(dropped as u64);
            }

            if let Some((existing_rank, target)) = candidates
                .iter_mut()
                .find(|(_, target)| target.address == *address)
            {
                *existing_rank = (*existing_rank).min(rank);
                target.storage_slots.extend(storage_slots);
                target.storage_slots.sort_unstable();
                target.storage_slots.dedup();
                if target.storage_slots.len() > MAX_TWIG_PROBE_SLOTS_PER_ACCOUNT {
                    let dropped = target.storage_slots.len() - MAX_TWIG_PROBE_SLOTS_PER_ACCOUNT;
                    target
                        .storage_slots
                        .truncate(MAX_TWIG_PROBE_SLOTS_PER_ACCOUNT);
                    metrics::counter!("n42_twig_probe_storage_slots_dropped_total")
                        .increment(dropped as u64);
                }
                continue;
            }

            let target = TwigProbeTarget {
                address: *address,
                storage_slots,
            };
            if candidates.len() < self.config.accounts_per_sample {
                candidates.push((rank, target));
                continue;
            }
            if let Some((worst_index, _)) = candidates
                .iter()
                .enumerate()
                .max_by_key(|(_, (candidate_rank, _))| *candidate_rank)
                && rank < candidates[worst_index].0
            {
                candidates[worst_index] = (rank, target);
            }
        }
    }

    fn pending_probe_targets(&self) -> Vec<TwigProbeTarget> {
        let mut candidates = self
            .probe_candidates
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        candidates.sort_unstable_by_key(|(rank, _)| *rank);
        candidates.into_iter().map(|(_, target)| target).collect()
    }

    fn clear_probe_candidates(&self) {
        self.probe_candidates
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
    }

    fn read_twig(
        tree: &PersistentTwig,
        targets: &[TwigProbeTarget],
    ) -> Result<Vec<TwigProbeAccount>, String> {
        targets
            .iter()
            .map(|target| {
                let account = tree
                    .inner()
                    .get(&account_key(&target.address).0)
                    .map(|value| {
                        if value.len() != 72 {
                            return Err(format!(
                                "Twig account leaf for {} has length {}, expected 72",
                                target.address,
                                value.len()
                            ));
                        }
                        let mut nonce = [0u8; 8];
                        nonce.copy_from_slice(&value[32..40]);
                        Ok(TwigProbeBasicAccount {
                            balance: U256::from_be_slice(&value[..32]),
                            nonce: u64::from_be_bytes(nonce),
                            code_hash: B256::from_slice(&value[40..72]),
                        })
                    })
                    .transpose()?;
                let storage = target
                    .storage_slots
                    .iter()
                    .map(|slot| {
                        tree.inner()
                            .get(&storage_key(&target.address, slot).0)
                            .map_or(Ok(U256::ZERO), |value| {
                                if value.len() != 32 {
                                    Err(format!(
                                        "Twig storage leaf for {} slot {slot} has length {}, expected 32",
                                        target.address,
                                        value.len()
                                    ))
                                } else {
                                    Ok(U256::from_be_slice(value))
                                }
                            })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(TwigProbeAccount { account, storage })
            })
            .collect()
    }

    fn mark_unhealthy(&self, block_hash: B256, version: u64, reason: &str, mismatch: bool) {
        let was_healthy = self.healthy.swap(false, Ordering::AcqRel);
        metrics::gauge!("n42_twig_sidecar_healthy").set(0.0);
        metrics::gauge!("n42_twig_rebuild_required").set(1.0);
        if mismatch {
            metrics::counter!("n42_twig_consistency_mismatch_total").increment(1);
        } else {
            metrics::counter!("n42_twig_sidecar_error_total").increment(1);
        }
        if was_healthy {
            metrics::counter!("n42_twig_rebuild_required_total").increment(1);
        }
        tracing::error!(
            target: "n42::twig",
            %block_hash,
            version,
            reason,
            "Twig/reth state sample diverged; sidecar unhealthy, mobile packet output disabled until rebuild"
        );
    }
}

impl StateSink for TwigStateSink {
    fn apply_diff(&self, block_hash: B256, diff: &StateDiff) -> Result<(u64, B256), String> {
        let _apply_guard = self
            .apply_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut tree = self.tree.lock().unwrap_or_else(|e| {
            tracing::warn!("twig mutex poisoned, recovering");
            e.into_inner()
        });
        let (version, root) = match tree.apply_diff(diff) {
            Ok(result) => result,
            Err(error) => {
                self.mark_unhealthy(block_hash, tree.version(), &error.to_string(), false);
                return Err(error.to_string());
            }
        };
        self.record_probe_candidates(block_hash, diff);
        if version % self.config.interval != 0 {
            return Ok((version, root));
        }

        let targets = self.pending_probe_targets();
        if targets.is_empty() {
            return Ok((version, root));
        }
        let actual = match Self::read_twig(&tree, &targets) {
            Ok(actual) => actual,
            Err(error) => {
                drop(tree);
                self.mark_unhealthy(block_hash, version, &error, true);
                return Ok((version, root));
            }
        };
        drop(tree);

        let expected = match self.reader.read_at(block_hash, &targets) {
            Ok(expected) => expected,
            Err(error) => {
                metrics::counter!("n42_twig_consistency_probe_error_total").increment(1);
                tracing::warn!(
                    target: "n42::twig",
                    %block_hash,
                    version,
                    error,
                    "Twig/reth state sample unavailable; will retry on the next interval"
                );
                return Ok((version, root));
            }
        };
        self.clear_probe_candidates();
        metrics::counter!("n42_twig_consistency_probe_total").increment(1);
        metrics::histogram!("n42_twig_consistency_probe_accounts").record(targets.len() as f64);
        if expected != actual {
            let reason = if expected.len() != actual.len() {
                format!(
                    "sample length mismatch: reth={}, Twig={}",
                    expected.len(),
                    actual.len()
                )
            } else {
                expected
                    .iter()
                    .zip(&actual)
                    .zip(&targets)
                    .find_map(|((expected, actual), target)| {
                        (expected != actual).then(|| {
                            format!(
                                "sample mismatch for {}: reth={expected:?}, Twig={actual:?}",
                                target.address
                            )
                        })
                    })
                    .unwrap_or_else(|| "sample mismatch".to_string())
            };
            self.mark_unhealthy(block_hash, version, &reason, true);
        } else if self.healthy.load(Ordering::Acquire) {
            metrics::gauge!("n42_twig_sidecar_healthy").set(1.0);
        }
        Ok((version, root))
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
    fn scan_committed_block(&self, block_number: u64, payload: &[u8]) {
        let mut mgr = self.0.lock().unwrap_or_else(|e| {
            tracing::warn!("staking_mgr mutex poisoned, recovering");
            e.into_inner()
        });
        mgr.scan_committed_block(block_number, payload);
    }

    fn last_scanned_block(&self) -> u64 {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .last_scanned_block()
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
    use n42_execution::state_diff::{AccountChangeType, AccountDiff, ValueChange};
    use std::collections::BTreeMap;
    use std::sync::atomic::AtomicUsize;

    struct FixedProbeReader {
        expected: Mutex<TwigProbeAccount>,
        reads: AtomicUsize,
    }

    impl TwigProbeReader for FixedProbeReader {
        fn read_at(
            &self,
            _block_hash: B256,
            targets: &[TwigProbeTarget],
        ) -> Result<Vec<TwigProbeAccount>, String> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            Ok(vec![self.expected.lock().unwrap().clone(); targets.len()])
        }
    }

    fn one_account_diff(
        address: Address,
        change_type: AccountChangeType,
        from_balance: u64,
        to_balance: u64,
        nonce: u64,
    ) -> StateDiff {
        StateDiff {
            accounts: BTreeMap::from([(
                address,
                AccountDiff {
                    change_type,
                    balance: Some(ValueChange::new(
                        U256::from(from_balance),
                        U256::from(to_balance),
                    )),
                    nonce: Some(ValueChange::new(nonce.saturating_sub(1), nonce)),
                    code_change: None,
                    storage: BTreeMap::new(),
                },
            )]),
        }
    }

    #[test]
    fn twig_probe_detects_earlier_wrong_write_within_interval_and_stays_unhealthy() {
        let dir = tempfile::tempdir().unwrap();
        let tree = Arc::new(Mutex::new(
            PersistentTwig::open(dir.path().join("twig.snapshot"), u64::MAX).unwrap(),
        ));
        let healthy = Arc::new(AtomicBool::new(true));
        let address = Address::with_last_byte(0x42);
        let reader = Arc::new(FixedProbeReader {
            expected: Mutex::new(TwigProbeAccount {
                account: Some(TwigProbeBasicAccount {
                    balance: U256::from(1_000),
                    nonce: 2,
                    code_hash: EMPTY_CODE_HASH,
                }),
                storage: vec![],
            }),
            reads: AtomicUsize::new(0),
        });
        let sink = TwigStateSink::with_probe_reader(
            tree,
            reader.clone(),
            Arc::clone(&healthy),
            TwigProbeConfig {
                interval: 2,
                accounts_per_sample: 1,
            },
        );

        // Inject the wrong value before the sampling boundary. A boundary-only
        // sampler would forget this account when block 2 has an empty diff.
        sink.apply_diff(
            B256::repeat_byte(1),
            &one_account_diff(address, AccountChangeType::Created, 0, 999, 1),
        )
        .unwrap();
        assert!(healthy.load(Ordering::Acquire));

        sink.apply_diff(B256::repeat_byte(2), &StateDiff::default())
            .unwrap();
        assert_eq!(reader.reads.load(Ordering::Relaxed), 1);
        assert!(
            !healthy.load(Ordering::Acquire),
            "the probe must latch unhealthy no later than its configured N-block interval"
        );

        // A later matching sample cannot silently clear the latch. Recovery
        // requires rebuilding/reopening a known-good sidecar.
        sink.apply_diff(
            B256::repeat_byte(3),
            &one_account_diff(address, AccountChangeType::Modified, 999, 1_000, 3),
        )
        .unwrap();
        reader.expected.lock().unwrap().account = Some(TwigProbeBasicAccount {
            balance: U256::from(1_001),
            nonce: 4,
            code_hash: EMPTY_CODE_HASH,
        });
        sink.apply_diff(
            B256::repeat_byte(4),
            &one_account_diff(address, AccountChangeType::Modified, 1_000, 1_001, 4),
        )
        .unwrap();
        assert_eq!(reader.reads.load(Ordering::Relaxed), 2);
        assert!(!healthy.load(Ordering::Acquire));
    }

    #[test]
    fn twig_probe_caps_storage_slots_per_sampled_account() {
        let dir = tempfile::tempdir().unwrap();
        let tree = Arc::new(Mutex::new(
            PersistentTwig::open(dir.path().join("twig.snapshot"), u64::MAX).unwrap(),
        ));
        let address = Address::with_last_byte(0x52);
        let reader = Arc::new(FixedProbeReader {
            expected: Mutex::new(TwigProbeAccount {
                account: None,
                storage: Vec::new(),
            }),
            reads: AtomicUsize::new(0),
        });
        let sink = TwigStateSink::with_probe_reader(
            tree,
            reader,
            Arc::new(AtomicBool::new(true)),
            TwigProbeConfig {
                interval: 32,
                accounts_per_sample: 1,
            },
        );
        let storage = (0..(MAX_TWIG_PROBE_SLOTS_PER_ACCOUNT + 44))
            .map(|slot| {
                (
                    U256::from(slot),
                    ValueChange::new(U256::ZERO, U256::from(slot + 1)),
                )
            })
            .collect();
        let diff = StateDiff {
            accounts: BTreeMap::from([(
                address,
                AccountDiff {
                    change_type: AccountChangeType::Modified,
                    balance: None,
                    nonce: None,
                    code_change: None,
                    storage,
                },
            )]),
        };

        sink.record_probe_candidates(B256::repeat_byte(0x53), &diff);
        let targets = sink.pending_probe_targets();
        assert_eq!(targets.len(), 1);
        assert_eq!(
            targets[0].storage_slots.len(),
            MAX_TWIG_PROBE_SLOTS_PER_ACCOUNT
        );
    }

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

    #[test]
    fn withdrawal_source_combines_reward_resolution_and_staking_returns() {
        let staked_bls = [0x11u8; 48];
        let unstaking_bls = [0x22u8; 48];
        let staked_addr = alloy_primitives::Address::with_last_byte(0x11);
        let unstaking_addr = alloy_primitives::Address::with_last_byte(0x22);
        let stake_wei = "32000000000000000000";
        let staking_json = format!(
            r#"{{
              "stakes": [
                {{
                  "staker": "{staked_addr}",
                  "bls_pubkey": "{}",
                  "amount": "{stake_wei}",
                  "staked_at_block": 1,
                  "status": "Active"
                }},
                {{
                  "staker": "{unstaking_addr}",
                  "bls_pubkey": "{}",
                  "amount": "{stake_wei}",
                  "staked_at_block": 1,
                  "status": {{ "Unstaking": {{ "initiated_block": 1 }} }}
                }}
              ],
              "registrations": [],
              "pending_returns": [],
              "last_scanned_block": 1,
              "next_withdrawal_index": 1000000
            }}"#,
            hex::encode(staked_bls),
            hex::encode(unstaking_bls)
        );

        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), staking_json).unwrap();
        let staking = Arc::new(Mutex::new(StakingManager::load_or_new(tmp.path())));

        let reward = Arc::new(Mutex::new(MobileRewardManager::new(
            10,
            100_000_000,
            4.0,
            32,
            0.1,
        )));
        for _ in 0..10 {
            reward.lock().unwrap().record_attestation(&staked_bls);
        }

        let src = NodeWithdrawalSource {
            reward: Some(reward),
            staking: Some(staking),
        };

        let withdrawals = src.withdrawals_for_block(1 + crate::staking::UNSTAKE_COOLDOWN_BLOCKS);

        assert_eq!(withdrawals.len(), 2);
        assert_eq!(withdrawals[0].address, staked_addr);
        assert_eq!(withdrawals[0].amount, 100_000_000);
        assert_eq!(
            withdrawals[0].index,
            (1 + crate::staking::UNSTAKE_COOLDOWN_BLOCKS) * 32
        );
        assert_eq!(withdrawals[1].address, unstaking_addr);
        assert_eq!(withdrawals[1].amount, 32_000_000_000);
        assert_eq!(withdrawals[1].index, 1_000_000);
    }
}
