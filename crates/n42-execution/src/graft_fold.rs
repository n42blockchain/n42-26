// Copyright (c) 2017-2025 N42 Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! The bundle half of the graft.
//!
//! Lifted from n42-rs `crates/n42/engine-types/src/graft_fold.rs`. What
//! differs here: an account the batches changed whose storage, code or
//! existence the serial part changed afterwards is folded, not refused (see
//! [`sequential`]).
//!
//! reth's block executor keeps its accumulator private until `finish` turns
//! it into a bundle, so the batches' bundles cannot be grafted into a live
//! bundle mid-block. The graft is two halves:
//!
//! 1. **Cache half** (the caller's): before the serial tail and `finish`,
//!    every account the batches changed that anything after them may read
//!    is written into the executor's accepted overlay at its post-batch
//!    value ([`MergedBatches::accounts`]), so the serial transactions, the
//!    withdrawals and the post-execution calls read what a sequential
//!    execution would have left there.
//! 2. **Bundle half** ([`fold_into`]): after `finish`, the batches' accounts
//!    are folded into the finished bundle. An account only the batches
//!    touched goes in as they left it. An account the serial part touched
//!    too keeps the serial part's final info, storage and code, which
//!    already include the batches' effect, and takes the batches' original
//!    (the parent's); its status and its revert are recomputed from that
//!    pair, and the serial part's revert to the post-batch value is dropped.
//!    The batches' reverts go into the bundle's revert set for the block,
//!    sorted by address, so a revert by index still rebuilds the parent's
//!    state.
//!
//! The fold is defensive: before it changes anything it checks every
//! invariant the rule relies on and refuses the block with a [`FoldRefused`]
//! (one warn line, [`refused_count`] up):
//! - a batch account carries no storage, no code and no selfdestruct;
//! - every batch built against the same parent (an account two batches
//!   changed has one original), and at most one of them debited it (a
//!   sender's balance and nonce move in one batch; the others only credit,
//!   so the credits commute);
//! - an account the serial part touched too was read by it at its
//!   post-batch value: its serial original equals the batches' final info.
//!   This is what the cache half guarantees, and what catches a withdrawal
//!   recipient or a beneficiary the cache half missed.

use alloy_primitives::{
    map::{AddressMap, Entry},
    Address, U256,
};
use evm2::evm::AccountInfo;
use evm2::block::bundle_state::{AccountRevert, AccountStatus, BundleAccount, BundleState, TransitionAccount};
use std::sync::atomic::{AtomicU64, Ordering};

/// Why the bundle half refused a block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FoldRefused {
    /// A batch changed an account's storage.
    Storage(Address),
    /// A batch account has code, or a batch deployed code.
    Code(Address),
    /// A batch destroyed an account, or left it absent.
    Destroyed(Address),
    /// Two batches loaded an account with different originals.
    Originals(Address),
    /// Two batches debited the same account.
    TwoDebits(Address),
    /// The serial part read an account the batches changed at a value other
    /// than their result: the cache half did not hold it.
    Stale(Address),
    /// The batches' changes to an account do not add up: a balance over the
    /// range or a nonce that went down.
    Arithmetic(Address),
    /// A batch's bundle does not hold exactly one revert for each of its
    /// accounts.
    Reverts(Address),
}

impl std::fmt::Display for FoldRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Storage(a) => write!(f, "batch account {a} has storage changes"),
            Self::Code(a) => write!(f, "batch account {a} has code"),
            Self::Destroyed(a) => write!(f, "batch account {a} was destroyed"),
            Self::Originals(a) => write!(f, "batches loaded {a} with different originals"),
            Self::TwoDebits(a) => write!(f, "two batches debited {a}"),
            Self::Stale(a) => write!(f, "the serial part read {a} at a value other than the batches' result"),
            Self::Arithmetic(a) => write!(f, "the batches' changes to {a} overflow or decrease its nonce"),
            Self::Reverts(a) => write!(f, "a batch's reverts do not match its accounts at {a}"),
        }
    }
}

impl std::error::Error for FoldRefused {}

static REFUSED: AtomicU64 = AtomicU64::new(0);

/// How many blocks the bundle half has refused in this process.
pub fn refused_count() -> u64 {
    REFUSED.load(Ordering::Relaxed)
}

fn refuse<T>(err: FoldRefused) -> Result<T, FoldRefused> {
    REFUSED.fetch_add(1, Ordering::Relaxed);
    tracing::warn!(target: "n42::graft", reason = %err, "parallel graft refused");
    Err(err)
}

/// The batches' bundles merged and checked: what the cache half writes and
/// the bundle half folds.
///
/// Kept in the block bundle's own shape, so that the fold moves it into the
/// bundle instead of entering its accounts one by one a second time: a full
/// block's 190,000 accounts through a second map and a second revert set
/// were 45 ms of the batches' merge and 50 ms of the executor's finish.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct MergedBatches {
    /// Every account the batches changed, as the one batch that changed it
    /// left it. An account several batches changed (`shared`) holds the
    /// first one's entry with every batch's change added into its info; its
    /// status is made at the fold.
    state: AddressMap<BundleAccount>,
    /// The batches' reverts, each batch's set as it came, until
    /// [`BatchMerger::finish`] makes one set of them sorted by address. A
    /// shared account has one per batch that changed it; the fold keeps one,
    /// made anew.
    reverts: Vec<Vec<(Address, AccountRevert)>>,
    /// Accounts more than one batch changed, with whether one of them
    /// debited it (balance down or nonce up).
    shared: AddressMap<bool>,
}

impl MergedBatches {
    /// The accounts the batches changed, in the order the bundle holds them.
    pub fn addresses(&self) -> impl Iterator<Item = &Address> + '_ {
        self.state.keys()
    }
}

fn debits(original: Option<&AccountInfo>, info: &AccountInfo) -> bool {
    let (balance, nonce) = original.map_or((U256::ZERO, 0), |o| (o.balance, o.nonce));
    info.balance < balance || info.nonce != nonce
}

/// One batch's bundle with everything about it that the merge relies on
/// checked: made on the batch's own thread, so that the merge, which runs on
/// one, only enters the accounts.
#[derive(Debug)]
pub struct CheckedBundle(BundleState);

impl CheckedBundle {
    /// Checks `bundle`: no code deployed; every account present, without
    /// storage changes, code or a selfdestruct; and exactly one revert for
    /// every account, without storage.
    pub fn new(bundle: BundleState) -> Result<Self, FoldRefused> {
        if let Some(hash) = bundle.contracts.keys().next() {
            // A batch never deploys code; name an account if one carries it.
            let holder = bundle
                .state
                .iter()
                .find(|(_, a)| a.info.as_ref().is_some_and(|i| i.code_hash == *hash))
                .map_or(Address::ZERO, |(a, _)| *a);
            return refuse(FoldRefused::Code(holder));
        }
        for (address, account) in &bundle.state {
            if !account.storage.is_empty() {
                return refuse(FoldRefused::Storage(*address));
            }
            if account.status.was_destroyed() {
                return refuse(FoldRefused::Destroyed(*address));
            }
            let Some(info) = account.info.as_ref() else { return refuse(FoldRefused::Destroyed(*address)) };
            if !info.is_empty_code_hash() || account.original_info.as_ref().is_some_and(|o| !o.is_empty_code_hash()) {
                return refuse(FoldRefused::Code(*address));
            }
        }
        // One revert for every account and no other: what a bundle made of
        // one block's transitions holds, and what lets the reverts stay
        // apart from the accounts in the merge.
        let mut reverts = 0usize;
        for (address, revert) in bundle.reverts.iter().flatten() {
            if !bundle.state.contains_key(address) {
                return refuse(FoldRefused::Reverts(*address));
            }
            if !revert.storage.is_empty() || revert.wipe_storage {
                return refuse(FoldRefused::Storage(*address));
            }
            reverts += 1;
        }
        if reverts != bundle.state.len() {
            let mut seen: alloy_primitives::map::AddressHashSet = Default::default();
            let twice = bundle.reverts.iter().flatten().find(|(address, _)| !seen.insert(*address)).map(|(a, _)| *a);
            let address =
                twice.or_else(|| bundle.state.keys().find(|a| !seen.contains(*a)).copied()).unwrap_or(Address::ZERO);
            return refuse(FoldRefused::Reverts(address));
        }
        Ok(Self(bundle))
    }
}

/// Merges batches' bundles one at a time, as the batches finish, so that the
/// merge of a full block runs beside the batches still executing.
#[derive(Debug, Default)]
pub struct BatchMerger {
    merged: MergedBatches,
}

impl BatchMerger {
    /// A merger with room for `accounts` without regrowing.
    pub fn with_capacity(accounts: usize) -> Self {
        Self {
            merged: MergedBatches {
                state: AddressMap::with_capacity_and_hasher(accounts, Default::default()),
                ..Default::default()
            },
        }
    }

    /// A merger over `state`, an empty map made by [`prefaulted_state`].
    pub fn with_state(state: AddressMap<BundleAccount>) -> Self {
        debug_assert!(state.is_empty(), "a merger starts empty");
        Self {
            merged: MergedBatches {
                state,
                ..Default::default()
            },
        }
    }

    /// Adds one batch's bundle, built against the same parent as the others
    /// with [`crate::batch_state::BatchState`], refusing a change to an
    /// account another batch changed too that the fold cannot model;
    /// otherwise the account gets this batch's change added. The merger is
    /// not to be used after a refusal.
    pub fn add(&mut self, bundle: CheckedBundle) -> Result<(), FoldRefused> {
        let CheckedBundle(mut bundle) = bundle;
        let merged = &mut self.merged;
        merged.state.reserve(bundle.state.len());
        for (address, account) in bundle.state {
            match merged.state.entry(address) {
                Entry::Vacant(entry) => {
                    entry.insert(account);
                }
                Entry::Occupied(mut entry) => {
                    let m = entry.get_mut();
                    if m.original_info != account.original_info {
                        return refuse(FoldRefused::Originals(address));
                    }
                    let (Some(info), Some(held)) = (account.info.as_ref(), m.info.as_mut()) else {
                        return refuse(FoldRefused::Destroyed(address));
                    };
                    let debited = debits(account.original_info.as_ref(), info);
                    // Until now the entry was one batch's own, so whether
                    // that batch debited the account shows in it.
                    let shared = merged.shared.entry(address).or_insert_with(|| debits(m.original_info.as_ref(), held));
                    if *shared && debited {
                        return refuse(FoldRefused::TwoDebits(address));
                    }
                    let (balance, nonce) = m.original_info.as_ref().map_or((U256::ZERO, 0), |o| (o.balance, o.nonce));
                    // `info` = original + this batch's change, added onto
                    // what the earlier batches made of it.
                    let added = if info.balance >= balance {
                        held.balance.checked_add(info.balance - balance)
                    } else {
                        held.balance.checked_sub(balance - info.balance)
                    };
                    let bumped = info.nonce.checked_sub(nonce).and_then(|by| held.nonce.checked_add(by));
                    let (Some(added), Some(bumped)) = (added, bumped) else {
                        return refuse(FoldRefused::Arithmetic(address));
                    };
                    held.balance = added;
                    held.nonce = bumped;
                    *shared |= debited;
                }
            }
        }
        merged.reverts.extend(std::mem::take(&mut *bundle.reverts).into_iter().filter(|set| !set.is_empty()));
        Ok(())
    }

    /// The merge of everything added, its reverts in one set sorted by
    /// address (on the worker pool this is called on, where there are enough
    /// of them).
    pub fn finish(self) -> MergedBatches {
        let mut merged = self.merged;
        if merged.reverts.len() > 1 {
            let sets = std::mem::take(&mut merged.reverts);
            merged.reverts.push(crate::parallel_common::concat_sorted(sets));
        } else if let Some(set) = merged.reverts.first_mut() {
            crate::parallel_common::sort_reverts(set);
        }
        merged
    }
}

/// The empty map [`BatchMerger::with_capacity`] makes for `accounts`, with
/// every page of its table already written: it is filled to its capacity
/// with placeholder accounts and cleared, which keeps the table. A merger
/// over it enters a block's accounts without the page faults of a fresh
/// table. The table's iteration order is that of any fresh one: per table,
/// from its random hasher seed; nothing reads the bundle in map order.
pub fn prefaulted_state(accounts: usize) -> AddressMap<BundleAccount> {
    let mut state: AddressMap<BundleAccount> =
        AddressMap::with_capacity_and_hasher(accounts, Default::default());
    let room = state.capacity();
    for n in 0..room as u64 {
        let mut bytes = [0u8; 20];
        bytes[12..].copy_from_slice(&n.to_be_bytes());
        state.insert(
            Address::from(bytes),
            BundleAccount::new(None, None, Default::default(), AccountStatus::LoadedNotExisting),
        );
    }
    debug_assert_eq!(state.capacity(), room, "filled without regrowing");
    state.clear();
    state
}

/// Merges the batches' bundles, each built against the same parent with
/// [`crate::batch_state::BatchState`], refusing anything the fold cannot
/// model. An account two batches changed gets every batch's change added
/// together.
pub fn merge_batches(batches: Vec<BundleState>) -> Result<MergedBatches, FoldRefused> {
    let mut merger = BatchMerger::with_capacity(batches.iter().map(|b| b.state.len()).sum());
    for bundle in batches {
        merger.add(CheckedBundle::new(bundle)?)?;
    }
    Ok(merger.finish())
}

impl MergedBatches {
    /// Accounts the batches changed.
    pub fn len(&self) -> usize {
        self.state.len()
    }

    /// Whether the batches changed nothing.
    pub fn is_empty(&self) -> bool {
        self.state.is_empty()
    }

    /// An account's value after the batches, if they changed it.
    pub fn get(&self, address: &Address) -> Option<&AccountInfo> {
        self.state.get(address).and_then(|m| m.info.as_ref())
    }

    /// An account's parent value as the batches loaded it (`Some(None)`: the
    /// parent did not have it), if they changed it.
    pub fn original(&self, address: &Address) -> Option<Option<&AccountInfo>> {
        self.state.get(address).map(|m| m.original_info.as_ref())
    }

    /// Every account the batches changed at its post-batch value: what the
    /// cache half writes into the executor's overlay.
    pub fn accounts(&self) -> impl Iterator<Item = (&Address, &AccountInfo)> {
        self.state.iter().filter_map(|(address, m)| Some((address, m.info.as_ref()?)))
    }
}

/// The status reth's accumulator gives an account loaded as `original`.
fn loaded_status(original: Option<&AccountInfo>) -> AccountStatus {
    match original {
        None => AccountStatus::LoadedNotExisting,
        Some(info) if info.is_empty() => AccountStatus::LoadedEmptyEIP161,
        Some(_) => AccountStatus::Loaded,
    }
}

/// The bundle account and revert of one transition from `original` (the
/// parent's) to `info`, as reth's accumulator makes them: the status from
/// the pair, never copied from either side.
fn transition(original: Option<AccountInfo>, info: AccountInfo) -> (BundleAccount, Option<AccountRevert>) {
    let previous_status = loaded_status(original.as_ref());
    let had_no_nonce_and_code = original.as_ref().is_none_or(|o| o.nonce == 0 && o.is_empty_code_hash());
    let t = TransitionAccount {
        info: Some(info),
        status: previous_status.on_changed(had_no_nonce_and_code),
        previous_info: original,
        previous_status,
        storage: Default::default(),
        storage_was_destroyed: false,
    };
    (t.present_bundle_account(), t.create_revert())
}

/// The bundle account and revert of an account the batches changed and the
/// serial part changed after them, as the accumulator makes them of the same
/// transactions run in order: the serial part's final info and storage over
/// the parent's `original`.
///
/// The serial part saw the account as loaded and existing (a batch never
/// leaves one empty), where a sequential execution has it already changed by
/// the batches' transfers. Every status but one comes out the same from
/// either start: a creation, a destruction and whatever follows them do not
/// depend on it, and an in-memory change reached by changes alone means the
/// account had no nonce and no code after the batches, so it had none in the
/// parent either. Only `Changed` is the batches' own status carried on. The
/// revert is then the one transition from the parent's account.
fn sequential(original: Option<AccountInfo>, serial: BundleAccount) -> (BundleAccount, Option<AccountRevert>) {
    let previous_status = loaded_status(original.as_ref());
    let status = match serial.status {
        AccountStatus::Changed => {
            previous_status.on_changed(original.as_ref().is_none_or(|o| o.nonce == 0 && o.is_empty_code_hash()))
        }
        status => status,
    };
    let t = TransitionAccount {
        info: serial.info,
        status,
        previous_info: original,
        previous_status,
        storage: serial.storage,
        storage_was_destroyed: false,
    };
    (t.present_bundle_account(), t.create_revert())
}

/// The accounts the fold made anew, in place of the batches' own entries.
#[derive(Default)]
struct Remade {
    /// Their reverts (`None`: the account has none).
    reverts: AddressMap<Option<AccountRevert>>,
    /// Accounts whose block-level transition has no revert: out of the
    /// bundle, and the serial part's revert to the post-batch value with them.
    unchanged: Vec<Address>,
}

impl Remade {
    /// Enters `account` into `state` when its transition has a revert, and
    /// returns what it adds to the bundle's size.
    fn put(
        &mut self,
        state: &mut AddressMap<BundleAccount>,
        address: Address,
        (account, revert): (BundleAccount, Option<AccountRevert>),
    ) -> usize {
        let size = match &revert {
            Some(_) => {
                let size = account.size_hint();
                state.insert(address, account);
                size
            }
            // The accumulator leaves an account whose block-level transition
            // has no revert out of the bundle
            // (`apply_transitions_and_create_reverts`): one the parent did
            // not have and the block destroyed.
            None => {
                self.unchanged.push(address);
                0
            }
        };
        self.reverts.insert(address, revert);
        size
    }
}

/// What [`fold_into`] did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FoldStats {
    /// Accounts folded in.
    pub accounts: usize,
    /// Of those, accounts the serial part had touched too.
    pub overlapped: usize,
}

/// Folds the merged batches into `block`, the bundle the serial part's
/// `finish` returned, after checking every account the two share (see the
/// module docs). Nothing is changed when it refuses.
///
/// The batches' accounts are the block's but for a handful, so the block's
/// own are entered into the batches' map, which becomes the block's, and
/// the batches' reverts, sorted already, become the block's set.
pub fn fold_into(block: &mut BundleState, merged: MergedBatches) -> Result<FoldStats, FoldRefused> {
    // Checks first: the block is left as it was on a refusal.
    for (address, serial) in &block.state {
        if let Some(m) = merged.state.get(address)
            && serial.original_info != m.info
        {
            return refuse(FoldRefused::Stale(*address));
        }
    }
    let MergedBatches { mut state, reverts, shared } = merged;
    let mut reverts = reverts.into_iter().next().unwrap_or_default();
    let mut stats = FoldStats { accounts: state.len(), overlapped: 0 };
    // A batch account has no storage (the merge refuses one), so each
    // counts one.
    let mut state_size = block.state_size + state.len();
    let mut remade = Remade::default();
    // An account several batches changed and the serial part did not: one
    // transition from the parent's value to the sum of their changes.
    for address in shared.keys() {
        if block.state.contains_key(address) {
            continue;
        }
        let Some(batch) = state.remove(address) else { continue };
        state_size -= batch.size_hint();
        let Some(info) = batch.info else { continue };
        state_size += remade.put(&mut state, *address, transition(batch.original_info, info));
    }
    state.reserve(block.state.len());
    for (address, serial) in std::mem::take(&mut block.state) {
        match state.remove(&address) {
            Some(batch) => {
                stats.overlapped += 1;
                state_size -= batch.size_hint() + serial.size_hint();
                state_size += remade.put(&mut state, address, sequential(batch.original_info, serial));
            }
            None => {
                state.insert(address, serial);
            }
        }
    }
    let Remade { reverts: mut remade, mut unchanged } = remade;
    // A batch's revert has no storage ([`CheckedBundle::new`]), nor has one
    // made here of a batch's original and a serial account's unless the
    // serial part gave it some.
    let plain = remade.values().flatten().all(|revert| revert.size_hint() == 1);
    block.state = state;
    block.state_size = state_size;
    if !remade.is_empty() {
        // The first of an account's reverts gives its place to the one made
        // anew; the others go.
        reverts.retain_mut(|(address, revert)| match remade.get_mut(address) {
            None => true,
            Some(slot) => match slot.take() {
                Some(new) => {
                    *revert = new;
                    true
                }
                None => false,
            },
        });
    }
    if !unchanged.is_empty() {
        unchanged.sort_unstable();
        if let Some(last) = block.reverts.last_mut() {
            last.retain(|(address, _)| unchanged.binary_search(address).is_err());
        }
        block.reverts_size = block.reverts.iter().flatten().map(|(_, revert)| revert.size_hint()).sum();
    }
    crate::parallel_common::append_sorted_reverts(block, reverts, plain);
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        batch_state::BatchState,
        parallel_tests::{config, CHAIN_ID},
    };
    use alloy_consensus::{Signed, TxEip1559};
    use alloy_primitives::{Bytes, Signature, TxKind, B256};
    use evm2::{
        block::{
            bundle_state::{AccountInfoRevert, StorageSlot},
            BlockState,
        },
        evm::{CacheDB, EmptyDB},
    };
    use n42_tx_types::N42TxEnvelope;
    use reth_evm::ConfigureEvm as _;
    use reth_primitives_traits::Recovered;

    type Config = crate::N42EvmConfig;

    fn addr(i: u64) -> Address {
        Address::left_padding_from(&i.to_be_bytes())
    }

    const BENEFICIARY: u64 = 0xbe;
    const FUND: u64 = 1_000_000_000_000_000_000;

    fn parent() -> CacheDB<EmptyDB> {
        let mut db = CacheDB::new(EmptyDB::default());
        for i in 1..=40 {
            db.insert_account_info(&addr(i), AccountInfo::empty().with_balance(U256::from(FUND)));
        }
        db.insert_account_info(&addr(BENEFICIARY), AccountInfo::empty().with_balance(U256::from(1u64)));
        db
    }

    fn parent_info(address: &Address) -> Option<AccountInfo> {
        parent().cache.accounts.get(address).cloned().flatten()
    }

    fn tx(from: u64, nonce: u64, to: Address, value: u64) -> Recovered<N42TxEnvelope> {
        let tx = TxEip1559 {
            chain_id: CHAIN_ID,
            nonce,
            gas_limit: 21_000,
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 3,
            to: TxKind::Call(to),
            value: U256::from(value),
            input: Bytes::new(),
            ..Default::default()
        };
        let signed = Signed::new_unchecked(tx, Signature::test_signature(), B256::left_padding_from(&(from * 1000 + nonce).to_be_bytes()));
        Recovered::new_unchecked(N42TxEnvelope::from(reth_ethereum_primitives::TransactionSigned::from(signed)), addr(from))
    }

    fn env() -> reth_evm::EvmEnvFor<Config> {
        let header = alloy_consensus::Header {
            number: 5,
            timestamp: 1_700_000_000,
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(7),
            beneficiary: addr(BENEFICIARY),
            ..Default::default()
        };
        config().evm_env(&header).expect("env")
    }

    /// Executes `txs` in order on one EVM over the parent, with `cache`
    /// written into its overlay first; the block accumulator's bundle and,
    /// if every change is modelled, a batch state's.
    fn execute(txs: &[Recovered<N42TxEnvelope>], cache: &[(Address, AccountInfo)]) -> (BundleState, BundleState) {
        let c = config();
        let mut evm = c.evm_with_env(parent(), env());
        for (address, info) in cache {
            evm.overlay_db_mut().insert_account_info(address, info.clone());
        }
        let (mut block, mut batch) = (BlockState::new(), BatchState::with_capacity(8));
        for t in txs {
            let t = t.clone().map(evm2::ethereum::TxEnvelope::from);
            let executed = evm.transact(&t).expect("the transaction executes").detach();
            for changes in executed.pending_state.account_changes() {
                block.commit_account_changes(changes, None);
            }
            batch.commit(&executed.pending_state).expect("a transfer");
            evm.state_mut().set_pending_state(executed.pending_state);
            evm.state_mut().commit_transaction();
        }
        (block.into_bundle(), batch.take_bundle())
    }

    fn sorted_reverts(bundle: &BundleState) -> Vec<Vec<(Address, String)>> {
        bundle
            .reverts
            .iter()
            .map(|block| {
                let mut v: Vec<_> = block.iter().map(|(a, r)| (*a, format!("{r:?}"))).collect();
                v.sort_by_key(|(a, _)| *a);
                v
            })
            .collect()
    }

    fn assert_same(ours: &BundleState, theirs: &BundleState) {
        assert_eq!(ours.state, theirs.state, "state");
        assert_eq!(sorted_reverts(ours), sorted_reverts(theirs), "reverts");
        assert_eq!(ours.state_size, theirs.state_size, "state size");
    }

    /// Two batches sharing a recipient and the beneficiary, then a serial
    /// tail spending from that recipient and from a batch sender, run over
    /// the cache half: the folded bundle is the sequential execution's, and
    /// reverting it by index gives back the parent.
    #[test]
    fn the_fold_equals_the_sequential_execution_and_reverts_to_the_parent() {
        let fresh = addr(0xf00d);
        let a = vec![tx(1, 0, addr(2), 100), tx(3, 0, fresh, 100_000_000_000_000_000)];
        let b = vec![tx(5, 0, addr(2), 300), tx(6, 0, addr(7), 400)];
        let tail = vec![tx(2, 0, addr(9), 50), tx(1, 1, addr(10), 60), tx(0xf00d, 0, addr(11), 10)];

        let sequential: Vec<_> = a.iter().chain(&b).chain(&tail).cloned().collect();
        let (reference, _) = execute(&sequential, &[]);

        let (_, bundle_a) = execute(&a, &[]);
        let (_, bundle_b) = execute(&b, &[]);
        let merged = merge_batches(vec![bundle_a, bundle_b]).expect("mergeable");
        let cache: Vec<_> = merged.accounts().map(|(a, i)| (*a, i.clone())).collect();
        let (mut serial, _) = execute(&tail, &cache);
        let stats = fold_into(&mut serial, merged).expect("foldable");
        assert!(stats.overlapped >= 3, "{stats:?}");
        assert_same(&serial, &reference);

        // Reverting the folded bundle by index gives back the parent.
        let touched: Vec<Address> = serial.state.keys().copied().collect();
        let mut reverted = serial;
        assert!(reverted.revert_latest());
        for address in touched {
            let now = reverted.state.get(&address).and_then(|a| a.info.clone());
            let want = parent_info(&address);
            assert!(
                now == want || (now.is_none() && !reverted.state.contains_key(&address)),
                "{address}: {now:?} after the revert, {want:?} in the parent"
            );
        }
    }

    fn info(balance: u64, nonce: u64) -> AccountInfo {
        AccountInfo::new(U256::from(balance), nonce, alloy_primitives::KECCAK256_EMPTY, None)
    }

    fn one_account_bundle(address: Address, original: Option<AccountInfo>, now: AccountInfo, status: AccountStatus) -> BundleState {
        let account = BundleAccount { info: Some(now), original_info: original.clone(), storage: Default::default(), status };
        let revert = AccountRevert {
            account: match original {
                Some(o) => AccountInfoRevert::RevertTo(o),
                None => AccountInfoRevert::DeleteIt,
            },
            storage: Default::default(),
            previous_status: loaded_status(None),
            wipe_storage: false,
        };
        let mut bundle = BundleState { state_size: account.size_hint(), ..Default::default() };
        bundle.state.insert(address, account);
        bundle.reverts.push(vec![(address, revert)]);
        bundle
    }

    /// A batch created the account; the serial part changed it again, which
    /// on its own reads as `Changed` over an existing account. Folded, the
    /// status is recomputed from the parent's absence: created in memory,
    /// with a revert that deletes it.
    #[test]
    fn a_serial_change_over_a_batch_created_account_reads_as_created() {
        let x = addr(0xc4);
        let batch = one_account_bundle(x, None, info(500, 0), AccountStatus::InMemoryChange);
        let merged = merge_batches(vec![batch]).expect("mergeable");
        let mut serial = one_account_bundle(x, Some(info(500, 0)), info(300, 1), AccountStatus::Changed);
        fold_into(&mut serial, merged).expect("foldable");
        let account = &serial.state[&x];
        assert_eq!(account.status, AccountStatus::InMemoryChange);
        assert_eq!(account.original_info, None);
        assert_eq!(account.info, Some(info(300, 1)));
        assert_eq!(serial.reverts.len(), 1);
        let (address, revert) = &serial.reverts[0][0];
        assert_eq!((*address, &revert.account), (x, &AccountInfoRevert::DeleteIt));
        assert_eq!(serial.reverts[0].len(), 1, "the serial revert to the post-batch value is dropped");
    }

    /// The batches credited the account, the serial part took exactly that
    /// back: its final info is the parent's. The accumulator keeps such an
    /// account (a change with nothing to restore is still a change of the
    /// block), and so does the fold, with the serial part's revert to the
    /// post-batch value replaced by the accumulator's.
    #[test]
    fn a_serial_change_back_to_the_parent_value_folds_as_the_accumulator_does() {
        let x = addr(0xc5);
        let y = addr(0xc8);
        let mut batch = one_account_bundle(x, Some(info(1_000, 3)), info(1_500, 3), AccountStatus::Changed);
        // A second account the batches changed and nothing else touched.
        let other = one_account_bundle(y, Some(info(10, 0)), info(20, 0), AccountStatus::Changed);
        batch.state.extend(other.state);
        batch.reverts[0].extend(other.reverts[0].clone());
        let merged = merge_batches(vec![batch]).expect("mergeable");
        let mut serial = one_account_bundle(x, Some(info(1_500, 3)), info(1_000, 3), AccountStatus::Changed);
        fold_into(&mut serial, merged).expect("foldable");

        // The same two changes of `x` through the accumulator.
        let mut block = BlockState::new();
        let (parent, credited) = (info(1_000, 3), info(1_500, 3));
        for (original, current) in [(&parent, &credited), (&credited, &parent)] {
            use evm2::evm::StateChangeSink as _;
            let mut tx = evm2::block::TransactionChanges::default();
            tx.account(evm2::evm::AccountChangeRef {
                address: x,
                original: Some(original),
                current: Some(current),
                created: false,
                selfdestructed: false,
            })
            .expect("infallible");
            block.commit(&tx);
        }
        let reference = block.into_bundle();
        assert_eq!(serial.state.get(&x), reference.state.get(&x));
        let revert_of = |bundle: &BundleState| bundle.reverts.iter().flatten().find(|(a, _)| *a == x).map(|(_, r)| r.clone());
        assert_eq!(revert_of(&serial), revert_of(&reference));
        // The other account's revert, and this one's when the accumulator has one.
        assert_eq!(serial.reverts[0].len(), 1 + usize::from(revert_of(&reference).is_some()));
        assert!(serial.state.contains_key(&y));
    }

    /// The serial part read a batch account at the parent's value -- a
    /// withdrawal recipient the cache half missed: refused, the block left
    /// as it was, the counter up.
    #[test]
    fn an_account_the_cache_half_missed_is_refused() {
        let x = addr(0xc6);
        let batch = one_account_bundle(x, Some(info(1_000, 0)), info(1_200, 0), AccountStatus::Changed);
        let merged = merge_batches(vec![batch]).expect("mergeable");
        let mut serial = one_account_bundle(x, Some(info(1_000, 0)), info(1_100, 0), AccountStatus::Changed);
        let before = serial.clone();
        let count = refused_count();
        assert_eq!(fold_into(&mut serial, merged), Err(FoldRefused::Stale(x)));
        assert_eq!(serial, before);
        assert!(refused_count() > count);
    }

    /// The merge's own refusals: storage, an overflow, two debits, different
    /// originals.
    #[test]
    fn the_merge_refuses_what_it_cannot_model() {
        let x = addr(0xc7);
        let mut with_storage = one_account_bundle(x, Some(info(1, 0)), info(2, 0), AccountStatus::Changed);
        with_storage.state.get_mut(&x).expect("x").storage.insert(U256::from(1), StorageSlot::new_changed(U256::ZERO, U256::from(1)));
        assert_eq!(merge_batches(vec![with_storage]).unwrap_err(), FoldRefused::Storage(x));

        // Two credits that do not fit the balance's range.
        let top = |by: u64| {
            let mut near = info(0, 0);
            near.balance = U256::MAX - U256::from(10u64);
            let mut after = near.clone();
            after.balance += U256::from(by);
            one_account_bundle(x, Some(near), after, AccountStatus::Changed)
        };
        assert_eq!(merge_batches(vec![top(7), top(7)]).unwrap_err(), FoldRefused::Arithmetic(x));

        let debit = || one_account_bundle(x, Some(info(10, 0)), info(5, 1), AccountStatus::Changed);
        assert_eq!(merge_batches(vec![debit(), debit()]).unwrap_err(), FoldRefused::TwoDebits(x));

        let a = one_account_bundle(x, Some(info(10, 0)), info(12, 0), AccountStatus::Changed);
        let b = one_account_bundle(x, Some(info(11, 0)), info(13, 0), AccountStatus::Changed);
        assert_eq!(merge_batches(vec![a, b]).unwrap_err(), FoldRefused::Originals(x));

        // A debit and a credit merge, the credit added onto the debit.
        let a = one_account_bundle(x, Some(info(10, 0)), info(5, 1), AccountStatus::Changed);
        let b = one_account_bundle(x, Some(info(10, 0)), info(17, 0), AccountStatus::Changed);
        let merged = merge_batches(vec![a, b]).expect("mergeable");
        assert_eq!(merged.get(&x), Some(&info(12, 1)));
    }

    #[test]
    fn a_prefaulted_table_merges_as_a_fresh_one() {
        let batches = || {
            (0..600u64)
                .map(|n| {
                    let x = addr(n % 400);
                    let original = info(1_000, 0);
                    let now = if n < 400 { info(900, 1) } else { info(1_005, 0) };
                    CheckedBundle::new(one_account_bundle(x, Some(original), now, AccountStatus::Changed))
                        .expect("checked")
                })
                .collect::<Vec<_>>()
        };
        let accounts = 600 * 2 + 64;
        let mut fresh = BatchMerger::with_capacity(accounts);
        let state = prefaulted_state(accounts);
        assert!(state.is_empty());
        let mut prefaulted = BatchMerger::with_state(state);
        for (a, b) in batches().into_iter().zip(batches()) {
            fresh.add(a).expect("merges");
            prefaulted.add(b).expect("merges");
        }
        let (fresh, prefaulted) = (fresh.finish(), prefaulted.finish());
        assert_eq!(fresh, prefaulted);
        assert_eq!(prefaulted.addresses().count(), 400);
        assert_eq!(fresh.get(&addr(3)), Some(&info(905, 1)));
    }
}
