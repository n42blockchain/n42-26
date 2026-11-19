//! The parts of the parallel transfer step that do not touch the EVM: why a
//! block is not taken, the sender partition, the worker pool, the batching
//! and the revert helpers over the bundle state.
//!
//! The partition and the pool are this chain's (a recipient that also sends
//! joins its funders' group); the revert helpers are lifted from n42-rs
//! `crates/n42/engine-types/src/parallel_common.rs`.

use alloy_primitives::Address;
use evm2::block::bundle_state::{AccountRevert, BundleState};

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

/// Groups candidate transfers by sender: every sender's transfers, in
/// candidate order, form one group. If a recipient is also a sender, the
/// affected sender groups are joined and retain candidate order. `keys` is
/// each candidate's (sender, recipient). Passive recipients do not join groups -- a transfer only adds to its
/// recipient's balance, and additions commute, so the merge ([`crate::graft_fold::merge_batches`]) can fold
/// batches that share a recipient in any order. (Grouping every
/// recipient by connected component merges a full block of random transfers
/// into a handful of giant groups: round 43.)
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

/// Appends a graft's reverts to a taken bundle's revert set for the block
/// (the last one, which the state's merge created; a new one if the merge
/// found nothing to revert). An account the block touched again after the
/// graft (a later transaction's sender, a withdrawal's recipient) got a
/// second revert from the merge, back to the grafted value: that one is
/// dropped, since the block's revert is to the parent's value, which the
/// graft's carries -- and two entries for one account in a block's
/// changeset fail persistence's history index (round 43, `UnsortedInput`).
/// The set is sorted by address, as the merge leaves it.
pub fn append_reverts(bundle: &mut BundleState, reverts: Vec<(Address, AccountRevert)>) {
    if reverts.is_empty() {
        return;
    }
    // Sorted by address; on the worker pool,
    // a block's 147,000 reverts being too many for one thread on the
    // follower's critical path.
    let mut reverts = reverts;
    sort_reverts(&mut reverts);
    append_sorted_reverts(bundle, reverts);
}

/// [`append_reverts`] for a set already sorted by address.
pub(crate) fn append_sorted_reverts(bundle: &mut BundleState, reverts: Vec<(Address, AccountRevert)>) {
    if reverts.is_empty() {
        return;
    }
    if bundle.reverts.is_empty() {
        bundle.reverts.push(Vec::new());
    }
    let last = bundle.reverts.len() - 1;
    let mut reverts = reverts;
    let merged = &mut bundle.reverts[last];
    if merged.is_empty() {
        // The block's merge had nothing of its own to revert: the graft's set
        // becomes the block's, moved rather than copied into it.
        *merged = reverts;
    } else {
        // The block's own reverts are a handful -- the beneficiary, the
        // withdrawals' recipients -- so each is looked up in the graft's
        // sorted set rather than the graft's 147,000 being put in a set of
        // their own (5-8 ms of a follower's import, loop174).
        let mut few: Vec<(Address, AccountRevert)> = std::mem::take(merged)
            .into_iter()
            .filter(|(address, _)| reverts.binary_search_by_key(address, |(address, _)| *address).is_err())
            .collect();
        few.sort_unstable_by_key(|(address, _)| *address);
        // Merged into the graft's set from the back: every entry of it moves
        // at most once, and no second buffer of 147,000 reverts is allocated.
        let (mut i, mut k) = (reverts.len(), reverts.len() + few.len());
        reverts.resize_with(k, Default::default);
        while let Some(entry) = few.pop() {
            while i > 0 && reverts[i - 1].0 > entry.0 {
                reverts.swap(k - 1, i - 1);
                i -= 1;
                k -= 1;
            }
            reverts[k - 1] = entry;
            k -= 1;
        }
        *merged = reverts;
    }
    bundle.reverts_size = bundle.reverts.iter().flatten().map(|(_, revert)| revert.size_hint()).sum();
}

/// By address, on the worker pool where there are enough of them to pay for it.
pub(crate) fn sort_reverts(reverts: &mut [(Address, AccountRevert)]) {
    if reverts.len() >= 4096 {
        use rayon::prelude::*;
        reverts.par_sort_unstable_by_key(|(address, _)| *address);
    } else {
        reverts.sort_unstable_by_key(|(address, _)| *address);
    }
}
