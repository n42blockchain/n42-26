//! The parallel transfer step: a block's plain transfers executed in sender
//! batches beside each other, and handed to the block's own executor.
//!
//! Lifted from n42-rs `crates/n42/engine-types/src/parallel_build.rs` and cut
//! to what this chain runs: the batches, the cache half, the receipts'
//! commit and the follower's import. The partition
//! ([`crate::parallel_common::partition_by_sender`]), the refusal of a sender
//! rather than of its group, the candidate identity check, the touched
//! accounts and the restored-slot record are this chain's.
//!
//! A block's plain transfers are partitioned by sender, the groups packed
//! into batches ([`crate::parallel_common::batch_groups`]), and each batch
//! runs on the build pool over its own view of the parent's state, its
//! changes folded into a [`BatchState`]. A transfer the EVM refuses (a nonce
//! that is not the sender's, a balance short, a recipient whose code ran) is
//! skipped with every later transfer of its sender in that batch, and left
//! to the serial loop. The batches' bundles are merged
//! ([`crate::graft_fold::merge_batches`]); [`commit_run`] writes the merged
//! accounts into the block executor's overlay (the cache half), commits the
//! transfers' receipts in candidate order, and hands the merge to the
//! executor so that `finish` folds it into the block's bundle (the bundle
//! half, [`crate::graft_fold::fold_into`]).
//!
//! Nothing here validates a header, a signature or a certificate, and
//! neither a run nor its commit means a block has passed consensus.

use crate::{
    batch_state::{BatchState, Unsupported},
    direct_transfer::{DirectTransfers, Layered, Step},
    graft_fold::{FoldRefused, MergedBatches},
    parallel_common::{batch_groups, build_pool, partition_by_sender, NotParallel},
    restored_slots::TrackingExecutor,
    N42EvmConfig,
};
use alloy_consensus::transaction::TxHashRef;
use alloy_primitives::{Address, B256};
use evm2::{block::ethereum::EthTransactionResultWithState, evm::AccountInfo};
use n42_tx_types::{Block, N42TxEnvelope, N42TxType, Receipt};
use reth_evm::{
    execute::{BlockExecutionOutput, BlockExecutor},
    BlockExecutorFactory, ConfigureEvm, Evm as _, EvmEnv as _, EvmEnvFor,
};
use reth_primitives_traits::{Recovered, RecoveredBlock};

/// The EVM type family a configuration's executors run on.
pub type TypesOf<C> = <<C as ConfigureEvm>::BlockExecutorFactory as BlockExecutorFactory>::EvmTypes;

/// The transaction type a configuration's EVM executes.
pub type EvmTxOf<C> = <TypesOf<C> as evm2::EvmTypesHost>::Tx;

/// One transfer the batches executed: its index among the candidates and the
/// EVM's result, for its receipt. Its state changes are in its batch's
/// bundle.
#[derive(Debug)]
pub struct Executed<C: ConfigureEvm> {
    /// Index into the candidates.
    pub index: usize,
    /// The EVM's result.
    pub result: evm2::TxResult<TypesOf<C>>,
}

/// The run's phase timings, in milliseconds.
#[derive(Debug, Clone, Copy, Default)]
pub struct Phases {
    /// Partitioning the candidates by sender and packing the batches.
    pub partition_ms: u64,
    /// The batches' execution on the worker pool, wall time, with the merge
    /// of each batch's bundle as it finished.
    pub groups_ms: u64,
    /// Of `groups_ms`: opening the parent's state, summed over the batches.
    pub open_ms_sum: u64,
    /// Of `groups_ms`: the transfers themselves (reads included), summed
    /// over the batches.
    pub run_ms_sum: u64,
    /// Of `groups_ms`: each batch's bundle, summed.
    pub batch_merge_ms_sum: u64,
    /// The longest batch, wall time.
    pub batch_ms_max: u64,
    /// After the last batch: the merge's reverts made one sorted set and the
    /// batches' results placed in candidate order.
    pub collect_ms: u64,
    /// The batches' bundles merged, on the calling thread: each as its batch
    /// finished (inside `groups_ms`), then the reverts' set (inside
    /// `collect_ms`).
    pub merge_ms: u64,
    /// How many sender groups there were.
    pub groups: usize,
    /// How many batches of groups ran.
    pub batches: usize,
    /// Transfers the direct path applied.
    pub direct_hits: u64,
    /// Transactions run on the EVM in batches the direct path was offered.
    pub direct_delegated: u64,
}

/// What [`execute_batches`] produced.
#[derive(Debug)]
pub struct ParallelRun<C: ConfigureEvm> {
    /// The executed transfers in candidate order, the order they take in the
    /// block.
    pub executed: Vec<Executed<C>>,
    /// Candidates the EVM refused, with every later one of the same sender
    /// in its batch: left to the serial loop, in candidate order.
    pub skipped: Vec<usize>,
    /// The batches' changes, merged and checked.
    pub merged: MergedBatches,
    /// Accounts the transfers touched and left unchanged, for the block's
    /// bundle (`restored_slots`).
    pub touched: Vec<(Address, AccountInfo)>,
    /// Phase timings.
    pub phases: Phases,
}

/// Why the parallel step gave the block to the serial path.
#[derive(Debug)]
pub enum Refused {
    /// The candidates could not be partitioned or a batch could not run.
    NotParallel(NotParallel),
    /// A batch made a change the batch state does not model.
    Unsupported(Unsupported),
    /// The batches' bundles could not be merged, or the fold refused them.
    Fold(FoldRefused),
    /// The block executor's overlay already holds an account the batches
    /// changed at a value other than the one they started from (a
    /// pre-execution system call changed it).
    Cache(Address),
    /// A receipt's commit failed after the cache half was written: the
    /// executor is left part-way and cannot go on.
    Commit(String),
    /// The block's rules are not the ones the batches reproduce (Amsterdam's
    /// transfer gas and block access list).
    Fork,
    /// The transaction at this index does not fit the block's remaining gas
    /// at its position: the stock import refuses the block.
    InclusionGas(usize),
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotParallel(err) => err.fmt(f),
            Self::Unsupported(err) => err.fmt(f),
            Self::Fold(err) => err.fmt(f),
            Self::Cache(address) => write!(f, "the executor already holds {address} at another value"),
            Self::Commit(err) => write!(f, "a receipt's commit failed: {err}"),
            Self::Fork => write!(f, "the block's fork is not one the batches reproduce"),
            Self::InclusionGas(i) => write!(f, "transaction {i}'s gas limit is over the block's remaining gas"),
        }
    }
}

impl std::error::Error for Refused {}

impl Refused {
    /// A short name for the per-block line and the metric's label.
    pub const fn reason(&self) -> &'static str {
        match self {
            Self::NotParallel(_) => "not-parallel",
            Self::Unsupported(_) => "unsupported-change",
            Self::Fold(_) => "merge-refused",
            Self::Cache(_) => "cache-conflict",
            Self::Commit(_) => "commit-failed",
            Self::Fork => "fork",
            Self::InclusionGas(_) => "inclusion-gas",
        }
    }

    /// Whether the block executor was left as it was: the caller can go on
    /// with it serially. Only a failed commit leaves it part-way.
    pub const fn executor_untouched(&self) -> bool {
        !matches!(self, Self::Commit(_))
    }
}

fn same_account(a: &AccountInfo, b: &AccountInfo) -> bool {
    a.balance == b.balance && a.nonce == b.nonce && a.code_hash == b.code_hash
}

/// The cache half: the merged accounts written into the block executor's
/// overlay at their post-batch values, so that whatever runs after them
/// (the serial loop, the withdrawals, the post-block system calls) reads
/// them there and starts from where the batches left them. Refused, with
/// nothing written, when the overlay already holds one of them at a value
/// other than the parent's the batches started from: the batches' changes
/// would then not be the block's.
pub fn install_cache<X>(overlay: &mut evm2::evm::CacheDB<X>, merged: &MergedBatches) -> Result<usize, Address> {
    let addresses: Vec<Address> = merged.accounts().map(|(address, _)| *address).collect();
    install_entries(overlay, merged, addresses.iter().copied(), true)
}

/// [`install_cache`]'s check for every merged account, and its write for the
/// merged accounts among `addresses` only.
pub fn install_cache_only<X>(
    overlay: &mut evm2::evm::CacheDB<X>,
    merged: &MergedBatches,
    addresses: &[Address],
) -> Result<usize, Address> {
    // The overlay holds a handful of accounts here (the system calls'), so
    // they are looked up among the merged ones rather than the reverse.
    for (address, held) in &overlay.cache.accounts {
        if let Some(original) = merged.original(address)
            && !agrees(held.as_ref(), original)
        {
            return Err(*address);
        }
    }
    install_entries(overlay, merged, addresses.iter().copied(), false)
}

fn agrees(held: Option<&AccountInfo>, original: Option<&AccountInfo>) -> bool {
    match (held, original) {
        (Some(held), Some(original)) => same_account(held, original),
        (None, None) => true,
        // Loaded as absent and absent in the parent read alike.
        (Some(held), None) => held.is_empty(),
        (None, Some(original)) => original.is_empty(),
    }
}

fn install_entries<X>(
    overlay: &mut evm2::evm::CacheDB<X>,
    merged: &MergedBatches,
    addresses: impl Iterator<Item = Address> + Clone,
    check: bool,
) -> Result<usize, Address> {
    if check {
        for address in addresses.clone() {
            let Some(original) = merged.original(&address) else { continue };
            if let Some(held) = overlay.cache.accounts.get(&address)
                && !agrees(held.as_ref(), original)
            {
                return Err(address);
            }
        }
    }
    let mut written = 0;
    for address in addresses {
        if let Some(info) = merged.get(&address) {
            overlay.insert_account_info(&address, info.clone());
            written += 1;
        }
    }
    Ok(written)
}

/// The gas of a plain transfer: what every transfer the batches keep used.
pub const TRANSFER_GAS: u64 = 21_000;

/// The stock executor's inclusion check (`validate_transaction_gas_limit`,
/// which `commit_transaction` does not run, so the batches' receipts skip
/// it): a transaction's gas limit (capped by EIP-7825) against the block's
/// remaining gas at its position.
#[derive(Debug, Clone, Copy)]
pub struct InclusionGas {
    block_gas_limit: u64,
    tx_gas_limit_cap: u64,
}

impl InclusionGas {
    /// The check for a block on `version` with `block_gas_limit`.
    pub const fn of(version: &evm2::Version, block_gas_limit: u64) -> Self {
        Self { block_gas_limit, tx_gas_limit_cap: version.tx_gas_limit_cap }
    }

    /// `validate_transaction_gas_limit` with `cumulative` gas used so far.
    pub const fn fits(&self, gas_limit: u64, cumulative: u64) -> bool {
        let regular_limit = if gas_limit < self.tx_gas_limit_cap { gas_limit } else { self.tx_gas_limit_cap };
        regular_limit <= self.block_gas_limit.saturating_sub(cumulative)
    }

    /// The check over transactions in block order from an empty block, each
    /// as (index, gas limit, gas used). `Err` names the first that does not
    /// fit.
    pub fn check(&self, spent: impl IntoIterator<Item = (usize, u64, u64)>) -> Result<(), usize> {
        let mut cumulative = 0u64;
        for (index, gas_limit, used) in spent {
            if !self.fits(gas_limit, cumulative) {
                return Err(index);
            }
            cumulative = cumulative.saturating_add(used);
        }
        Ok(())
    }
}

/// Which merged accounts the cache half writes into the executor's overlay.
#[derive(Debug, Clone, Copy)]
pub enum CacheScope<'a> {
    /// Every one: whatever runs after the batches may read any of them (a
    /// serial loop).
    All,
    /// Only these: after the batches only the block's end runs, which reads
    /// the withdrawal recipients and the system contracts -- never another
    /// batch account, the merge refuses code. An account the end reads that
    /// is not written here makes the fold refuse (`Stale`).
    Only(&'a [Address]),
}

/// The gas [`commit_run`] committed.
#[derive(Debug, Clone, Copy, Default)]
pub struct Committed {
    /// Gas used, as the receipts cumulate it.
    pub gas_used: u64,
    /// Regular gas used, for the block's capacity.
    pub regular_gas_used: u64,
    /// State gas used, for the block's capacity.
    pub state_gas_used: u64,
}

/// Hands a run to the block executor: the cache half, then each executed
/// transfer's receipt and gas in block order (a commit with an empty write
/// set: the state is in the merge; its hash goes into the restored-slot
/// record), then the merge and the touched accounts for `finish` to fold.
/// The batches must have run on this executor's exact pre-transaction state,
/// and nothing may have been committed before them. Refused with nothing
/// changed when the cache half is ([`Refused::Cache`]); a
/// [`Refused::Commit`] leaves the executor part-way.
pub fn commit_run<'a, E, T>(
    executor: &mut TrackingExecutor<E>,
    executed: impl IntoIterator<Item = (B256, N42TxType, evm2::TxResult<T>)>,
    merged: MergedBatches,
    touched: Vec<(Address, AccountInfo)>,
    cache: CacheScope<'_>,
) -> Result<Committed, Refused>
where
    T: evm2::EvmTypes<TxResultExt: Default>,
    E: BlockExecutor<
            Evm = evm2::Evm<'a, T>,
            TransactionResultWithState = EthTransactionResultWithState<T, N42TxType>,
        >,
    E::Transaction: TxHashRef,
{
    if !executor.receipts().is_empty() || executor.has_pending_fold() {
        return Err(Refused::NotParallel(NotParallel::Failed(
            0,
            "a transfer batch must precede the serial transactions".to_owned(),
        )));
    }
    let overlay = executor.evm_mut().overlay_db_mut();
    match cache {
        CacheScope::All => install_cache(overlay, &merged),
        CacheScope::Only(addresses) => install_cache_only(overlay, &merged, addresses),
    }
    .map_err(Refused::Cache)?;
    let mut committed = Committed::default();
    for (hash, tx_type, result) in executed {
        let detached = evm2::TxResultWithState::<T> { result, ..Default::default() };
        let out = executor
            .commit_preexecuted_transfer(hash, EthTransactionResultWithState::new(detached, tx_type, 0))
            .map_err(|err| Refused::Commit(err.to_string()))?;
        committed.gas_used += out.tx_gas_used();
        committed.regular_gas_used += out.regular_gas_used();
        committed.state_gas_used += out.state_gas_used();
    }
    executor.set_pending_fold(merged, touched);
    Ok(committed)
}

/// What one batch hands back: the candidates it skipped, its bundle, the
/// accounts it touched and left unchanged, and its times (open, run, bundle,
/// whole; microseconds) with the direct path's counts.
type BatchOut =
    Result<(Vec<usize>, crate::graft_fold::CheckedBundle, Vec<(Address, AccountInfo)>, [u64; 4], (u64, u64)), Refused>;

/// Runs the candidates' transfers in batches on the build pool, each batch on
/// an EVM from `config` with `env` over a database `open` returns (a view of
/// the block's parent). `keys` are the candidates' (sender, recipient);
/// `convert` gives candidate `i` as the EVM executes it, on the batch's
/// thread.
///
/// `direct`, when given, applies each batch's plain transfers straight to
/// the batch's accounts ([`crate::direct_transfer`]); the first transaction
/// of a batch it does not apply goes to an EVM made then over the same
/// accounts, and the rest of that batch runs on it.
///
/// `Err` when the candidates are not all transfers away from the
/// beneficiary, a view of the parent cannot be opened, or the batches made a
/// change the merge does not model: nothing of the run is kept, and the
/// serial path takes every candidate.
pub fn execute_batches<C, D>(
    config: &C,
    env: &EvmEnvFor<C>,
    keys: &[(Address, Address)],
    convert: &(dyn Fn(usize) -> Recovered<EvmTxOf<C>> + Sync),
    open: &(dyn Fn() -> Option<D> + Sync),
    direct: Option<&DirectTransfers<EvmTxOf<C>>>,
) -> Result<ParallelRun<C>, Refused>
where
    C: ConfigureEvm + Sync,
    EvmEnvFor<C>: Sync,
    D: evm2::evm::Database + Send + 'static,
    evm2::TxResult<TypesOf<C>>: Send + Sync + Default,
    EvmTxOf<C>: alloy_consensus::Transaction,
{
    use evm2::EvmFeatures;
    let version = env.version();
    if version.feature(EvmFeatures::EIP2780) || version.feature(EvmFeatures::EIP8037) {
        return Err(Refused::Fork);
    }
    let beneficiary = env.block_env().beneficiary;
    let mut phases = Phases::default();
    let at = std::time::Instant::now();
    let groups = partition_by_sender(keys, beneficiary).map_err(Refused::NotParallel)?;
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
    let slots: Vec<std::sync::OnceLock<Executed<C>>> = (0..keys.len()).map(|_| std::sync::OnceLock::new()).collect();
    let micros = |at: std::time::Instant| at.elapsed().as_micros() as u64;
    let run_batch = |members: &Vec<&Vec<usize>>| -> BatchOut {
        let batch_at = std::time::Instant::now();
        // The parent's state: the direct path reads through it into the
        // batch state, and the batch's EVM, when one is made, reads it under
        // the accounts the direct path changed (`Layered::over`).
        let mut parent = Some(open().ok_or(Refused::NotParallel(NotParallel::NoState))?);
        let open_us = micros(batch_at);
        let run_at = std::time::Instant::now();
        let txs = members.iter().map(|g| g.len()).sum::<usize>();
        let (mut direct_hits, mut delegated) = (0u64, 0u64);
        // The batch's EVM, made at the first transaction the direct path
        // does not apply (at the start without it).
        let mut evm = None;
        let mut state = BatchState::with_capacity(txs * 2 + 1);
        let mut skipped = Vec::new();
        // Senders with a refused transfer: their later ones would only fail
        // their nonce check, and are skipped unrun.
        let mut refused = alloy_primitives::map::AddressHashSet::default();
        let put = |i: usize, result| {
            slots[i]
                .set(Executed { index: i, result })
                .map_err(|_| Refused::NotParallel(NotParallel::Failed(i, "executed twice".to_owned())))
        };
        for group in members {
            for &i in group.iter() {
                let (sender, to) = keys[i];
                if !refused.is_empty() && refused.contains(&sender) {
                    skipped.push(i);
                    continue;
                }
                // Converted here, on the batch's thread: the conversion of a
                // full block was 55-100 ms of the builder's own thread
                // otherwise.
                let tx = convert(i);
                if tx.signer() != sender || alloy_consensus::Transaction::to(tx.inner()) != Some(to) {
                    return Err(Refused::NotParallel(NotParallel::Failed(
                        i,
                        "candidate identity changed during conversion".to_owned(),
                    )));
                }
                if let (Some(direct), Some(parent)) = (direct, parent.as_mut())
                    && let Some(shape) = direct.shape(&tx)
                    && let Step::Applied { state_gas } = direct.apply(&shape, parent, &mut state, |_| true)
                {
                    direct_hits += 1;
                    put(i, direct.result_for(&shape, state_gas))?;
                    continue;
                }
                if direct.is_some() {
                    delegated += 1;
                }
                let evm = match &mut evm {
                    Some(evm) => evm,
                    None => {
                        let Some(parent) = parent.take() else {
                            return Err(Refused::NotParallel(NotParallel::NoState));
                        };
                        evm.insert(config.evm_with_env(Layered::over(parent, &state), env.clone()))
                    }
                };
                match evm.transact(&tx) {
                    // One that used more than a transfer's gas ran code, and
                    // code may read what another batch changes: not the
                    // batches'. Checked before the commit: a transfer
                    // refused here leaves the batch's state as it was.
                    Ok(out) if out.result.tx_gas_used() == TRANSFER_GAS => {
                        state.commit(&out.pending_state).map_err(Refused::Unsupported)?;
                        evm.commit_state(&out.pending_state);
                        put(i, out.result)?;
                    }
                    Ok(_) | Err(_) => {
                        skipped.push(i);
                        refused.insert(sender);
                    }
                }
            }
        }
        crate::direct_transfer::count(direct_hits, delegated);
        let run_us = micros(run_at);
        let merge_at = std::time::Instant::now();
        // Checked here, on the batch's thread: the merge only enters it.
        let bundle = crate::graft_fold::CheckedBundle::new(state.take_bundle()).map_err(Refused::Fold)?;
        let touched = state.take_touched();
        Ok((skipped, bundle, touched, [open_us, run_us, micros(merge_at), micros(batch_at)], (direct_hits, delegated)))
    };
    // Each batch's bundle is merged here as the batch finishes, beside the
    // batches still running: a full block's merge after the last of them was
    // 45 ms of this thread with the pool idle.
    let (mut skipped, mut touched) = (Vec::new(), Vec::new());
    let (mut open_us, mut run_us, mut merge_us, mut fold_us) = (0u64, 0u64, 0u64, 0u64);
    // Room for every sender and recipient, and for the block's own accounts
    // the fold enters afterwards.
    let mut merger = crate::graft_fold::BatchMerger::with_capacity(keys.len() * 2 + 64);
    let mut failed = None;
    let (send, results) = std::sync::mpsc::channel::<BatchOut>();
    pool.in_place_scope(|scope| {
        for members in &batches {
            let (send, run_batch) = (send.clone(), &run_batch);
            scope.spawn(move |_| {
                // The receiver outlives the scope.
                let _ = send.send(run_batch(members));
            });
        }
        drop(send);
        for result in results {
            if failed.is_some() {
                continue;
            }
            match result {
                Ok((s, bundle, t, [open, run, merge, whole], (hits, delegated))) => {
                    skipped.extend(s);
                    touched.extend(t);
                    open_us += open;
                    run_us += run;
                    merge_us += merge;
                    phases.batch_ms_max = phases.batch_ms_max.max(whole / 1000);
                    phases.direct_hits += hits;
                    phases.direct_delegated += delegated;
                    let fold_at = std::time::Instant::now();
                    if let Err(refused) = merger.add(bundle) {
                        failed = Some(Refused::Fold(refused));
                    }
                    fold_us += micros(fold_at);
                }
                Err(refused) => failed = Some(refused),
            }
        }
    });
    phases.groups_ms = at.elapsed().as_millis() as u64;
    if let Some(refused) = failed {
        return Err(refused);
    }

    let at = std::time::Instant::now();
    phases.open_ms_sum = open_us / 1000;
    phases.run_ms_sum = run_us / 1000;
    phases.batch_merge_ms_sum = merge_us / 1000;
    // The reverts' one sorted set, on the pool the batches have left.
    let merged = pool.install(|| merger.finish());
    phases.merge_ms = (fold_us + micros(at)) / 1000;
    // Candidate order, as the serial builder would have laid the block out
    // (each sender's transfers were run in that order, and the merge does
    // not care): the slots are in that order already.
    let executed: Vec<Executed<C>> = slots.into_iter().filter_map(|slot| slot.into_inner()).collect();
    skipped.sort_unstable();
    phases.collect_ms = at.elapsed().as_millis() as u64;
    Ok(ParallelRun { executed, skipped, merged, touched, phases })
}

static IMPORTS_PARALLEL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static IMPORTS_REFUSED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Imports that went parallel, since start.
pub fn imports_parallel() -> u64 {
    IMPORTS_PARALLEL.load(std::sync::atomic::Ordering::Relaxed)
}

/// Imports the parallel step refused (each then imported serially by the
/// caller), since start.
pub fn imports_refused() -> u64 {
    IMPORTS_REFUSED.load(std::sync::atomic::Ordering::Relaxed)
}

/// What a parallel import did, for its log line.
#[derive(Debug, Clone, Copy, Default)]
pub struct ImportStats {
    /// Transfers executed in batches (the block's leading plain transfers).
    pub txs: usize,
    /// Transactions executed after them by the block's executor.
    pub tail: usize,
    /// Accounts the batches changed.
    pub accounts: usize,
    /// Of those, written into the executor's overlay (the withdrawal
    /// recipients among them; every one when a tail follows).
    pub cached: usize,
    /// The batches' phases.
    pub phases: Phases,
    /// The executor's part (pre-execution, receipts, the block's end and
    /// the fold), microseconds.
    pub finish_us: u64,
}

/// A transaction's (sender, recipient) when the batches take it: a call
/// with no input, no access list, no blobs and no authorizations.
fn import_key(tx: &N42TxEnvelope, sender: Address) -> Option<(Address, Address)> {
    use alloy_consensus::Transaction as _;
    (tx.input().is_empty()
        && tx.access_list().is_none_or(|list| list.is_empty())
        && tx.blob_versioned_hashes().is_none()
        && tx.authorization_list().is_none())
    .then(|| tx.to())
    .flatten()
    .filter(|to| *to != sender)
    .map(|to| (sender, to))
}

/// The parallel import of a sealed block: its leading plain transfers --
/// every transaction up to the first that is not one or that names the
/// beneficiary -- executed in sender batches over the parent
/// ([`execute_batches`], `open` yields one view of the parent per batch),
/// then the block's own executor over `main_db` (the same parent): its
/// pre-execution changes, the transfers' receipts with the cache half, the
/// rest of the block transaction by transaction, the block's end and the
/// fold. A block of transfers only installs just the withdrawal recipients
/// in the executor's cache ([`CacheScope::Only`]); a block with a serial
/// tail installs every batch account, which the tail may read.
///
/// `Err` names why the block is not imported this way, with nothing of it
/// kept: the caller imports it serially, and a block that is genuinely
/// invalid is refused there with the stock executor's error. Signature
/// recovery and every consensus check stay with the caller.
pub fn import_block<M, D>(
    config: &N42EvmConfig,
    block: &RecoveredBlock<Block>,
    main_db: M,
    open: &(dyn Fn() -> Option<D> + Sync),
) -> Result<(BlockExecutionOutput<Receipt>, ImportStats), Refused>
where
    M: evm2::evm::Database,
    D: evm2::evm::Database + Send + 'static,
{
    if block.header().block_access_list_hash.is_some() {
        return Err(refuse(Refused::Fork));
    }
    let env = config.evm_env(block.header()).map_err(|err| refuse(Refused::Commit(err.to_string())))?;
    let ctx = config
        .context_for_block(block.sealed_block())
        .map_err(|err| refuse(Refused::Commit(err.to_string())))?;
    import_transactions(config, env, ctx, &block.body().transactions, block.senders(), main_db, open)
}

/// [`import_block`] for a block as the engine API carries it, with `senders`
/// recovered by the caller: the environment and the context are the ones the
/// engine executes the payload with.
pub fn import_payload<M, D>(
    config: &N42EvmConfig,
    payload: &alloy_rpc_types_engine::ExecutionData,
    transactions: &[N42TxEnvelope],
    senders: &[Address],
    main_db: M,
    open: &(dyn Fn() -> Option<D> + Sync),
) -> Result<(BlockExecutionOutput<Receipt>, ImportStats), Refused>
where
    M: evm2::evm::Database,
    D: evm2::evm::Database + Send + 'static,
{
    use reth_evm::ConfigureEngineEvm as _;
    if payload.payload.block_access_list().is_some() || transactions.len() != senders.len() {
        return Err(refuse(Refused::Fork));
    }
    let env = config.evm_env_for_payload(payload).map_err(|err| refuse(Refused::Commit(err.to_string())))?;
    let ctx = config.context_for_payload(payload).map_err(|err| refuse(Refused::Commit(err.to_string())))?;
    import_transactions(config, env, ctx, transactions, senders, main_db, open)
}

fn refuse(why: Refused) -> Refused {
    IMPORTS_REFUSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    why
}

fn import_transactions<M, D>(
    config: &N42EvmConfig,
    env: EvmEnvFor<N42EvmConfig>,
    ctx: reth_evm::ExecutionCtxFor<'_, N42EvmConfig>,
    transactions: &[N42TxEnvelope],
    senders: &[Address],
    main_db: M,
    open: &(dyn Fn() -> Option<D> + Sync),
) -> Result<(BlockExecutionOutput<Receipt>, ImportStats), Refused>
where
    M: evm2::evm::Database,
    D: evm2::evm::Database + Send + 'static,
{
    let refused = refuse;
    let commit_err = |err: &dyn std::fmt::Display| Refused::Commit(err.to_string());
    if !crate::fast_transfer::covers(env.spec) {
        return Err(refused(Refused::Fork));
    }
    let beneficiary = env.block.beneficiary;
    let keys: Vec<(Address, Address)> = transactions
        .iter()
        .zip(senders)
        .map_while(|(tx, sender)| import_key(tx, *sender).filter(|(from, to)| *from != beneficiary && *to != beneficiary))
        .collect();
    if keys.is_empty() {
        return Err(refused(Refused::NotParallel(NotParallel::NotATransfer(0))));
    }
    let tail = keys.len()..transactions.len();
    let tail_len = tail.len();
    let convert = |i: usize| Recovered::new_unchecked(n42_tx_types::execution::evm_tx_of(&transactions[i]), senders[i]);
    let direct = DirectTransfers::for_env(&env, crate::direct_transfer::shape_of);
    let batch_config = batch_config(config);
    let run = execute_batches(&batch_config, &env, &keys, &convert, open, direct.as_ref()).map_err(refused)?;
    if let Some(&i) = run.skipped.first() {
        return Err(refused(Refused::NotParallel(NotParallel::Failed(
            i,
            "refused by the EVM on the batches' view".to_owned(),
        ))));
    }
    // The stock import's inclusion check, which the receipts' commit skips:
    // a block whose transaction does not fit the remaining gas is the stock
    // import's to refuse.
    InclusionGas::of(&env.version, env.block.gas_limit.saturating_to::<u64>())
        .check(run.executed.iter().map(|e| {
            (e.index, alloy_consensus::Transaction::gas_limit(&transactions[e.index]), e.result.tx_gas_used())
        }))
        .map_err(|i| refused(Refused::InclusionGas(i)))?;
    let finish_at = std::time::Instant::now();
    let ParallelRun { executed, merged, touched, phases, .. } = run;
    let accounts = merged.len();
    let withdrawals: Vec<Address> =
        ctx.withdrawals.as_deref().map_or_else(Vec::new, |w| w.iter().map(|w| w.address).collect());
    let (scope, cached) = if tail.is_empty() {
        (CacheScope::Only(&withdrawals), withdrawals.iter().filter(|a| merged.get(a).is_some()).count())
    } else {
        (CacheScope::All, accounts)
    };
    let evm = config.evm_with_env(main_db, env);
    let mut executor = config.block_executor_factory().create_executor(evm, ctx);
    executor.apply_pre_execution_changes().map_err(|err| refused(commit_err(&err)))?;
    let txs = executed.len();
    let results = executed.into_iter().map(|e| {
        let tx = &transactions[e.index];
        (*tx.tx_hash(), alloy_consensus::TransactionEnvelope::tx_type(tx), e.result)
    });
    commit_run(&mut executor, results, merged, touched, scope).map_err(refused)?;
    for i in tail {
        executor
            .execute_transaction(Recovered::new_unchecked(&transactions[i], senders[i]))
            .map_err(|err| refused(commit_err(&err)))?;
    }
    let output = executor.finish().map_err(|err| match &err {
        reth_evm::BlockExecutionError::Internal(internal) => match internal.downcast_other::<FoldRefused>() {
            Some(fold) => refused(Refused::Fold(*fold)),
            None => refused(commit_err(&err)),
        },
        _ => refused(commit_err(&err)),
    })?;
    IMPORTS_PARALLEL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let stats = ImportStats {
        txs,
        tail: tail_len,
        accounts,
        cached,
        phases,
        finish_us: finish_at.elapsed().as_micros() as u64,
    };
    Ok((output, stats))
}

/// `N42_BATCH_FAST_TRANSFER` (default on; `0` runs the batches' EVM
/// transactions through the stock handlers).
pub fn batch_fast_transfer() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("N42_BATCH_FAST_TRANSFER").map_or(true, |v| v != "0"))
}

/// The configuration the batches' EVMs are made from: `config`'s chain with
/// the transfer fast path ([`crate::fast_transfer`]) as
/// [`batch_fast_transfer`] says, whatever `config` itself runs with.
pub fn batch_config(config: &N42EvmConfig) -> N42EvmConfig {
    N42EvmConfig::with_evm_factory(
        config.chain_spec().clone(),
        crate::N42EvmFactory::with_fast_transfers(batch_fast_transfer()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        parallel_tests::{config, config_on, CHAIN_ID},
        restored_slots_env_key, restored_slots_execution_key, restored_slots_for, restored_slots_key,
    };
    use alloy_consensus::{BlockBody, Header, TxEip1559, TxEip2930, TxLegacy};
    use alloy_eips::{eip2935, eip4788, eip4895::Withdrawal, eip7002, eip7251};
    use alloy_primitives::{keccak256, Bytes, Signature, TxKind, U256};
    use evm2::{
        bytecode::Bytecode,
        evm::{CacheDB, EmptyDB},
    };
    use reth_ethereum_primitives::{Transaction, TransactionSigned};
    use reth_evm::execute::{BasicBlockExecutor, Executor as _};

    type Output = BlockExecutionOutput<Receipt>;
    type Tx = Recovered<N42TxEnvelope>;

    /// Clear of the precompiles (0x01-0x11): a value transfer to one of
    /// those runs out of gas at 21,000 and moves nothing.
    fn addr(i: u64) -> Address {
        Address::left_padding_from(&(0x1_0000 + i).to_be_bytes())
    }

    const BENEFICIARY: u64 = 0xbe;
    /// A contract that stores its calldata's first word at slot 0.
    const STORE: u64 = 0x5701;
    const FRESH: u64 = 0xf00d;
    const FUND: u64 = 1_000_000_000_000_000_000;
    /// Enough for two transfers of 1,000 at a fee cap of 100 and not a
    /// third (each settles at 10 a gas).
    const TWO_TRANSFERS: u64 = 2_312_000;
    const POOR: u64 = 30;
    const DELEGATED: u64 = 31;

    fn code(db: &mut CacheDB<EmptyDB>, address: Address, bytes: Bytes, balance: u64) {
        let hash = keccak256(&bytes);
        db.insert_account_info(&address, AccountInfo::new(U256::from(balance), 1, hash, Some(Bytecode::new_raw(bytes))));
    }

    fn parent() -> CacheDB<EmptyDB> {
        let mut db = CacheDB::new(EmptyDB::default());
        for i in 1..=40 {
            db.insert_account_info(&addr(i), AccountInfo::empty().with_balance(U256::from(FUND)));
        }
        db.insert_account_info(&addr(BENEFICIARY), AccountInfo::empty().with_balance(U256::from(1u64)));
        db.insert_account_info(&addr(POOR), AccountInfo::empty().with_balance(U256::from(TWO_TRANSFERS)));
        // PUSH1 0 CALLDATALOAD PUSH1 0 SSTORE STOP
        code(&mut db, addr(STORE), Bytes::from_static(&[0x60, 0x00, 0x35, 0x60, 0x00, 0x55, 0x00]), 0);
        let mut delegation = vec![0xef, 0x01, 0x00];
        delegation.extend_from_slice(addr(2).as_slice());
        code(&mut db, addr(DELEGATED), delegation.into(), FUND);
        for (address, bytes) in [
            (eip4788::BEACON_ROOTS_ADDRESS, eip4788::BEACON_ROOTS_CODE.clone()),
            (eip2935::HISTORY_STORAGE_ADDRESS, eip2935::HISTORY_STORAGE_CODE.clone()),
            (eip7002::WITHDRAWAL_REQUEST_PREDEPLOY_ADDRESS, eip7002::WITHDRAWAL_REQUEST_PREDEPLOY_CODE.clone()),
            (eip7251::CONSOLIDATION_REQUEST_PREDEPLOY_ADDRESS, eip7251::CONSOLIDATION_REQUEST_PREDEPLOY_CODE.clone()),
        ] {
            code(&mut db, address, bytes, 1);
        }
        db
    }

    /// The senders are given with the block, so no signature is recovered.
    fn signed(tx: Transaction, from: Address) -> Tx {
        Recovered::new_unchecked(TransactionSigned::new_unhashed(tx, Signature::test_signature()).into(), from)
    }

    #[expect(clippy::too_many_arguments)]
    fn fee_call(from: u64, nonce: u64, to: TxKind, value: u64, gas_limit: u64, input: Bytes, max_fee: u128, priority: u128) -> Tx {
        signed(
            Transaction::Eip1559(TxEip1559 {
                chain_id: CHAIN_ID,
                nonce,
                gas_limit,
                max_fee_per_gas: max_fee,
                max_priority_fee_per_gas: priority,
                to,
                value: U256::from(value),
                input,
                ..Default::default()
            }),
            addr(from),
        )
    }

    fn tx(from: u64, nonce: u64, to: Address, value: u64, gas_limit: u64, input: Bytes) -> Tx {
        fee_call(from, nonce, TxKind::Call(to), value, gas_limit, input, 100, 3)
    }

    fn transfer(from: u64, nonce: u64, to: Address, value: u64) -> Tx {
        tx(from, nonce, to, value, 21_000, Bytes::new())
    }

    fn fee_tx(from: u64, nonce: u64, to: Address, value: u64, max_fee: u128, priority: u128) -> Tx {
        fee_call(from, nonce, TxKind::Call(to), value, 21_000, Bytes::new(), max_fee, priority)
    }

    fn legacy_tx(from: u64, nonce: u64, to: Address, value: u64, chain_id: Option<u64>) -> Tx {
        signed(
            Transaction::Legacy(TxLegacy {
                chain_id,
                nonce,
                gas_price: 50,
                gas_limit: 21_000,
                to: TxKind::Call(to),
                value: U256::from(value),
                input: Bytes::new(),
            }),
            addr(from),
        )
    }

    fn eip2930_tx(from: u64, to: Address, value: u64, access_list: alloy_eips::eip2930::AccessList) -> Tx {
        signed(
            Transaction::Eip2930(TxEip2930 {
                chain_id: CHAIN_ID,
                nonce: 0,
                gas_price: 50,
                gas_limit: 40_000,
                to: TxKind::Call(to),
                value: U256::from(value),
                access_list,
                input: Bytes::new(),
            }),
            addr(from),
        )
    }

    fn withdrawals() -> Vec<Withdrawal> {
        vec![
            Withdrawal { index: 0, validator_index: 0, address: addr(7), amount: 1 },
            Withdrawal { index: 1, validator_index: 0, address: addr(FRESH), amount: 2 },
        ]
    }

    /// `txs` as a block of `gas_limit` with [`withdrawals`].
    fn block_with(txs: &[Tx], gas_limit: u64) -> RecoveredBlock<Block> {
        let header = Header {
            parent_hash: B256::repeat_byte(0x77),
            number: 5,
            timestamp: 1_700_000_000,
            gas_limit,
            base_fee_per_gas: Some(7),
            beneficiary: addr(BENEFICIARY),
            parent_beacon_block_root: Some(B256::repeat_byte(0x33)),
            blob_gas_used: Some(0),
            excess_blob_gas: Some(0),
            ..Default::default()
        };
        let body = BlockBody {
            transactions: txs.iter().map(|t| t.inner().clone()).collect(),
            ommers: Vec::new(),
            withdrawals: Some(withdrawals().into()),
        };
        RecoveredBlock::new_unhashed(Block { header, body }, txs.iter().map(|t| t.signer()).collect())
    }

    fn block_of(txs: &[Tx]) -> RecoveredBlock<Block> {
        block_with(txs, 30_000_000)
    }

    /// The batches' part of the block: plain transfers, two senders paying
    /// one recipient, one creating its recipient, one sender with two (every
    /// one pays the beneficiary its fee; a transfer to the beneficiary is
    /// not the batches', `partition_by_sender`).
    fn batch_part() -> Vec<Tx> {
        vec![
            transfer(1, 0, addr(2), 100),
            transfer(3, 0, addr(FRESH), 100_000_000_000_000_000),
            transfer(5, 0, addr(2), 300),
            transfer(6, 0, addr(7), 400),
            transfer(8, 0, addr(9), 1),
            transfer(11, 0, addr(12), 6),
            transfer(11, 1, addr(13), 7),
        ]
    }

    /// The serial part after them: spending what the batches credited, a
    /// batch sender again, the created account spending, a contract call
    /// that writes storage.
    fn serial_part() -> Vec<Tx> {
        vec![
            transfer(2, 0, addr(9), 50),
            transfer(1, 1, addr(10), 60),
            transfer(FRESH, 0, addr(11), 10),
            transfer(10, 0, addr(BENEFICIARY), 5),
            tx(4, 0, addr(STORE), 0, 100_000, Bytes::from(B256::with_last_byte(0x2a).to_vec())),
        ]
    }

    fn keys(txs: &[Tx]) -> Vec<(Address, Address)> {
        use alloy_consensus::Transaction as _;
        txs.iter().map(|t| (t.signer(), t.to().unwrap_or_default())).collect()
    }

    fn sort_reverts(out: &mut Output) {
        for reverts in out.state.reverts.iter_mut() {
            reverts.sort_unstable_by_key(|(address, _)| *address);
        }
    }

    /// Receipts, gas, requests and the whole bundle: accounts with their
    /// originals and statuses, storage, code, reverts and sizes.
    fn assert_same(ours: &Output, theirs: &Output, what: &str) {
        let (mut ours, mut theirs) = (ours.clone(), theirs.clone());
        sort_reverts(&mut ours);
        sort_reverts(&mut theirs);
        assert_eq!(ours.result, theirs.result, "{what}: receipts, gas, requests and blob gas");
        assert_eq!(ours.state, theirs.state, "{what}: the bundle, code and revert records");
    }

    fn restored_key(c: &N42EvmConfig, block: &RecoveredBlock<Block>) -> B256 {
        use alloy_consensus::transaction::TxHashRef as _;
        let env = c.evm_env(block.header()).expect("env");
        let context = restored_slots_execution_key(
            block.header().parent_hash,
            restored_slots_env_key(&env.block),
            block.header().parent_beacon_block_root,
            block.body().withdrawals.as_ref().map(|w| w.as_slice()),
            [],
        );
        restored_slots_key(context, block.body().transactions.iter().map(|tx| *tx.tx_hash()))
    }

    /// The stock import: the block through the block executor over `db`,
    /// transaction by transaction.
    fn stock_on(c: &N42EvmConfig, db: CacheDB<EmptyDB>, block: &RecoveredBlock<Block>) -> Output {
        BasicBlockExecutor::new(c.clone(), db).execute(block).expect("the stock import")
    }

    fn stock(block: &RecoveredBlock<Block>) -> Output {
        stock_on(&config(), parent(), block)
    }

    fn run_on(
        c: &N42EvmConfig,
        db: &CacheDB<EmptyDB>,
        txs: &[Tx],
        direct: bool,
    ) -> Result<ParallelRun<N42EvmConfig>, Refused> {
        let env = c.evm_env(block_of(txs).header()).expect("env");
        let convert = |i: usize| Recovered::new_unchecked(n42_tx_types::execution::evm_tx_of(txs[i].inner()), txs[i].signer());
        let direct = direct.then(|| {
            DirectTransfers::of_env(&env, crate::direct_transfer::shape_of).expect("the fork has the direct path")
        });
        execute_batches(c, &env, &keys(txs), &convert, &|| Some(db.clone()), direct.as_ref())
    }

    fn run_batches(txs: &[Tx]) -> Result<ParallelRun<N42EvmConfig>, Refused> {
        run_on(&config(), &parent(), txs, false)
    }

    /// `run` (of the block's first transactions) handed to the block's
    /// executor over `db`, then the rest of the block through it, finished
    /// with the fold.
    fn finish_on(
        c: &N42EvmConfig,
        db: CacheDB<EmptyDB>,
        block: &RecoveredBlock<Block>,
        run: ParallelRun<N42EvmConfig>,
        scope: CacheScope<'_>,
    ) -> Result<Output, Refused> {
        use alloy_consensus::transaction::TxHashRef as _;
        let transactions = &block.body().transactions;
        let env = c.evm_env(block.header()).expect("env");
        let ctx = c.context_for_block(block.sealed_block()).expect("ctx");
        let mut ex = c.block_executor_factory().create_executor(c.evm_with_env(db, env), ctx);
        ex.apply_pre_execution_changes().expect("pre");
        let batch = run.executed.len() + run.skipped.len();
        let results = run.executed.into_iter().map(|e| {
            let tx = &transactions[e.index];
            (*tx.tx_hash(), alloy_consensus::TransactionEnvelope::tx_type(tx), e.result)
        });
        commit_run(&mut ex, results, run.merged, run.touched, scope)?;
        for (tx, sender) in transactions.iter().zip(block.senders()).skip(batch) {
            ex.execute_transaction(Recovered::new_unchecked(tx, *sender)).expect("the transaction executes");
        }
        Ok(ex.finish().expect("the block finishes with the fold"))
    }

    /// The batches' part through [`execute_batches`] and [`commit_run`],
    /// then `serial` through the block's executor.
    fn parallel(batch: &[Tx], serial: &[Tx]) -> (Output, usize) {
        let run = run_batches(batch).expect("the batches run");
        assert!(run.skipped.is_empty(), "skipped {:?}", run.skipped);
        let batches = run.phases.batches;
        let all: Vec<_> = batch.iter().chain(serial).cloned().collect();
        (finish_on(&config(), parent(), &block_of(&all), run, CacheScope::All).expect("the commit"), batches)
    }

    /// A mixed block -- batched transfers (shared recipients, a created
    /// recipient), then a serial tail that spends what the batches credited
    /// and calls a contract that writes storage, then the withdrawals and
    /// the system contracts -- gives the receipts, gas and bundle (reverts
    /// included) the sequential execution of the same transactions gives,
    /// and files the block's restored slots under the same key.
    #[test]
    fn the_parallel_build_equals_the_sequential_execution() {
        let (batch, serial) = (batch_part(), serial_part());
        let all: Vec<_> = batch.iter().chain(&serial).cloned().collect();
        let block = block_of(&all);
        let (ours, batches) = parallel(&batch, &serial);
        assert!(restored_slots_for(restored_key(&config(), &block)).is_some(), "the batches' hashes are recorded");
        let reference = stock(&block);
        assert!(batches >= 1, "{batches} batches");
        assert_same(&ours, &reference, "mixed");
        assert_eq!(ours.result.receipts.len(), 12);
        // The contract call's storage is in the bundle, the created
        // account's original is the parent's absence, and the system
        // contracts ran around the batches.
        assert!(!ours.state.state[&addr(STORE)].storage.is_empty());
        assert_eq!(ours.state.state[&addr(FRESH)].original_info, None);
        assert!(!ours.state.state[&eip4788::BEACON_ROOTS_ADDRESS].storage.is_empty());
        assert!(!ours.state.state[&eip2935::HISTORY_STORAGE_ADDRESS].storage.is_empty());
    }

    /// The batches alone, nothing after them but the block's end.
    #[test]
    fn a_block_of_only_batched_transfers_equals_the_sequential_execution() {
        let batch = batch_part();
        let (ours, _) = parallel(&batch, &[]);
        assert_same(&ours, &stock(&block_of(&batch)), "transfers only");
    }

    /// A contract deployed serially at an address the batches funded: the
    /// account's code, nonce and storage come from the serial part, its
    /// original from the parent, as the sequential execution has them.
    #[test]
    fn a_serial_create_at_a_batch_funded_address_equals_the_sequential_execution() {
        let created = addr(4).create(0);
        // PUSH1 0x2a PUSH1 0 SSTORE STOP: storage and empty code.
        let init = Bytes::from_static(&[0x60, 0x2a, 0x60, 0x00, 0x55, 0x00]);
        let batch = vec![transfer(1, 0, created, 500), transfer(3, 0, addr(2), 9)];
        let serial = vec![
            fee_call(4, 0, TxKind::Create, 3, 200_000, init, 100, 3),
            transfer(1, 1, created, 11),
        ];
        let all: Vec<_> = batch.iter().chain(&serial).cloned().collect();
        let reference = stock(&block_of(&all));
        let (ours, _) = parallel(&batch, &serial);
        assert_same(&ours, &reference, "create at a funded address");
        let account = &ours.state.state[&created];
        assert_eq!(account.original_info, None);
        assert!(!account.storage.is_empty());
        assert_eq!(account.info.as_ref().map(|i| i.balance), Some(U256::from(514u64)));
    }

    /// A transfer the EVM refuses (a nonce ahead of the sender's) is
    /// skipped with its sender's later ones; the rest execute.
    #[test]
    fn a_refused_transfer_is_skipped_with_its_senders_later_ones() {
        let txs = vec![transfer(1, 0, addr(2), 1), transfer(3, 5, addr(4), 1), transfer(3, 6, addr(4), 1), transfer(6, 0, addr(7), 1)];
        let run = run_batches(&txs).expect("the batches run");
        assert_eq!(run.skipped, vec![1, 2]);
        assert_eq!(run.executed.iter().map(|e| e.index).collect::<Vec<_>>(), vec![0, 3]);
    }

    /// A recipient that is also a sender is run with its payers, in
    /// candidate order: a credit that comes later does not fund an earlier
    /// spend, and the refused spend is skipped alone.
    #[test]
    fn a_future_credit_cannot_fund_an_earlier_spend() {
        let mut db = parent();
        db.insert_account_info(&addr(2), AccountInfo::empty().with_balance(U256::from(21_000u64 * 100)));
        let txs = vec![transfer(1, 0, addr(2), 1), transfer(2, 0, addr(3), 100), transfer(1, 1, addr(2), 99)];
        for direct in [false, true] {
            let run = run_on(&config(), &db, &txs, direct).expect("the batches run");
            assert_eq!(run.phases.groups, 1);
            assert_eq!(run.executed.iter().map(|e| e.index).collect::<Vec<_>>(), vec![0, 2], "direct {direct}");
            assert_eq!(run.skipped, vec![1]);
        }
    }

    /// The candidates' keys are the partition: a conversion that yields
    /// another sender or recipient ends the run.
    #[test]
    fn changed_candidate_identity_aborts_instead_of_using_another_partition() {
        let c = config();
        let txs = vec![transfer(1, 0, addr(2), 1)];
        let env = c.evm_env(block_of(&txs).header()).expect("env");
        let other = transfer(3, 0, addr(2), 1);
        let convert = |_: usize| Recovered::new_unchecked(n42_tx_types::execution::evm_tx_of(other.inner()), other.signer());
        let result = execute_batches(&c, &env, &keys(&txs), &convert, &|| Some(parent()), None);
        assert!(matches!(result, Err(Refused::NotParallel(NotParallel::Failed(0, _)))));
    }

    /// Two senders' long runs to one recipient, in several worker batches,
    /// with a withdrawal to a batch sender, the shared recipient and a
    /// created one.
    #[test]
    fn multiple_worker_batches_match_serial_on_shared_recipients() {
        let mut txs = Vec::new();
        for nonce in 0..2500 {
            for from in [1, 6] {
                txs.push(transfer(from, nonce, addr(7), 1 + nonce));
            }
        }
        txs.push(transfer(3, 0, addr(FRESH), 9));
        let block = block_with(&txs, 200_000_000);
        for direct in [false, true] {
            let run = run_on(&config(), &parent(), &txs, direct).expect("the batches run");
            assert!(run.skipped.is_empty());
            assert!(run.phases.batches >= 2);
            let ours = finish_on(&config(), parent(), &block, run, CacheScope::All).expect("the commit");
            assert_same(&ours, &stock(&block), "shared recipients");
        }
    }

    /// `txs` through the batches on evm2 alone and with the direct path, on
    /// Cancun and on Prague: the same skipped set, the same results (status,
    /// gas, refund, floor, output, logs), the same touched accounts and,
    /// after the commit and the block's end, the same receipts, gas and
    /// bundle -- the stock import's, when nothing was skipped. `hits` is how
    /// many the direct path applied.
    fn direct_equals_evm2(name: &str, txs: &[Tx], hits: u64) {
        for prague in [false, true] {
            let c = config_on(prague);
            let name = format!("{name} (prague {prague})");
            let (evm, ours) = match (run_on(&c, &parent(), txs, false), run_on(&c, &parent(), txs, true)) {
                (Err(evm), Err(ours)) => {
                    assert_eq!(ours.to_string(), evm.to_string(), "{name}: the refusal");
                    assert_eq!(hits, 0, "{name}: refused, so nothing applied");
                    continue;
                }
                (Ok(evm), Ok(ours)) => (evm, ours),
                (evm, ours) => panic!("{name}: {:?} against {:?}", evm.map(|_| ()), ours.map(|_| ())),
            };
            assert_eq!(ours.phases.direct_hits, hits, "{name}: applied directly");
            assert_eq!(evm.phases.direct_hits, 0);
            assert_eq!(ours.skipped, evm.skipped, "{name}: skipped");
            let results = |run: &ParallelRun<N42EvmConfig>| {
                run.executed.iter().map(|e| (e.index, format!("{:?}", e.result))).collect::<Vec<_>>()
            };
            assert_eq!(results(&ours), results(&evm), "{name}: results");
            let touched = |run: &ParallelRun<N42EvmConfig>| {
                let mut touched: Vec<_> = run.touched.iter().map(|(a, i)| (*a, i.balance, i.nonce)).collect();
                touched.sort_unstable();
                touched.dedup();
                touched
            };
            assert_eq!(touched(&ours), touched(&evm), "{name}: touched");
            let whole = ours.skipped.is_empty();
            // The executed transfers alone are a block of their own.
            let kept: Vec<Tx> = ours.executed.iter().map(|e| txs[e.index].clone()).collect();
            let block = block_of(&kept);
            let finish = |run: ParallelRun<N42EvmConfig>| {
                let run = ParallelRun {
                    executed: run.executed.into_iter().enumerate().map(|(index, e)| Executed { index, ..e }).collect(),
                    skipped: Vec::new(),
                    ..run
                };
                finish_on(&c, parent(), &block, run, CacheScope::All).expect("the commit")
            };
            let ours = finish(ours);
            assert_same(&ours, &finish(evm), &name);
            if whole {
                assert_same(&ours, &stock_on(&c, parent(), &block), &name);
            }
        }
    }

    #[test]
    fn the_direct_path_equals_evm2_on_qualifying_transfers() {
        direct_equals_evm2("1559 to an existing account", &[transfer(1, 0, addr(2), 100)], 1);
        direct_equals_evm2("1559 creating its recipient", &[transfer(1, 0, addr(FRESH), 100)], 1);
        direct_equals_evm2("1559 over the intrinsic gas", &[tx(1, 0, addr(2), 100, 30_000, Bytes::new())], 1);
        direct_equals_evm2("legacy", &[legacy_tx(2, 0, addr(3), 7, Some(CHAIN_ID))], 1);
        direct_equals_evm2("2930 with an empty access list", &[eip2930_tx(3, addr(4), 9, Default::default())], 1);
        direct_equals_evm2("zero value to an existing account", &[transfer(4, 0, addr(5), 0)], 1);
        direct_equals_evm2("a zero tip to an existing beneficiary", &[fee_tx(4, 0, addr(5), 3, 7, 0)], 1);
    }

    #[test]
    fn the_direct_path_hands_everything_else_to_evm2() {
        let access_list = alloy_eips::eip2930::AccessList(vec![alloy_eips::eip2930::AccessListItem {
            address: addr(6),
            storage_keys: vec![B256::ZERO],
        }]);
        direct_equals_evm2("zero value to an empty account", &[transfer(1, 0, addr(FRESH), 0)], 0);
        direct_equals_evm2("to a contract", &[tx(1, 0, addr(STORE), 1, 100_000, Bytes::new())], 0);
        direct_equals_evm2("to a precompile", &[tx(1, 0, Address::with_last_byte(4), 1, 30_000, Bytes::new())], 0);
        direct_equals_evm2("to itself", &[transfer(2, 0, addr(2), 5)], 0);
        direct_equals_evm2("to the beneficiary", &[transfer(3, 0, addr(BENEFICIARY), 5)], 0);
        direct_equals_evm2("with input", &[tx(4, 0, addr(5), 1, 30_000, Bytes::from_static(&[1, 2]))], 0);
        direct_equals_evm2("2930 with an access list", &[eip2930_tx(5, addr(6), 1, access_list)], 0);
        direct_equals_evm2("value the sender lacks", &[transfer(POOR, 0, addr(2), TWO_TRANSFERS)], 0);
        direct_equals_evm2("a nonce ahead", &[transfer(6, 3, addr(2), 1)], 0);
        direct_equals_evm2("an intrinsic gas too low", &[tx(7, 0, addr(2), 1, 20_000, Bytes::new())], 0);
        direct_equals_evm2("a legacy transaction without a chain id", &[legacy_tx(8, 0, addr(3), 7, None)], 0);
        direct_equals_evm2("a fee cap under the base fee", &[fee_tx(9, 0, addr(3), 7, 6, 0)], 0);
        direct_equals_evm2("a priority fee over the fee cap", &[fee_tx(9, 0, addr(3), 7, 10, 11)], 0);
        direct_equals_evm2("a sender with a delegation", &[transfer(DELEGATED, 0, addr(3), 7)], 0);
    }

    #[test]
    fn the_direct_path_equals_evm2_on_sender_runs() {
        let run: Vec<_> = (0..6).map(|n| transfer(1, n, addr(10 + n), 100 + n)).collect();
        direct_equals_evm2("a run of six", &run, 6);
        let gap = vec![transfer(1, 0, addr(2), 1), transfer(1, 1, addr(3), 1), transfer(1, 3, addr(4), 1), transfer(1, 4, addr(5), 1)];
        direct_equals_evm2("a nonce gap", &gap, 2);
        let poor: Vec<_> = (0..4).map(|n| transfer(POOR, n, addr(2 + n), 1_000)).collect();
        direct_equals_evm2("the balance running out mid-run", &poor, 2);
        // 2 is paid by 1, then spends; 3 is paid by 2 and spends after: one
        // group on this chain (`partition_by_sender`).
        let chained = vec![transfer(1, 0, addr(2), 5_000), transfer(2, 0, addr(3), 4_000), transfer(3, 0, addr(4), 3_000), transfer(1, 1, addr(3), 1)];
        direct_equals_evm2("a recipient that sends later", &chained, 4);
        direct_equals_evm2("zero value to an empty account mid-run", &[transfer(1, 0, addr(2), 1), transfer(1, 1, addr(FRESH), 0)], 1);
        direct_equals_evm2("to a precompile after a transfer", &[transfer(1, 0, addr(2), 1), transfer(1, 1, Address::with_last_byte(4), 1)], 1);
        // The created account spends in its payer's group, after the credit.
        direct_equals_evm2("a creating transfer then the created spending", &[transfer(1, 0, addr(FRESH), 1_000_000_000_000_000), transfer(FRESH, 0, addr(5), 1)], 2);
    }

    fn import_on(c: &N42EvmConfig, db: &CacheDB<EmptyDB>, block: &RecoveredBlock<Block>) -> Result<(Output, ImportStats), Refused> {
        import_block(c, block, db.clone(), &|| Some(db.clone()))
    }

    fn import(block: &RecoveredBlock<Block>) -> Result<(Output, ImportStats), Refused> {
        import_on(&config(), &parent(), block)
    }

    /// A mixed block -- batched transfers (shared recipients, a created
    /// recipient), then a serial tail that calls a contract that writes
    /// storage, spends what the batches credited, sends from a batch sender
    /// again and pays the beneficiary, then the withdrawals (one to a batch
    /// recipient) -- imported in parallel gives the stock import's receipts
    /// and bundle.
    #[test]
    fn the_parallel_import_of_a_mixed_block_equals_the_stock_import() {
        // The tail opens with the contract call: the transfers after it are
        // the tail's, not the batches'.
        let mut tail = serial_part();
        tail.rotate_right(1);
        let txs: Vec<_> = batch_part().into_iter().chain(tail).collect();
        let block = block_of(&txs);
        let before = imports_parallel();
        let (ours, stats) = import(&block).expect("the parallel import");
        assert!(restored_slots_for(restored_key(&config(), &block)).is_some());
        // Process-wide, and the tests run beside each other.
        assert!(imports_parallel() > before);
        assert_eq!((stats.txs, stats.tail), (7, 5), "{stats:?}");
        assert_eq!(stats.cached, stats.accounts, "a tail follows: every batch account cached");
        assert_same(&ours, &stock(&block), "mixed import");
        assert!(!ours.state.state[&addr(STORE)].storage.is_empty());
        assert_eq!(ours.state.state[&addr(FRESH)].original_info, None);
    }

    /// Leading transfers that spend in-block credits run in their payer's
    /// group, in block order, an account the block creates included.
    #[test]
    fn leading_transfers_that_spend_in_block_credits_import() {
        let block = block_of(&[transfer(1, 0, addr(2), 100), transfer(2, 0, addr(9), 50), transfer(9, 0, addr(1), 7)]);
        let (ours, stats) = import(&block).expect("the parallel import");
        assert_eq!((stats.txs, stats.tail), (3, 0), "{stats:?}");
        assert_same(&ours, &stock(&block), "in-block credits");
        let created = block_of(&[transfer(3, 0, addr(FRESH), 100_000_000_000_000_000), transfer(FRESH, 0, addr(11), 10)]);
        let (ours, _) = import(&created).expect("the parallel import");
        assert_same(&ours, &stock(&created), "the created account spends");
    }

    /// A block of transfers only caches just the withdrawal recipients
    /// among the batch accounts and still equals the stock import.
    #[test]
    fn a_block_of_transfers_imports_with_only_the_withdrawal_recipients_cached() {
        let block = block_of(&batch_part());
        let (ours, stats) = import(&block).expect("the parallel import");
        assert_eq!((stats.txs, stats.tail), (7, 0), "{stats:?}");
        assert_eq!(stats.cached, 2, "addr(7) and the created recipient, both withdrawal recipients");
        assert!(stats.accounts > stats.cached);
        assert_same(&ours, &stock(&block), "transfers-only import");
    }

    /// What the parallel import leaves to the serial one: a block that
    /// opens with a contract call, a transfer the batches' EVM refuses or
    /// that runs code, a view of the parent that cannot be opened, and a
    /// transaction over the block's remaining gas.
    #[test]
    fn a_refused_import_leaves_the_block_to_the_serial_import() {
        let refused = imports_refused();
        let call_first: Vec<_> = serial_part().into_iter().rev().collect();
        assert!(matches!(import(&block_of(&call_first)), Err(Refused::NotParallel(NotParallel::NotATransfer(0)))));
        assert!(imports_refused() > refused);
        // A transfer to a contract runs code: skipped on the batches, so the
        // block goes serial.
        let to_contract = vec![transfer(1, 0, addr(2), 1), tx(4, 0, addr(STORE), 0, 100_000, Bytes::new())];
        assert!(matches!(import(&block_of(&to_contract)), Err(Refused::NotParallel(NotParallel::Failed(1, _)))));
        let block = block_of(&batch_part());
        let db = parent();
        let none = import_block(&config(), &block, db, &|| None::<CacheDB<EmptyDB>>);
        assert!(matches!(none, Err(Refused::NotParallel(NotParallel::NoState))));
        let small = block_with(&batch_part(), 42_000);
        assert!(matches!(import(&small), Err(Refused::InclusionGas(2))), "the third transfer does not fit");
    }

    /// A pre-execution system call that changes a batch account: the
    /// batches ran on the parent's value, so the cache half refuses with
    /// the executor as it was, and the serial execution of the block from
    /// there is the stock one.
    #[test]
    fn a_pre_execution_change_to_a_batch_account_declines_with_the_executor_unchanged() {
        let c = config();
        let mut db = parent();
        // The beacon roots contract pays its balance to a batch recipient.
        let mut pre = vec![0x73];
        pre.extend_from_slice(addr(2).as_slice());
        pre.push(0xff); // SELFDESTRUCT(addr(2)), the code kept under EIP-6780.
        code(&mut db, eip4788::BEACON_ROOTS_ADDRESS, pre.into(), 1_000_000_000);
        let txs = batch_part();
        let block = block_of(&txs);
        let run = run_on(&c, &db, &txs, true).expect("the batches run");
        let err = finish_on(&c, db.clone(), &block, run, CacheScope::All).expect_err("the cache half refuses");
        assert!(matches!(err, Refused::Cache(address) if address == addr(2)), "{err:?}");
        assert!(err.executor_untouched());
        assert!(matches!(import_on(&c, &db, &block), Err(Refused::Cache(_))));

        // Refused, the executor goes on serially from where it was.
        let env = c.evm_env(block.header()).expect("env");
        let ctx = c.context_for_block(block.sealed_block()).expect("ctx");
        let mut ex = c.block_executor_factory().create_executor(c.evm_with_env(db.clone(), env), ctx);
        ex.apply_pre_execution_changes().expect("pre");
        let run = run_on(&c, &db, &txs, true).expect("the batches run");
        let before = ex.evm_mut().overlay_db_mut().cache.accounts.clone();
        assert!(commit_run(&mut ex, std::iter::empty(), run.merged, run.touched, CacheScope::All).is_err());
        assert_eq!(ex.evm_mut().overlay_db_mut().cache.accounts, before, "nothing written");
        assert!(ex.receipts().is_empty());
        for (tx, sender) in block.body().transactions.iter().zip(block.senders()) {
            ex.execute_transaction(Recovered::new_unchecked(tx, *sender)).expect("the transaction executes");
        }
        let serial = ex.finish().expect("the block finishes");
        assert_same(&serial, &stock_on(&c, db, &block), "the serial fallback");
    }

    /// An account a block touches and leaves unchanged is in the bundle at
    /// its parent value on every path (`restored_slots`): every transfer of
    /// the block pays a zero tip, so the funded beneficiary is credited
    /// nothing, and two of them send nothing to a funded account that stays
    /// as it was. The stock import, the batches on evm2, the batches with
    /// the direct path and the follower's import give one bundle.
    #[test]
    fn a_touched_and_unchanged_account_is_the_same_on_every_path() {
        let unchanged = addr(5);
        let beneficiary = addr(BENEFICIARY);
        let txs = vec![
            fee_tx(1, 0, addr(2), 100, 100, 0),
            fee_tx(3, 0, unchanged, 0, 100, 0),
            fee_tx(6, 0, addr(7), 400, 100, 0),
            fee_tx(8, 0, unchanged, 0, 100, 0),
            fee_tx(11, 0, addr(12), 6, 100, 0),
            fee_tx(11, 1, addr(13), 7, 100, 0),
        ];
        let block = block_of(&txs);
        for prague in [false, true] {
            let c = config_on(prague);
            let reference = stock_on(&c, parent(), &block);
            for address in [beneficiary, unchanged] {
                let account = &reference.state.state[&address];
                assert_eq!(account.info, account.original_info, "{address} is unchanged");
            }
            for direct in [false, true] {
                let what = format!("the batches (direct {direct}, prague {prague})");
                let run = run_on(&c, &parent(), &txs, direct).expect("the batches run");
                assert!(run.skipped.is_empty(), "{what}: skipped {:?}", run.skipped);
                assert_eq!(run.phases.direct_hits > 0, direct, "{what}: applied directly");
                assert!(run.merged.get(&unchanged).is_none() && run.merged.get(&beneficiary).is_none());
                // The block's end reads the withdrawal recipients.
                let paid: Vec<Address> = withdrawals().iter().map(|w| w.address).collect();
                for scope in [CacheScope::All, CacheScope::Only(&paid)] {
                    let run = run_on(&c, &parent(), &txs, direct).expect("the batches run");
                    let ours = finish_on(&c, parent(), &block, run, scope).expect("the commit");
                    assert_same(&ours, &reference, &what);
                }
            }
            let (ours, stats) = import_on(&c, &parent(), &block).expect("the parallel import");
            assert_eq!((stats.txs, stats.tail), (txs.len(), 0), "{stats:?}");
            assert_same(&ours, &reference, "import_block");
        }
    }

    /// The cache half refuses, writing nothing, when the executor already
    /// holds a batch account at another value than the batches' original.
    #[test]
    fn the_cache_half_refuses_an_account_the_executor_holds_otherwise() {
        use evm2::evm::Database as _;
        let run = run_batches(&batch_part()).expect("the batches run");
        let mut overlay = parent();
        overlay.insert_account_info(&addr(2), AccountInfo::empty().with_balance(U256::from(5u64)));
        let before = overlay.cache.accounts.clone();
        assert_eq!(install_cache(&mut overlay, &run.merged), Err(addr(2)));
        assert_eq!(install_cache_only(&mut overlay, &run.merged, &[]), Err(addr(2)));
        assert_eq!(overlay.cache.accounts, before, "nothing written");
        // Held at the parent's value it is overwritten.
        let mut overlay = parent();
        assert_eq!(install_cache(&mut overlay, &run.merged), Ok(run.merged.len()));
        assert_eq!(
            overlay.get_account(&addr(2)).expect("read").map(|i| i.balance),
            run.merged.get(&addr(2)).map(|i| i.balance)
        );
    }

    /// The parent's accounts behind an `Arc`: a batch's view costs nothing
    /// to open, as the node's does.
    #[derive(Clone)]
    struct SharedDb(std::sync::Arc<alloy_primitives::map::AddressMap<AccountInfo>>);

    impl evm2::evm::Database for SharedDb {
        type Error = std::convert::Infallible;

        fn get_account(&mut self, address: &Address) -> Result<Option<AccountInfo>, Self::Error> {
            Ok(self.0.get(address).cloned())
        }

        fn get_code_by_hash(&mut self, hash: &B256) -> Result<Bytecode, Self::Error> {
            Ok(self.0.values().find(|i| i.code_hash == *hash).and_then(|i| i.code.clone()).unwrap_or_default())
        }

        fn get_storage(&mut self, _: &Address, _: &U256) -> Result<U256, Self::Error> {
            Ok(U256::ZERO)
        }

        fn get_block_hash(&mut self, _: &U256) -> Result<B256, Self::Error> {
            Ok(B256::ZERO)
        }
    }

    /// The leader's stages for a full block of plain transfers, timed: the
    /// batches with their merge, the commit of the receipts and the
    /// executor's finish with the fold. `N42_BENCH_TXS` transfers (95,238 by
    /// default) between as many senders and recipients, `N42_BENCH_ROUNDS`
    /// rounds (12), on the build pool (`N42_PARALLEL_BUILD_THREADS`).
    ///
    /// `cargo test --release -p n42-execution --lib leader_stages -- --ignored --nocapture`
    #[test]
    #[ignore = "timing run, not a check"]
    fn leader_stages_of_a_full_block_of_transfers() {
        use alloy_consensus::transaction::TxHashRef as _;
        let var = |name: &str, default: usize| std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default);
        let (count, rounds) = (var("N42_BENCH_TXS", 95_238) as u64, var("N42_BENCH_ROUNDS", 12));
        const BASE: u64 = 0x100_0000;
        let mut accounts = std::mem::take(&mut parent().cache.accounts)
            .into_iter()
            .filter_map(|(address, info)| Some((address, info?)))
            .collect::<alloy_primitives::map::AddressMap<_>>();
        for i in 0..2 * count {
            accounts.insert(addr(BASE + i), AccountInfo::empty().with_balance(U256::from(FUND)));
        }
        let db = SharedDb(std::sync::Arc::new(accounts));
        let txs: Vec<Tx> = (0..count).map(|i| transfer(BASE + i, 0, addr(BASE + count + i), 1_000)).collect();
        let block = block_with(&txs, count * 21_000);
        let hashes: Vec<B256> = block.body().transactions.iter().map(|tx| *tx.tx_hash()).collect();
        let types: Vec<N42TxType> =
            block.body().transactions.iter().map(alloy_consensus::TransactionEnvelope::tx_type).collect();
        let c = config();
        let env = c.evm_env(block.header()).expect("env");
        let keys = keys(&txs);
        let convert = |i: usize| Recovered::new_unchecked(n42_tx_types::execution::evm_tx_of(txs[i].inner()), txs[i].signer());
        let direct = DirectTransfers::of_env(&env, crate::direct_transfer::shape_of).expect("the fork has the direct path");
        println!(
            "txs={count} threads={} sizes: account={} revert={} info={}",
            build_pool().current_num_threads(),
            size_of::<(Address, evm2::block::bundle_state::BundleAccount)>(),
            size_of::<(Address, evm2::block::bundle_state::AccountRevert)>(),
            size_of::<AccountInfo>(),
        );
        let ms = |at: std::time::Instant| at.elapsed().as_secs_f64() * 1e3;
        let mut rows: Vec<[f64; 7]> = Vec::new();
        for round in 0..rounds {
            let at = std::time::Instant::now();
            let run = execute_batches(&c, &env, &keys, &convert, &|| Some(db.clone()), Some(&direct)).expect("the batches run");
            let run_ms = ms(at);
            let phases = run.phases;
            assert_eq!(run.executed.len() as u64, count);
            let ctx = c.context_for_block(block.sealed_block()).expect("ctx");
            let mut ex = c.block_executor_factory().create_executor(c.evm_with_env(db.clone(), env.clone()), ctx);
            ex.apply_pre_execution_changes().expect("pre");
            let at = std::time::Instant::now();
            let results = run.executed.into_iter().map(|e| (hashes[e.index], types[e.index], e.result));
            commit_run(&mut ex, results, run.merged, run.touched, CacheScope::Only(&[])).expect("the commit");
            let commit_ms = ms(at);
            let at = std::time::Instant::now();
            let out = ex.finish().expect("the block finishes with the fold");
            let finish_ms = ms(at);
            assert!(out.state.state.len() as u64 >= 2 * count, "every sender and recipient");
            let at = std::time::Instant::now();
            drop(out);
            let drop_ms = ms(at);
            println!(
                "round {round}: run {run_ms:.1} (partition {} batches {} collect {} merge {}) commit {commit_ms:.1} finish {finish_ms:.1} drop {drop_ms:.1}",
                phases.partition_ms, phases.groups_ms, phases.collect_ms, phases.merge_ms
            );
            rows.push([run_ms, phases.partition_ms as f64, phases.groups_ms as f64, phases.collect_ms as f64, commit_ms, finish_ms, drop_ms]);
        }
        let median = |k: usize| {
            let mut v: Vec<f64> = rows.iter().skip(2).map(|r| r[k]).collect();
            v.sort_by(f64::total_cmp);
            v[v.len() / 2]
        };
        println!(
            "LEADER_STAGES median_ms run={:.1} partition={:.0} batches={:.0} collect={:.0} commit={:.1} finish={:.1} drop={:.1}",
            median(0), median(1), median(2), median(3), median(4), median(5), median(6)
        );
    }
}
