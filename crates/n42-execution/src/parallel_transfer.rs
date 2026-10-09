// Copyright (c) 2017-2025 N42 Contributors
// SPDX-License-Identifier: Apache-2.0
//! Parallel transfer batches and deterministic bundle grafting.
//!
//! Adapted from n42-rs at 466c1839791afadba7f1dd4d48c5518440dcafba.
//! Integration must open one exact immutable pre-transaction state for every batch;
//! neither completion nor a graft means that a block has passed consensus validation.

use crate::evm_factory::N42EvmFactory;
use alloy_evm::{Evm as _, EvmFactory as _};
use alloy_primitives::{Address, U256};
use revm::{
    Database, DatabaseCommit,
    context::TxEnv,
    database::{
        AccountRevert, BundleAccount, BundleState, PlainAccount, State, states::CacheAccount,
        states::bundle_state::BundleRetention,
    },
    state::{Account, AccountInfo},
};

/// A graft error requires discarding the partially assembled candidate state.
#[derive(Debug, thiserror::Error)]
pub enum MergeError<E> {
    #[error("state read failed: {0}")]
    Database(E),
    #[error("account delta overflows or decreases nonce at {0}")]
    Arithmetic(Address),
}

fn apply_delta<E>(
    info: &mut revm::state::AccountInfo,
    new_balance: U256,
    old_balance: U256,
    new_nonce: u64,
    old_nonce: u64,
    address: Address,
) -> Result<(), MergeError<E>> {
    let balance = if new_balance >= old_balance {
        info.balance.checked_add(new_balance - old_balance)
    } else {
        info.balance.checked_sub(old_balance - new_balance)
    }
    .ok_or(MergeError::Arithmetic(address))?;
    let nonce = new_nonce
        .checked_sub(old_nonce)
        .and_then(|delta| info.nonce.checked_add(delta))
        .ok_or(MergeError::Arithmetic(address))?;
    info.balance = balance;
    info.nonce = nonce;
    Ok(())
}

fn add_credit<E>(
    total: &mut U256,
    new: U256,
    old: U256,
    address: Address,
) -> Result<(), MergeError<E>> {
    *total = new
        .checked_sub(old)
        .and_then(|delta| total.checked_add(delta))
        .ok_or(MergeError::Arithmetic(address))?;
    Ok(())
}

/// The executor's phase timings, in milliseconds: partitioning, the groups'
/// execution (wall), the merge into the block's state, the finish.
#[derive(Debug, Clone, Copy, Default)]
pub struct Phases {
    /// Partitioning the transactions into conflict-free groups.
    pub partition_ms: u64,
    /// The groups' execution on the worker pool, wall time.
    pub groups_ms: u64,
    /// Folding the groups' changes into the block's state.
    pub merge_ms: u64,
    /// Of `merge_ms`: the graft (or fold) of the batches' bundles.
    pub graft_ms: u64,
    /// Of `merge_ms`: the state's own transition merge and the bundle take.
    pub take_ms: u64,
    /// Of `merge_ms`: appending and sorting the grafted reverts.
    pub reverts_ms: u64,
    /// The batches' results placed in candidate order (the build).
    pub collect_ms: u64,
    /// Pre- and post-execution changes and the bundle.
    pub finish_ms: u64,
    /// How many groups there were.
    pub groups: usize,
    /// How many batches of groups ran (the build's [`execute_for_build`]
    /// runs a sender per group and several groups per batch).
    pub batches: usize,
}

/// Why the parallel path did not run; the caller executes serially.
#[derive(Debug)]
pub enum NotParallel {
    /// A transaction the transfer path does not take (index).
    NotATransfer(usize),
    /// A transfer to or from the block's beneficiary (index).
    TouchesBeneficiary(usize),
    /// A transfer failed on the path (index, message): the serial executor
    /// produces the exact error.
    Failed(usize, String),
    /// The parent's state could not be opened for a group.
    NoState,
}

impl std::fmt::Display for NotParallel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotATransfer(i) => write!(f, "transaction {i} is not a plain transfer"),
            Self::TouchesBeneficiary(i) => write!(f, "transaction {i} touches the beneficiary"),
            Self::Failed(i, m) => write!(f, "transaction {i} failed on the transfer path: {m}"),
            Self::NoState => write!(f, "the parent's state could not be opened"),
        }
    }
}

impl std::error::Error for NotParallel {}

/// Disjoint-set forest over the accounts a block touches.
struct Groups {
    parent: Vec<usize>,
}

impl Groups {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
        }
    }
    fn find(&mut self, mut x: usize) -> usize {
        while self.parent[x] != x {
            self.parent[x] = self.parent[self.parent[x]];
            x = self.parent[x];
        }
        x
    }
    fn union(&mut self, a: usize, b: usize) {
        let (a, b) = (self.find(a), self.find(b));
        if a != b {
            self.parent[a] = b;
        }
    }
}

/// Partitions transfers into groups that share no sender or recipient: the
/// groups can execute in any order relative to each other. Returns the groups
/// (indices into `txs`, in order) and the number of distinct parties.
pub fn partition(
    txs: &[TxEnv],
    beneficiary: Address,
) -> Result<(Vec<Vec<usize>>, usize), NotParallel> {
    let mut index_of: alloy_primitives::map::AddressHashMap<usize> =
        alloy_primitives::map::AddressHashMap::default();
    index_of.reserve(txs.len() * 2);
    let mut party = |a: Address| -> usize {
        let next = index_of.len();
        *index_of.entry(a).or_insert(next)
    };
    let mut edges: Vec<(usize, usize)> = Vec::with_capacity(txs.len());
    for (i, tx) in txs.iter().enumerate() {
        let alloy_primitives::TxKind::Call(to) = tx.kind else {
            return Err(NotParallel::NotATransfer(i));
        };
        if !tx.data.is_empty() {
            return Err(NotParallel::NotATransfer(i));
        }
        if tx.caller == beneficiary || to == beneficiary {
            return Err(NotParallel::TouchesBeneficiary(i));
        }
        edges.push((party(tx.caller), party(to)));
    }
    let mut sets = Groups::new(index_of.len());
    for (a, b) in &edges {
        sets.union(*a, *b);
    }
    let mut group_of_root: Vec<usize> = vec![usize::MAX; index_of.len()];
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for (i, (a, _)) in edges.iter().enumerate() {
        let root = sets.find(*a);
        if group_of_root[root] == usize::MAX {
            group_of_root[root] = groups.len();
            groups.push(Vec::new());
        }
        groups[group_of_root[root]].push(i);
    }
    Ok((groups, index_of.len()))
}

/// Groups candidate transfers by sender: every sender's transfers, in
/// candidate order, form one group. If a recipient is also a sender, the
/// affected sender groups are joined and retain candidate order. `keys` is
/// each candidate's (sender, recipient). Passive recipients do not join groups -- a transfer only adds to its
/// recipient's balance, and additions commute, so [`graft_bundles`] can fold
/// batches that share a recipient in any order. (Grouping by connected
/// component, as the follower's [`partition`] does, merges a full block of
/// random transfers into a handful of giant groups: round 43.)
///
/// Returns `Err` when a candidate touches the beneficiary.
pub fn partition_by_sender(
    keys: &[(Address, Address)],
    beneficiary: Address,
) -> Result<Vec<Vec<usize>>, NotParallel> {
    let mut sender_index = alloy_primitives::map::AddressHashMap::<usize>::default();
    for (i, (sender, to)) in keys.iter().enumerate() {
        if *sender == beneficiary || *to == beneficiary {
            return Err(NotParallel::TouchesBeneficiary(i));
        }
        let next = sender_index.len();
        sender_index.entry(*sender).or_insert(next);
    }
    // Credits commute only for passive recipients. A recipient that also spends
    // must stay ordered with every sender that can fund it, including later credits.
    let mut sets = Groups::new(sender_index.len());
    for (sender, to) in keys {
        if let Some(&recipient_sender) = sender_index.get(to) {
            sets.union(sender_index[sender], recipient_sender);
        }
    }
    let mut group_of = vec![usize::MAX; sender_index.len()];
    let mut groups = Vec::<Vec<usize>>::new();
    for (i, (sender, _)) in keys.iter().enumerate() {
        let root = sets.find(sender_index[sender]);
        if group_of[root] == usize::MAX {
            group_of[root] = groups.len();
            groups.push(Vec::new());
        }
        groups[group_of[root]].push(i);
    }
    Ok(groups)
}

/// The worker pool the build's batches run on: its own, so that they do not
/// queue behind the global pool's other jobs (the QMDB root of the block
/// before, a follower import). `N42_PARALLEL_BUILD_THREADS` threads, 16 by
/// default.
pub fn build_pool() -> &'static rayon::ThreadPool {
    static POOL: std::sync::OnceLock<rayon::ThreadPool> = std::sync::OnceLock::new();
    POOL.get_or_init(|| {
        let threads = std::env::var("N42_PARALLEL_BUILD_THREADS")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|n| *n > 0)
            .unwrap_or(16usize)
            .clamp(1, 32)
            .min(std::thread::available_parallelism().map_or(1, usize::from));
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|i| format!("n42-build-{i}"))
            .build()
            .expect("a thread pool for the parallel build")
    })
}

/// Whole groups packed into at most `2 x workers` batches of about equal
/// size (a couple of thousand transfers each), in group order: what one
/// worker executes on one view of the parent's state.
pub fn batch_groups(groups: &[Vec<usize>], total: usize, workers: usize) -> Vec<Vec<&Vec<usize>>> {
    let wanted = (total / 2048).clamp(1, workers.clamp(1, 32) * 2);
    let per_batch = total.div_ceil(wanted).max(1);
    let mut batches: Vec<Vec<&Vec<usize>>> = Vec::with_capacity(wanted + 1);
    let mut current: Vec<&Vec<usize>> = Vec::new();
    let mut filled = 0usize;
    for group in groups {
        current.push(group);
        filled += group.len();
        if filled >= per_batch {
            batches.push(std::mem::take(&mut current));
            filled = 0;
        }
    }
    if !current.is_empty() {
        batches.push(current);
    }
    batches
}

/// One transfer executed for a block being built: its index in the candidate
/// list, the transaction as `convert` produced it, the EVM's result, and the
/// gas it used. Its state changes are in its batch's bundle
/// ([`BuildRun::bundles`]).
#[derive(Debug)]
pub struct BuiltTransfer<T> {
    /// Index into the candidates.
    pub index: usize,
    /// The transaction, as `convert` produced it.
    pub tx: T,
    /// The EVM's result, for the receipt.
    pub result: revm::context::result::ExecutionResult<revm::context::result::HaltReason>,
    /// Gas used.
    pub gas_used: u64,
}

/// What [`execute_for_build`] produced.
#[derive(Debug)]
pub struct BuildRun<T> {
    /// The transfers that executed, in candidate order, which is the order
    /// they take in the block.
    pub executed: Vec<BuiltTransfer<T>>,
    /// Candidates the transfer path refused (a nonce that is not the
    /// account's, a balance short, a shape it does not take): left for the
    /// serial builder, in candidate order.
    pub skipped: Vec<usize>,
    /// Each batch's changes against the parent's state, with reverts, for
    /// [`graft_bundles`].
    pub bundles: Vec<BundleState>,
    /// Phase timings.
    pub phases: Phases,
}

impl<T> Default for BuildRun<T> {
    fn default() -> Self {
        Self {
            executed: Vec::new(),
            skipped: Vec::new(),
            bundles: Vec::new(),
            phases: Phases::default(),
        }
    }
}

/// Folds bundles that were each computed against the parent's state into
/// one set of changes on `state`, the block's state as it stands: every
/// account gets what the bundles changed it by, added to what `state` holds
/// (an account two bundles credited gets both credits; one a reward reached
/// and a transfer touched gets both). The beneficiary is left out and its
/// total credit returned, for the caller to apply with the block's other
/// credits. Nothing is committed here.
pub fn fold_bundles<DB: Database>(
    state: &mut State<DB>,
    bundles: &[revm::database::BundleState],
    beneficiary: Address,
) -> Result<(revm::state::EvmState, U256), MergeError<<State<DB> as Database>::Error>> {
    let mut changes: revm::state::EvmState = Default::default();
    changes.reserve(bundles.iter().map(|b| b.state.len()).sum::<usize>() + 1);
    let mut beneficiary_delta = U256::ZERO;
    for bundle in bundles {
        for (address, account) in &bundle.state {
            let BundleAccount {
                info,
                original_info,
                ..
            } = account;
            let (new_balance, new_nonce) = match info {
                Some(info) => (info.balance, info.nonce),
                None => continue,
            };
            let (old_balance, old_nonce) = match original_info {
                Some(orig) => (orig.balance, orig.nonce),
                None => (U256::ZERO, 0),
            };
            if *address == beneficiary {
                add_credit(
                    &mut beneficiary_delta,
                    new_balance,
                    old_balance,
                    beneficiary,
                )?;
                continue;
            }
            // Another bundle's change to the same account is already in
            // `changes`; otherwise the block's view, loaded so the transition
            // records the parent's original.
            let (mut merged, existed) = match changes.remove(address) {
                Some(acc) => {
                    let existed = !acc.is_loaded_as_not_existing();
                    (acc.info, existed)
                }
                None => {
                    let current = state.basic(*address).map_err(MergeError::Database)?;
                    let existed = current.is_some();
                    (current.unwrap_or_default(), existed)
                }
            };
            apply_delta(
                &mut merged,
                new_balance,
                old_balance,
                new_nonce,
                old_nonce,
                *address,
            )?;
            let mut acc = if existed {
                Account::from(merged.clone())
            } else {
                Account::new_not_existing(revm::state::TransactionId::ZERO)
            };
            acc.info = merged;
            acc.mark_touch();
            changes.insert(*address, acc);
        }
    }
    Ok((changes, beneficiary_delta))
}

/// What [`graft_bundles`] left for the caller.
#[derive(Debug, Default)]
pub struct Graft {
    /// The beneficiary's total credit across the bundles, not applied.
    pub beneficiary_delta: U256,
    /// The common parent value seen by the workers. This is a fallback for
    /// import databases whose provider cannot read the beneficiary directly.
    pub beneficiary_original: Option<AccountInfo>,
    /// The grafted accounts' reverts. They belong to the block's revert set,
    /// which the state's own merge creates later: append them to it once the
    /// bundle is taken (see [`append_reverts`]).
    pub reverts: Vec<(Address, AccountRevert)>,
    /// Accounts grafted.
    pub accounts: usize,
    /// Accounts the block's state already held and that went in as deltas
    /// through a commit instead.
    pub committed: usize,
}

/// Grafts the batches' bundles onto the block's state directly: each account
/// goes into the state's cache and bundle as its batch left it (the batch's
/// original is the parent's, which is what the block's state holds for an
/// account nothing before it touched), an account two batches touched gets
/// both changes added together, and the beneficiary is left out with its
/// credit returned. This goes around the state's transition machinery, which
/// is the point: committing 160,000 accounts through it and merging the
/// transitions cost more than executing the transfers did (round 43). The
/// few accounts the block's state already has in its cache (a system
/// contract, an earlier transaction's) are applied as deltas through a
/// commit, as [`fold_bundles`] does for all.
///
/// Each bundle must have been built with [`BundleRetention::Reverts`] against
/// the same immutable pre-transaction state. On error, discard the candidate:
/// some accounts may already have been grafted, so in-place serial retry is invalid.
pub fn graft_bundles<DB: Database>(
    state: &mut State<DB>,
    bundles: Vec<BundleState>,
    beneficiary: Address,
) -> Result<Graft, MergeError<<State<DB> as Database>::Error>> {
    graft_bundles_with(state, bundles, beneficiary, true)
}

/// [`graft_bundles`] with optional cache retention. Use `keep_cache = true`
/// whenever serial transactions or post-execution system calls can read changed
/// accounts. A caller may omit cache insertion only if it proves no such reads
/// occur. Installed state hooks receive one complete transfer-state update;
/// beneficiary credit and subsequent execution use ordinary commit notifications.
pub fn graft_bundles_with<DB: Database>(
    state: &mut State<DB>,
    bundles: Vec<BundleState>,
    beneficiary: Address,
    keep_cache: bool,
) -> Result<Graft, MergeError<<State<DB> as Database>::Error>> {
    // Direct bundle insertion bypasses State::commit. Root consumers still need
    // every changed account, including shared recipients after all deltas fold.
    // Keep this allocation off the hot path when no hook is installed.
    let mut hook = state.state_hook.take();
    let observed = hook.as_ref().map(|_| {
        let mut accounts = alloy_primitives::map::AddressHashMap::default();
        for bundle in &bundles {
            for (address, account) in &bundle.state {
                if *address != beneficiary && account.info.is_some() {
                    accounts.insert(*address, account.original_info.is_none());
                }
            }
        }
        accounts
    });
    let result = graft_bundles_inner(state, bundles, beneficiary, keep_cache);
    if result.is_ok()
        && let (Some(hook), Some(observed)) = (hook.as_mut(), observed)
    {
        let changes = observed
            .into_iter()
            .map(|(address, absent)| {
                let info = state
                    .cache
                    .accounts
                    .get(&address)
                    .and_then(|account| account.account.as_ref().map(|a| &a.info))
                    .or_else(|| {
                        state
                            .bundle_state
                            .state
                            .get(&address)
                            .and_then(|a| a.info.as_ref())
                    })
                    .expect("successfully grafted transfer account has final info")
                    .clone();
                let mut account = if absent {
                    Account::new_not_existing(revm::state::TransactionId::ZERO)
                } else {
                    Account::from(info.clone())
                };
                account.info = info;
                account.mark_touch();
                (address, account)
            })
            .collect();
        hook.on_state(changes);
    }
    // Restore on error too: dropping the hook prematurely signals root-job EOF.
    state.state_hook = hook;
    result
}

fn graft_bundles_inner<DB: Database>(
    state: &mut State<DB>,
    bundles: Vec<BundleState>,
    beneficiary: Address,
    keep_cache: bool,
) -> Result<Graft, MergeError<<State<DB> as Database>::Error>> {
    let mut graft = Graft::default();
    let mut bundles = bundles;
    // The largest bundle becomes the block's bundle instead of being copied
    // into an empty one, when nothing stands in its way: the follower's
    // partition by connected component puts most of a block of random
    // transfers into one giant group (round 43: 317 groups, one of them
    // nearly the whole block), and re-inserting its 140,000 accounts was the
    // bulk of an 84 ms merge. Only when the cache is not kept (the builder's
    // state needs the cache entries), the block's bundle is still empty and
    // no account of it is one the block's state already holds or the
    // beneficiary -- those go through the delta paths below.
    if !keep_cache
        && state.bundle_state.state.is_empty()
        && let Some(largest) = (0..bundles.len()).max_by_key(|&i| bundles[i].state.len())
    {
        let clear = {
            let base = &bundles[largest];
            !base.state.is_empty()
                && !base.state.keys().any(|address| {
                    *address != beneficiary && state.cache.accounts.contains_key(address)
                })
        };
        if clear {
            let base = bundles.swap_remove(largest);
            let BundleState {
                state: mut accounts,
                contracts,
                mut reverts,
                mut state_size,
                ..
            } = base;
            // The beneficiary -- every transfer's fee lands on it, so
            // every bundle holds it -- goes as a delta like everywhere
            // else in this function, and its revert is dropped with it.
            if let Some(account) = accounts.remove(&beneficiary) {
                state_size -= account.size_hint();
                let new_balance = account.info.as_ref().map(|i| i.balance).unwrap_or_default();
                let old_balance = account
                    .original_info
                    .as_ref()
                    .map(|i| i.balance)
                    .unwrap_or_default();
                graft.beneficiary_original = account.original_info.clone();
                add_credit(
                    &mut graft.beneficiary_delta,
                    new_balance,
                    old_balance,
                    beneficiary,
                )?;
            }
            graft.accounts += accounts.len();
            state.bundle_state.state = accounts;
            state.bundle_state.state_size = state_size;
            state.bundle_state.contracts.extend(contracts);
            graft.reverts.extend(
                std::mem::take(&mut *reverts)
                    .into_iter()
                    .flatten()
                    .filter(|(address, _)| *address != beneficiary),
            );
        }
    }
    let total: usize = bundles.iter().map(|b| b.state.len()).sum();
    if keep_cache {
        state.cache.accounts.reserve(total);
    }
    state.bundle_state.state.reserve(total);
    graft.reverts.reserve(total);
    let mut slow: revm::state::EvmState = Default::default();
    for bundle in bundles {
        let BundleState {
            state: accounts,
            contracts,
            reverts,
            ..
        } = bundle;
        state.bundle_state.contracts.extend(contracts);
        // Addresses this bundle changed that an earlier one had already put
        // in: their reverts are the earlier one's.
        let mut repeated: alloy_primitives::map::AddressHashSet = Default::default();
        for (address, account) in accounts {
            let Some(info) = account.info.as_ref() else {
                continue;
            };
            let (new_balance, new_nonce) = (info.balance, info.nonce);
            let (old_balance, old_nonce) = match &account.original_info {
                Some(orig) => (orig.balance, orig.nonce),
                None => (U256::ZERO, 0),
            };
            if address == beneficiary {
                if graft.beneficiary_original.is_none() {
                    graft.beneficiary_original = account.original_info.clone();
                }
                add_credit(
                    &mut graft.beneficiary_delta,
                    new_balance,
                    old_balance,
                    beneficiary,
                )?;
                repeated.insert(address);
                continue;
            }
            if state.bundle_state.state.contains_key(&address) {
                // An earlier bundle put it in (or an earlier merge did, in
                // which case the cache holds the block's view too): added to
                // what is there, in both places.
                repeated.insert(address);
                if let Some(info) = state
                    .bundle_state
                    .state
                    .get_mut(&address)
                    .and_then(|a| a.info.as_mut())
                {
                    apply_delta(
                        info,
                        new_balance,
                        old_balance,
                        new_nonce,
                        old_nonce,
                        address,
                    )?;
                }
                if let Some(info) = state
                    .cache
                    .accounts
                    .get_mut(&address)
                    .and_then(|a| a.account.as_mut())
                {
                    apply_delta(
                        &mut info.info,
                        new_balance,
                        old_balance,
                        new_nonce,
                        old_nonce,
                        address,
                    )?;
                }
                continue;
            }
            if let Some(cached) = state.cache.accounts.get(&address) {
                // The block's state has its own view of this account; a
                // delta through the ordinary path.
                repeated.insert(address);
                let existed = cached.account.is_some();
                let mut merged = slow
                    .get(&address)
                    .map(|account| account.info.clone())
                    .unwrap_or_else(|| {
                        cached
                            .account
                            .as_ref()
                            .map(|a| a.info.clone())
                            .unwrap_or_default()
                    });
                apply_delta(
                    &mut merged,
                    new_balance,
                    old_balance,
                    new_nonce,
                    old_nonce,
                    address,
                )?;
                let mut acc = if existed {
                    Account::from(merged.clone())
                } else {
                    Account::new_not_existing(revm::state::TransactionId::ZERO)
                };
                acc.info = merged;
                acc.mark_touch();
                slow.insert(address, acc);
                continue;
            }
            if keep_cache {
                state.cache.accounts.insert(
                    address,
                    CacheAccount {
                        account: Some(PlainAccount {
                            info: info.clone(),
                            storage: Default::default(),
                        }),
                        status: account.status,
                    },
                );
            }
            state.bundle_state.state_size += account.size_hint();
            state.bundle_state.state.insert(address, account);
            graft.accounts += 1;
        }
        let mut reverts = reverts;
        for (address, revert) in std::mem::take(&mut *reverts).into_iter().flatten() {
            if !repeated.contains(&address) {
                graft.reverts.push((address, revert));
            }
        }
    }
    if !slow.is_empty() {
        graft.committed = slow.len();
        state.commit(slow);
    }
    Ok(graft)
}

/// Appends a graft's reverts to a taken bundle's revert set for the block
/// (the last one, which the state's merge created; a new one if the merge
/// found nothing to revert). An account the block touched again after the
/// graft (a later transaction's sender, a withdrawal's recipient) got a
/// second revert from the merge, back to the grafted value: that one is
/// dropped, since the block's revert is to the parent's value, which the
/// graft's carries -- and two entries for one account in a block's
/// changeset fail persistence's history index (round 43, `UnsortedInput`).
/// The set is sorted by address, as the merge leaves it.
pub fn append_reverts(bundle: &mut BundleState, mut reverts: Vec<(Address, AccountRevert)>) {
    if reverts.is_empty() {
        return;
    }
    if bundle.reverts.is_empty() {
        bundle.reverts.push(Vec::new());
    }
    let last = bundle.reverts.len() - 1;
    let merged = &mut bundle.reverts[last];
    if !merged.is_empty() {
        let grafted: alloy_primitives::map::AddressHashMap<usize> = reverts
            .iter()
            .enumerate()
            .map(|(index, (address, _))| (*address, index))
            .collect();
        merged.retain_mut(|(address, later)| {
            let Some(&index) = grafted.get(address) else {
                return true;
            };
            let early = &mut reverts[index].1;
            // A serial tail can CREATE at an EOA funded by the prefix. Keep the
            // earlier account original, but retain storage first changed later.
            for (slot, original) in std::mem::take(&mut later.storage) {
                early.storage.entry(slot).or_insert(original);
            }
            early.wipe_storage |= later.wipe_storage;
            false
        });
    }
    merged.extend(reverts);
    // Sorted by address as revm's own merge leaves them; on the worker pool,
    // a block's 147,000 reverts being too many for one thread on the
    // follower's critical path.
    if merged.len() >= 4096 {
        use rayon::prelude::*;
        merged.par_sort_unstable_by_key(|(address, _)| *address);
    } else {
        merged.sort_unstable_by_key(|(address, _)| *address);
    }
    bundle.reverts_size = bundle
        .reverts
        .iter()
        .flatten()
        .map(|(_, revert)| revert.size_hint())
        .sum();
}

/// Executes candidate transfers for a block being built, one group per
/// sender ([`partition_by_sender`]), the groups spread over batches on the
/// worker pool, each batch on its own view of the parent's state from `open`
/// and yielding its own bundle.
///
/// Unlike the full-block import path, which must reproduce a sealed block exactly,
/// this may drop candidates: one the transfer path refuses is reported in
/// `skipped` together with that sender's remaining suffix. Transfers to another
/// sender join their dependency components and retain candidate order; passive
/// shared recipients remain parallel. A following serial builder may retry the
/// skipped candidates on the merged state, in its resulting new block order.
///
/// The caller grafts the bundles onto the block's state with
/// [`graft_bundles`]: committing each transfer's state on its own is what
/// the serial path spends half its time on (round 43: 112 ms of execution,
/// 110 ms of commits and 55 ms of transition merging for 163,000 transfers),
/// and one commit of the folded changes ([`fold_bundles`]) costs the same.
///
/// Returns `Err` when the candidates are not all plain transfers away from
/// the beneficiary: then the serial builder takes all of them.
pub fn execute_for_build<T, G>(
    evm_env: &reth_evm::EvmEnv,
    keys: &[(Address, Address)],
    convert: &(dyn Fn(usize) -> (T, TxEnv) + Sync),
    open: &(dyn Fn() -> Option<G> + Sync),
) -> Result<BuildRun<T>, NotParallel>
where
    T: Send + Sync,
    G: Database + std::fmt::Debug + Send,
    G::Error: std::fmt::Display + Send + Sync + 'static,
{
    let beneficiary = evm_env.block_env.beneficiary;
    let mut phases = Phases::default();
    let at = std::time::Instant::now();
    let groups = partition_by_sender(keys, beneficiary)?;
    phases.groups = groups.len();
    // Batches of whole groups, about equal in transfers: a couple of
    // thousand transfers each, at most two per worker. Each batch opens its
    // own view of the parent, which is not free.
    let pool = build_pool();
    let workers = pool.current_num_threads().max(1);
    let batches = batch_groups(&groups, keys.len(), workers);
    phases.batches = batches.len();
    phases.partition_ms = at.elapsed().as_millis() as u64;

    let at = std::time::Instant::now();
    // Each result goes into its candidate's slot from the batch's own
    // thread: collecting the batches' vectors and sorting them by index was
    // 80-150 ms of a full block's build (loop138-139).
    let slots: Vec<std::sync::OnceLock<BuiltTransfer<T>>> = (0..keys.len())
        .map(|_| std::sync::OnceLock::new())
        .collect();
    let slots_ref = &slots;
    let results: Vec<Result<(Vec<usize>, BundleState), NotParallel>> = pool.install(|| {
        use rayon::prelude::*;
        batches
            .par_iter()
            .map(|members| {
                let db = open().ok_or(NotParallel::NoState)?;
                let mut state = State::builder()
                    .with_database(db)
                    .with_bundle_update()
                    .build();
                let mut skipped = Vec::new();
                {
                    let mut evm = N42EvmFactory::with_fast_transfers(true)
                        .create_evm(&mut state, evm_env.clone());
                    let mut refused_senders = alloy_primitives::map::AddressHashSet::default();
                    for group in members {
                        for &i in group.iter() {
                            if refused_senders.contains(&keys[i].0) {
                                skipped.push(i);
                                continue;
                            }
                            // Converted here, on the batch's thread: the
                            // conversion of a full block was 55-100 ms of
                            // the builder's own thread otherwise.
                            let (tx, env) = convert(i);
                            if env.caller != keys[i].0 || env.kind.to() != Some(&keys[i].1) {
                                return Err(NotParallel::Failed(
                                    i,
                                    "candidate identity changed during conversion".into(),
                                ));
                            }
                            match evm.transfer(&env) {
                                Ok(Some(out)) => {
                                    let gas_used = out.result.gas().tx_gas_used();
                                    evm.db_mut().commit(out.state);
                                    if slots_ref[i]
                                        .set(BuiltTransfer {
                                            index: i,
                                            tx,
                                            result: out.result,
                                            gas_used,
                                        })
                                        .is_err()
                                    {
                                        return Err(NotParallel::Failed(
                                            i,
                                            "executed twice".to_string(),
                                        ));
                                    }
                                }
                                Ok(None) => {
                                    // The sender's later transfers would only
                                    // fail their nonce check: skipped unrun.
                                    skipped.push(i);
                                    refused_senders.insert(env.caller);
                                }
                                Err(err) => return Err(NotParallel::Failed(i, err.to_string())),
                            }
                        }
                    }
                }
                state.merge_transitions(BundleRetention::Reverts);
                Ok((skipped, state.take_bundle()))
            })
            .collect()
    });
    phases.groups_ms = at.elapsed().as_millis() as u64;

    let at = std::time::Instant::now();
    let mut run = BuildRun {
        phases,
        ..Default::default()
    };
    for r in results {
        let (skipped, bundle) = r?;
        run.skipped.extend(skipped);
        run.bundles.push(bundle);
    }
    // Candidate order, as the serial builder would have laid the block out
    // (each sender's transfers were run in that order, and the graft does
    // not care): round 43's followers imported a sender-grouped block 35%
    // slower than the serial builder's. The slots are in that order already.
    run.executed = slots
        .into_iter()
        .filter_map(|slot| slot.into_inner())
        .collect();
    run.skipped.sort_unstable();
    run.phases.collect_ms = at.elapsed().as_millis() as u64;
    Ok(run)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Bytes, TxKind, address};
    use revm::{
        context::{BlockEnv, CfgEnv},
        database::{CacheDB, EmptyDB},
        state::AccountInfo,
    };
    const A: Address = address!("1000000000000000000000000000000000000001");
    const B: Address = address!("2000000000000000000000000000000000000002");
    const C: Address = address!("3000000000000000000000000000000000000003");
    const BENEFICIARY: Address = address!("4000000000000000000000000000000000000004");

    fn environment() -> alloy_evm::EvmEnv {
        alloy_evm::EvmEnv::new(
            CfgEnv::new_with_spec(revm::primitives::hardfork::SpecId::OSAKA),
            BlockEnv {
                beneficiary: BENEFICIARY,
                basefee: 0,
                gas_limit: 200_000_000,
                ..Default::default()
            },
        )
    }
    fn db() -> CacheDB<EmptyDB> {
        let mut db = CacheDB::new(EmptyDB::default());
        for (address, balance) in [(A, 1000), (B, 0), (C, 10), (BENEFICIARY, 1)] {
            db.insert_account_info(
                address,
                AccountInfo {
                    balance: U256::from(balance),
                    ..Default::default()
                },
            );
        }
        db
    }
    fn transfer(caller: Address, to: Address, nonce: u64, value: u64) -> TxEnv {
        TxEnv {
            caller,
            kind: TxKind::Call(to),
            nonce,
            value: U256::from(value),
            gas_limit: 21_000,
            gas_price: 0,
            data: Bytes::new(),
            chain_id: Some(1),
            ..Default::default()
        }
    }
    fn run(txs: &[TxEnv]) -> BuildRun<()> {
        let keys: Vec<_> = txs
            .iter()
            .map(|t| (t.caller, *t.kind.to().unwrap()))
            .collect();
        execute_for_build(&environment(), &keys, &|i| ((), txs[i].clone()), &|| {
            Some(db())
        })
        .unwrap()
    }

    #[test]
    fn a_future_credit_cannot_fund_an_earlier_spend() {
        let txs = [
            transfer(A, B, 0, 1),
            transfer(B, C, 0, 100),
            transfer(A, B, 1, 99),
        ];
        let run = run(&txs);
        // In candidate order B has one unit, not the 100 it will receive later.
        assert!(
            !run.executed.iter().any(|tx| tx.index == 1),
            "future credit funded an invalid earlier transaction"
        );
    }

    #[test]
    fn cached_recipient_receives_every_batch_credit() {
        let mut parent = State::builder()
            .with_database(db())
            .with_bundle_update()
            .build();
        assert_eq!(parent.basic(C).unwrap().unwrap().balance, U256::from(10));
        let bundle = |value: u64| {
            let mut state = State::builder()
                .with_database(db())
                .with_bundle_update()
                .build();
            {
                let mut evm =
                    N42EvmFactory::with_fast_transfers(true).create_evm(&mut state, environment());
                let out = evm.transfer(&transfer(A, C, 0, value)).unwrap().unwrap();
                let mut changes = out.state;
                changes.remove(&A); // Two different sender groups' shared-recipient deltas.
                evm.db_mut().commit(changes);
            }
            state.merge_transitions(BundleRetention::Reverts);
            state.take_bundle()
        };
        graft_bundles(&mut parent, vec![bundle(5), bundle(7)], BENEFICIARY).unwrap();
        assert_eq!(parent.basic(C).unwrap().unwrap().balance, U256::from(22));
    }

    #[test]
    fn dependent_senders_keep_candidate_order_but_passive_recipients_do_not_serialize() {
        assert_eq!(
            partition_by_sender(&[(A, C), (B, C), (A, C)], BENEFICIARY).unwrap(),
            vec![vec![0, 2], vec![1]]
        );
        assert_eq!(
            partition_by_sender(&[(A, B), (B, C), (A, B)], BENEFICIARY).unwrap(),
            vec![vec![0, 1, 2]]
        );
        let txs = [
            transfer(A, B, 0, 1),
            transfer(B, C, 0, 100),
            transfer(A, B, 1, 99),
        ];
        let result = run(&txs);
        assert_eq!(
            result
                .executed
                .iter()
                .map(|tx| tx.index)
                .collect::<Vec<_>>(),
            vec![0, 2]
        );
        assert_eq!(result.skipped, vec![1]);
    }

    fn complete_comparison(txs: &[TxEnv], cached: &[Address], reward: Option<Address>) {
        let mut database = db();
        for address in [A, B] {
            database.insert_account_info(
                address,
                AccountInfo {
                    balance: U256::from(1_000_000_000u64),
                    ..Default::default()
                },
            );
        }
        let keys: Vec<_> = txs
            .iter()
            .map(|t| (t.caller, *t.kind.to().unwrap()))
            .collect();
        let run = execute_for_build(&environment(), &keys, &|i| ((), txs[i].clone()), &|| {
            Some(database.clone())
        })
        .unwrap();
        assert!(run.skipped.is_empty());
        assert_eq!(
            run.executed.iter().map(|t| t.index).collect::<Vec<_>>(),
            (0..txs.len()).collect::<Vec<_>>()
        );
        let mut parallel = State::builder()
            .with_database(database.clone())
            .with_bundle_update()
            .build();
        let mut serial = State::builder()
            .with_database(database)
            .with_bundle_update()
            .build();
        for address in cached {
            parallel.basic(*address).unwrap();
            serial.basic(*address).unwrap();
        }
        // Compare the actual state notification stream's final account view.
        // A graft used to omit every directly inserted account from this stream.
        fn observe(
            state: &mut State<CacheDB<EmptyDB>>,
        ) -> std::sync::Arc<std::sync::Mutex<revm::state::EvmState>> {
            let view = std::sync::Arc::new(std::sync::Mutex::new(revm::state::EvmState::default()));
            let sink = view.clone();
            state.set_state_hook(Some(Box::new(move |changes: revm::state::EvmState| {
                sink.lock()
                    .unwrap()
                    .extend(changes.into_iter().filter(|(_, a)| a.is_touched()));
            })));
            view
        }
        let parallel_updates = observe(&mut parallel);
        let serial_updates = observe(&mut serial);
        let mut results = Vec::new();
        {
            let mut evm =
                N42EvmFactory::with_fast_transfers(false).create_evm(&mut serial, environment());
            for tx in txs {
                let out = evm.transact_raw(tx.clone()).unwrap();
                assert!(out.result.is_success());
                results.push(out.result);
                evm.db_mut().commit(out.state);
            }
        }
        assert_eq!(
            run.executed.iter().map(|t| &t.result).collect::<Vec<_>>(),
            results.iter().collect::<Vec<_>>()
        );
        // Large cases pack independent sender groups into separate worker batches.
        let graft = graft_bundles(&mut parallel, run.bundles, BENEFICIARY).unwrap();
        if !graft.beneficiary_delta.is_zero() {
            let mut account = Account::from(parallel.basic(BENEFICIARY).unwrap().unwrap());
            account.info.balance += graft.beneficiary_delta;
            account.mark_touch();
            parallel.commit([(BENEFICIARY, account)].into_iter().collect());
        }
        for state in [&mut parallel, &mut serial] {
            if let Some(address) = reward {
                let mut account = Account::from(state.basic(address).unwrap().unwrap());
                account.info.balance += U256::from(1_000);
                account.mark_touch();
                state.commit([(address, account)].into_iter().collect());
            }
            state.merge_transitions(BundleRetention::Reverts);
        }
        let parallel_updates = parallel_updates.lock().unwrap();
        let serial_updates = serial_updates.lock().unwrap();
        assert_eq!(
            parallel_updates.len(),
            serial_updates.len(),
            "root hook covers every changed account"
        );
        for (address, expected) in serial_updates.iter() {
            let actual = parallel_updates.get(address).expect("missing root update");
            assert_eq!(
                actual.info, expected.info,
                "root hook final info for {address}"
            );
            assert_eq!(actual.storage, expected.storage);
        }
        let mut parallel_bundle = parallel.take_bundle();
        append_reverts(&mut parallel_bundle, graft.reverts);
        let mut serial_bundle = serial.take_bundle();
        // Revm's transition map iteration order is unspecified; compare the
        // entire revert records after canonicalizing only their address order.
        for reverts in serial_bundle.reverts.iter_mut() {
            reverts.sort_unstable_by_key(|(address, _)| *address);
        }
        assert_eq!(
            parallel_bundle, serial_bundle,
            "all account originals, statuses, storage and reverts"
        );
        let addresses: Vec<_> = parallel_bundle.reverts[0]
            .iter()
            .map(|(address, _)| address)
            .collect();
        assert!(
            addresses.windows(2).all(|pair| pair[0] < pair[1]),
            "exactly one sorted revert per address"
        );
    }

    #[test]
    fn batch_graft_matches_serial_with_cached_accounts_rewards_and_repeated_touches() {
        let recipient = address!("5000000000000000000000000000000000000005");
        let mut txs = vec![
            transfer(A, C, 0, 5),
            transfer(B, C, 0, 7),
            transfer(A, recipient, 1, 9),
            transfer(B, recipient, 1, 11),
        ];
        for tx in &mut txs {
            tx.gas_price = 1;
        }
        for cached in [vec![], vec![A, C, recipient]] {
            for reward in [None, Some(A), Some(C), Some(recipient)] {
                complete_comparison(&txs, &cached, reward);
            }
        }
    }

    #[test]
    fn multiple_worker_batches_match_serial_on_shared_recipients() {
        let mut txs = Vec::new();
        for nonce in 0..2500 {
            for caller in [A, B] {
                let mut tx = transfer(caller, C, nonce, 1);
                tx.gas_price = 1;
                txs.push(tx);
            }
        }
        complete_comparison(&txs, &[C], Some(C));
    }

    #[test]
    fn changed_candidate_identity_aborts_instead_of_using_another_partition() {
        let keys = [(A, C)];
        let result = execute_for_build(
            &environment(),
            &keys,
            &|_| ((), transfer(B, C, 0, 1)),
            &|| Some(db()),
        );
        assert!(matches!(result, Err(NotParallel::Failed(0, _))));
    }

    #[test]
    fn merged_balance_overflow_is_an_error() {
        let mut state = State::builder()
            .with_database(db())
            .with_bundle_update()
            .build();
        let make = |balance| {
            let original = AccountInfo {
                balance: U256::MAX - U256::from(10),
                ..Default::default()
            };
            BundleState::builder(0..=0)
                .state_original_account_info(C, original.clone())
                .state_present_account_info(
                    C,
                    AccountInfo {
                        balance,
                        ..original
                    },
                )
                .build()
        };
        assert!(matches!(graft_bundles(&mut state,
            vec![make(U256::MAX - U256::from(3)), make(U256::MAX - U256::from(3))], BENEFICIARY),
            Err(MergeError::Arithmetic(address)) if address == C));
    }
}
