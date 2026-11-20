//! Storage slots a block changed and restored — the writes gov5 records and
//! the block's bundle forgets.
//!
//! gov5 commits a block's state to QMDB by writing every dirty account and
//! storage slot of the block — dirty meaning written, not changed. A slot
//! that one transaction moves and a later one moves back is dirty with its
//! original value, and gov5 writes it (`BufferedPlainStateWriter`, which
//! refuses to short-circuit on `original == value`); QMDB appends a fresh
//! slot for the key and the root moves. The bundle, which is all reth hands
//! the state-root job, drops exactly that slot: a slot whose value at the end
//! of the block equals its value at the start is not a change. Chain 94
//! showed the case at block 13,561,251: an ERC-20 balance moved and moved
//! back within the block, gov5's root counted the rewrite, the bundle had no
//! trace of it.
//!
//! Every transaction's result passes through [`TrackingExecutor`] before it
//! is committed, so the slots each one changes are seen with their values at
//! the start of that transaction; the first sighting of a slot in a block is
//! its value at the block's start, and that is the value gov5 rewrites when
//! the slot ends up back there. The block's list is filed at `finish` under
//! the parent, full execution context and ordered transaction hashes —
//! known to both the executor and state-root job — in a
//! bounded registry the root job and the payload builder read.
//!
//! Successful SSTORE operations are also observed so a slot changed and
//! restored within one transaction is not lost in the transaction's net
//! result (see `dirty_storage`).
//!
//! The same pass keeps the accounts a transaction touched and left as they
//! were. gov5 writes an account leaf for each of them, and the revm executor
//! this chain's roots were produced with kept them in the bundle with a
//! modified status; the evm2 block state commits only accounts that changed.
//! [`TrackingExecutor`] puts them back into the bundle it returns (see
//! [`touched_unchanged`] for the rule), so every reader of the bundle — the
//! QMDB operations, the shared execution output, the EOF guard — sees what it
//! saw before.

use alloy_consensus::{Header, transaction::TxHashRef};
use alloy_eip7928::{BlockAccessIndex, BlockAccessList};
use alloy_eips::{eip2718::Typed2718, eip4895::Withdrawal};
use alloy_primitives::{Address, B256, U256, keccak256};
use evm2::{
    TxResult, TxStateChanges,
    block::bundle_state::{AccountStatus, BundleAccount},
    env::BlockEnvExt,
    evm::{AccountInfo, Bal},
};
use reth_chainspec::{EthChainSpec, EthereumHardforks};
use reth_evm::{
    BlockExecutionError, BlockExecutionOutput, BlockExecutor, BlockExecutorFactory,
    BlockTransactionResult, CommitChanges, Evm, ExecutorTx, GasOutput, ReceiptBuilder, RecoveredTx,
    StateUpdate, execute::ExecutableTxParts,
};
use reth_evm_ethereum::{EthBlockExecutionCtx, EthBlockExecutorFactory, EvmFactory};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Arc, Mutex, PoisonError},
};

/// A storage slot a block changed and then restored to its value at the
/// block's start: `(address, slot, that value)`.
pub type RestoredSlot = (Address, U256, U256);

/// Bind the committed transactions, in order, to an execution context.
///
/// The key only names an entry of this process's registry; it never leaves
/// the node. BLAKE3 is used because a full native block hashes 3 MB of
/// transaction hashes here, twice per import (executor and root job), which
/// took about 10 ms each with Keccak-256.
pub fn restored_slots_key(context_key: B256, tx_hashes: impl IntoIterator<Item = B256>) -> B256 {
    let mut hasher = blake3::Hasher::new();
    hasher.update(context_key.as_slice());
    for hash in tx_hashes {
        hasher.update(hash.as_slice());
    }
    B256::from(*hasher.finalize().as_bytes())
}

/// Bind restored writes to the block environment, not just parent and txs.
/// Two speculative builds may execute identical transactions at different
/// timestamps, coinbases or blob prices and restore different storage slots.
pub fn restored_slots_env_key<E>(block: &BlockEnvExt<E>) -> B256 {
    let mut bytes = Vec::with_capacity(8 * 32 + 20);
    bytes.extend_from_slice(&block.number.to_be_bytes::<32>());
    bytes.extend_from_slice(&block.timestamp.to_be_bytes::<32>());
    bytes.extend_from_slice(block.beneficiary.as_slice());
    bytes.extend_from_slice(&block.gas_limit.to_be_bytes::<32>());
    bytes.extend_from_slice(&block.basefee.to_be_bytes::<32>());
    bytes.extend_from_slice(&block.difficulty.to_be_bytes::<32>());
    bytes.extend_from_slice(&block.prevrandao.to_be_bytes::<32>());
    bytes.extend_from_slice(&block.blob_basefee.to_be_bytes::<32>());
    bytes.extend_from_slice(&block.slot_num.to_be_bytes::<32>());
    keccak256(bytes)
}

pub fn restored_slots_execution_key(
    parent_hash: B256,
    env_key: B256,
    parent_beacon_root: Option<B256>,
    withdrawals: Option<&[Withdrawal]>,
    tx_hashes: impl IntoIterator<Item = B256>,
) -> B256 {
    let mut bytes = Vec::with_capacity(32 * 64);
    bytes.extend_from_slice(b"N42 restored storage context v1");
    bytes.extend_from_slice(parent_hash.as_slice());
    bytes.extend_from_slice(env_key.as_slice());
    bytes.push(u8::from(parent_beacon_root.is_some()));
    bytes.extend_from_slice(parent_beacon_root.unwrap_or_default().as_slice());
    let withdrawals = withdrawals.unwrap_or_default();
    bytes.extend_from_slice(&(withdrawals.len() as u64).to_be_bytes());
    for withdrawal in withdrawals {
        // The index fields do not change the executed balance credit.
        bytes.extend_from_slice(withdrawal.address.as_slice());
        bytes.extend_from_slice(&withdrawal.amount.to_be_bytes());
    }
    for hash in tx_hashes {
        bytes.extend_from_slice(hash.as_slice());
    }
    keccak256(bytes)
}

/// Restored slots of recently executed blocks, by key. Bounded: a block is
/// executed and its root computed within moments, but a build that is never
/// used, or a validation that fails before the root, would otherwise leak.
struct RestoredSlotsRegistry {
    slots: HashMap<B256, Arc<Vec<RestoredSlot>>>,
    order: VecDeque<B256>,
    /// Blocks whose lists are kept.
    kept: usize,
}

impl RestoredSlotsRegistry {
    fn new(kept: usize) -> Self {
        Self {
            slots: HashMap::new(),
            order: VecDeque::new(),
            kept,
        }
    }

    /// Files `slots` under `key`, and drops the oldest keys over the bound.
    fn record(&mut self, key: B256, slots: Vec<RestoredSlot>) {
        if self.slots.insert(key, Arc::new(slots)).is_none() {
            self.order.push_back(key);
        }
        while self.order.len() > self.kept {
            if let Some(old) = self.order.pop_front() {
                self.slots.remove(&old);
            }
        }
    }

    fn get(&self, key: &B256) -> Option<Arc<Vec<RestoredSlot>>> {
        self.slots.get(key).cloned()
    }
}

/// Blocks whose lists are kept. The root job runs right after execution;
/// the builder reads its own block's list before sealing.
const RESTORED_SLOTS_KEPT: usize = 512;

static RESTORED_SLOTS: Mutex<Option<RestoredSlotsRegistry>> = Mutex::new(None);

/// Files a block's restored slots under `key`; an empty list is filed too,
/// so a look-up can tell "none" from "not executed here".
pub fn record_restored_slots(key: B256, slots: Vec<RestoredSlot>) {
    RESTORED_SLOTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get_or_insert_with(|| RestoredSlotsRegistry::new(RESTORED_SLOTS_KEPT))
        .record(key, slots);
}

/// The restored slots filed under `key`, if the block was executed here.
pub fn restored_slots_for(key: B256) -> Option<Arc<Vec<RestoredSlot>>> {
    let guard = RESTORED_SLOTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    guard.as_ref()?.get(&key)
}

/// A block executor factory whose executors record restored slots.
#[derive(Clone, Debug)]
pub struct TrackingBlockExecutorFactory<F> {
    inner: F,
}

impl<F> TrackingBlockExecutorFactory<F> {
    /// Wraps `inner`.
    pub const fn new(inner: F) -> Self {
        Self { inner }
    }

    /// The wrapped factory.
    pub const fn inner(&self) -> &F {
        &self.inner
    }
}

impl<R, C, F> BlockExecutorFactory
    for TrackingBlockExecutorFactory<EthBlockExecutorFactory<R, C, F>>
where
    C: EthChainSpec<Header = Header> + EthereumHardforks,
    F: EvmFactory,
    R: ReceiptBuilder<Transaction: TxHashRef>,
    <F::Types as evm2::EvmTypesHost>::Tx: Typed2718,
{
    type EvmFactory = F;
    type EvmTypes = F::Types;
    type Transaction = R::Transaction;
    type Receipt = R::Receipt;
    type Evm<'a> = evm2::Evm<'a, F::Types>;
    type EvmEnv = <EthBlockExecutorFactory<R, C, F> as BlockExecutorFactory>::EvmEnv;
    type ExecutionCtx<'a>
        = EthBlockExecutionCtx<'a>
    where
        Self: 'a;
    type Executor<'a>
        = TrackingExecutor<<EthBlockExecutorFactory<R, C, F> as BlockExecutorFactory>::Executor<'a>>
    where
        Self: 'a;

    fn create_executor<'a>(
        &'a self,
        evm: Self::Evm<'a>,
        ctx: Self::ExecutionCtx<'a>,
    ) -> Self::Executor<'a>
    where
        Self: 'a,
    {
        let parent_hash = ctx.parent_hash;
        let env_key = restored_slots_env_key(evm.block());
        let context_key = restored_slots_execution_key(
            parent_hash,
            env_key,
            ctx.parent_beacon_block_root,
            ctx.withdrawals.as_deref(),
            [],
        );
        TrackingExecutor {
            inner: self.inner.create_executor(evm, ctx),
            context_key,
            tx_hashes: Vec::new(),
            first_seen: HashMap::new(),
            touched: HashMap::new(),
            pending: None,
            pending_fold: None,
        }
    }

    fn evm_factory(&self) -> &Self::EvmFactory {
        self.inner.evm_factory()
    }

    fn evm_with_env<'a, DB>(&self, db: DB, evm_env: Self::EvmEnv) -> Self::Evm<'a>
    where
        DB: evm2::evm::DynDatabase + 'a,
    {
        BlockExecutorFactory::evm_with_env(&self.inner, db, evm_env)
    }
}

/// A block executor that notes, for every slot a transaction changes, the
/// value the slot had when the block began, and files the block's list at
/// the end.
#[derive(Debug)]
pub struct TrackingExecutor<E> {
    inner: E,
    context_key: B256,
    /// The committed transactions, in order.
    tx_hashes: Vec<B256>,
    /// Each slot changed so far, with its value at the block's start.
    first_seen: HashMap<(Address, U256), U256>,
    /// Accounts a committed transaction touched and left unchanged.
    touched: HashMap<Address, AccountInfo>,
    /// The transaction executed but not yet committed: its hash, the slots
    /// it changed with their values before it ran, and the accounts it
    /// touched and left unchanged.
    pending: Option<(B256, Vec<RestoredSlot>, Vec<TouchedAccount>)>,
    /// The parallel transfer batches' merged changes, folded into the bundle
    /// at the end (`graft_fold`).
    pending_fold: Option<crate::graft_fold::MergedBatches>,
}

/// An account a transaction touched and left unchanged, without its code.
type TouchedAccount = (Address, AccountInfo);

/// The accounts of a transaction that revm's state merge entered into the
/// bundle although nothing about them changed.
///
/// revm 43 (`CacheState::apply_account_state`) turned every touched account of
/// a transaction's result into a transition: a selfdestructed or created one
/// by its own rule, an empty one into a removal (none at all when it did not
/// exist), and any other one into a change — with or without a difference —
/// which the bundle kept with a modified status. evm2 commits the first three
/// itself and drops the last when nothing differs, so that case is the one
/// collected here: touched, not changed, existing and not empty. A touch is
/// what it was in revm: the target of a CALL, CALLCODE or STATICCALL or of
/// the transaction itself, whatever the value, and the beneficiary's fee
/// credit, whatever the amount; a touch made in a reverted frame is undone.
/// `known` are accounts already collected in this block.
fn touched_unchanged(
    changes: TxStateChanges<'_>,
    known: &HashMap<Address, AccountInfo>,
) -> Vec<TouchedAccount> {
    let mut touched = Vec::new();
    for account in changes.account_changes() {
        if !account.touched || account.is_changed() || known.contains_key(&account.address) {
            continue;
        }
        let Some(info) = account.current.filter(|info| !info.is_empty()) else {
            continue;
        };
        touched.push((
            account.address,
            AccountInfo::new(info.balance, info.nonce, info.code_hash, None),
        ));
    }
    touched
}

/// Enters the touched and unchanged accounts the block's bundle lacks, with
/// the status revm gave them: loaded, then changed.
fn add_touched(state: &mut reth_evm::BundleState, touched: HashMap<Address, AccountInfo>) {
    for (address, info) in touched {
        // An account another transaction changed is in the bundle already,
        // with the values that count.
        if state.state.contains_key(&address) {
            continue;
        }
        let status =
            AccountStatus::Loaded.on_changed(info.nonce == 0 && info.is_empty_code_hash());
        let account =
            BundleAccount::new(Some(info.clone()), Some(info), Default::default(), status);
        state.state_size += account.size_hint();
        state.state.insert(address, account);
    }
}

/// Folds the parallel batches' merge, when there is one, into the finished
/// bundle, before anything reads it.
fn fold(
    pending: Option<crate::graft_fold::MergedBatches>,
    state: &mut reth_evm::BundleState,
) -> Result<(), BlockExecutionError> {
    if let Some(merged) = pending {
        crate::graft_fold::fold_into(state, merged).map_err(BlockExecutionError::other)?;
    }
    Ok(())
}

/// Every slot a transaction changed, with the value it saw at its start —
/// the state it was loaded from, which is the block's start for the first
/// transaction to touch it. `stores` are the slots an SSTORE moved away from
/// that value, wherever they ended up.
fn changed_slots(changes: TxStateChanges<'_>, stores: Vec<(Address, U256)>) -> Vec<RestoredSlot> {
    let mut changed = Vec::new();
    let mut listed = HashSet::new();
    for account in changes.account_changes() {
        if account.selfdestructed {
            continue;
        }
        for (slot, value) in account.storage.changed_slots() {
            changed.push((account.address, *slot, value.original));
            listed.insert((account.address, *slot));
        }
    }
    for (address, slot) in stores {
        if listed.contains(&(address, slot)) {
            continue;
        }
        let Some(account) = changes.account_changes_for(&address) else {
            continue;
        };
        if account.selfdestructed {
            continue;
        }
        let Some(entry) = account.storage.slots.get(&slot) else {
            continue;
        };
        changed.push((address, slot, entry.value.original));
        listed.insert((address, slot));
    }
    changed
}

impl<E> TrackingExecutor<E> {
    /// The wrapped executor.
    pub const fn inner(&self) -> &E {
        &self.inner
    }

    /// The wrapped executor. Transactions executed or committed through it
    /// directly are not recorded.
    pub const fn inner_mut(&mut self) -> &mut E {
        &mut self.inner
    }

    fn record(&mut self, hash: B256, changed: Vec<RestoredSlot>, touched: Vec<TouchedAccount>) {
        self.tx_hashes.push(hash);
        for (address, info) in touched {
            self.touched.entry(address).or_insert(info);
        }
        for (address, slot, original) in changed {
            self.first_seen.entry((address, slot)).or_insert(original);
        }
    }

    fn file(&mut self) {
        // Filed whole; the root job keeps the ones the bundle lost.
        let mut restored: Vec<RestoredSlot> = std::mem::take(&mut self.first_seen)
            .into_iter()
            .map(|((address, slot), original)| (address, slot, original))
            .collect();
        restored.sort_unstable();
        record_restored_slots(
            restored_slots_key(self.context_key, self.tx_hashes.iter().copied()),
            restored,
        );
    }
}

impl<E: BlockExecutor> TrackingExecutor<E> {
    /// Hands over the parallel batches' merge for the end of the block to
    /// fold into the bundle, with the accounts the batches' transfers touched
    /// and left unchanged. The caller has written the merged accounts into
    /// the EVM's overlay and committed the batches' receipts
    /// (`parallel_build::commit_run`).
    pub(crate) fn set_pending_fold(
        &mut self,
        merged: crate::graft_fold::MergedBatches,
        touched: Vec<TouchedAccount>,
    ) {
        for (address, info) in touched {
            self.touched.entry(address).or_insert(info);
        }
        self.pending_fold = Some(merged);
    }

    /// Whether a fold is pending.
    pub(crate) const fn has_pending_fold(&self) -> bool {
        self.pending_fold.is_some()
    }

    /// Record a locally executed transfer whose changes are in the pending
    /// fold. Only the execution module may bypass the ordinary
    /// per-transaction path.
    pub(crate) fn commit_preexecuted_transfer(
        &mut self,
        hash: B256,
        output: E::TransactionResultWithState,
    ) -> Result<GasOutput, BlockExecutionError> {
        if self.pending.is_some() || !output.result().pending_state.is_empty() {
            return Err(BlockExecutionError::other(std::io::Error::other(
                "preexecuted transfer overlaps pending execution or carries its own state",
            )));
        }
        self.tx_hashes.push(hash);
        self.inner.commit_transaction(output)
    }
}

impl<E> BlockExecutor for TrackingExecutor<E>
where
    E: BlockExecutor,
    E::Transaction: TxHashRef,
{
    type Transaction = E::Transaction;
    type Receipt = E::Receipt;
    type Evm = E::Evm;
    type TransactionResultWithState = E::TransactionResultWithState;
    type BlockAccessList = E::BlockAccessList;

    fn evm(&self) -> &Self::Evm {
        self.inner.evm()
    }

    fn evm_mut(&mut self) -> &mut Self::Evm {
        self.inner.evm_mut()
    }

    fn set_state_hook(&mut self, hook: impl FnMut(StateUpdate) + Send + 'static) -> bool {
        self.inner.set_state_hook(hook)
    }

    fn convert_block_access_list(
        block_access_list: &BlockAccessList,
    ) -> Result<Self::BlockAccessList, BlockExecutionError> {
        E::convert_block_access_list(block_access_list)
    }

    fn set_block_access_list(&mut self, block_access_list: Arc<Self::BlockAccessList>) {
        self.inner.set_block_access_list(block_access_list)
    }

    fn set_block_access_index(&mut self, index: BlockAccessIndex) {
        self.inner.set_block_access_index(index)
    }

    fn enable_block_access_list_builder(&mut self) {
        self.inner.enable_block_access_list_builder()
    }

    fn take_block_access_list(&mut self) -> Option<BlockAccessList> {
        self.inner.take_block_access_list()
    }

    fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
        self.inner.apply_pre_execution_changes()
    }

    fn into_state(self) -> reth_evm::BundleState {
        let mut state = self.inner.into_state();
        if let Some(merged) = self.pending_fold {
            // A refusal here leaves the serial part's state, as a caller of
            // `into_state` (a partial execution) gets it without the batches.
            let _ = crate::graft_fold::fold_into(&mut state, merged);
        }
        add_touched(&mut state, self.touched);
        state
    }

    fn validate_transaction_gas_limit(
        &mut self,
        gas_limit: u64,
    ) -> Result<(), BlockExecutionError> {
        self.inner.validate_transaction_gas_limit(gas_limit)
    }

    fn execute_transaction_with_commit_condition(
        &mut self,
        transaction: impl ExecutorTx<Self>,
        f: impl FnOnce(&TxResult<<Self::Evm as Evm>::EvmTypes>, TxStateChanges<'_>) -> CommitChanges,
    ) -> Result<Option<GasOutput>, BlockExecutionError> {
        let (tx_env, recovered) = ExecutableTxParts::into_parts(transaction);
        let hash = *recovered.tx().tx_hash();
        let capture = crate::dirty_storage::Capture::start();
        let mut changed = Vec::new();
        let mut touched = Vec::new();
        let known = &self.touched;
        // The inner executor's own path: it commits without detaching the
        // transaction's state, which is read here before the decision.
        let output = self.inner.execute_transaction_with_commit_condition(
            (tx_env, recovered),
            |result, changes| {
                changed = changed_slots(changes, capture.finish());
                touched = touched_unchanged(changes, known);
                f(result, changes)
            },
        )?;
        if output.is_some() {
            self.record(hash, changed, touched);
        }
        Ok(output)
    }

    fn execute_transaction_without_commit(
        &mut self,
        transaction: impl ExecutorTx<Self>,
    ) -> Result<Self::TransactionResultWithState, BlockExecutionError> {
        let (tx_env, recovered) = ExecutableTxParts::into_parts(transaction);
        let hash = *recovered.tx().tx_hash();
        let capture = crate::dirty_storage::Capture::start();
        let executed = self
            .inner
            .execute_transaction_without_commit((tx_env, recovered));
        let stores = capture.finish();
        let output = executed?;
        let changes = output.result().pending_state.changes();
        let changed = changed_slots(changes, stores);
        let touched = touched_unchanged(changes, &self.touched);
        self.pending = Some((hash, changed, touched));
        Ok(output)
    }

    fn commit_transaction(
        &mut self,
        output: Self::TransactionResultWithState,
    ) -> Result<GasOutput, BlockExecutionError> {
        if let Some((hash, changed, touched)) = self.pending.take() {
            self.record(hash, changed, touched);
        }
        self.inner.commit_transaction(output)
    }

    fn receipts(&self) -> &[Self::Receipt] {
        self.inner.receipts()
    }

    fn finish_with_block_access_list(
        mut self,
    ) -> Result<(BlockExecutionOutput<Self::Receipt>, Option<BlockAccessList>), BlockExecutionError>
    {
        self.file();
        let touched = std::mem::take(&mut self.touched);
        let (mut output, block_access_list) = self.inner.finish_with_block_access_list()?;
        fold(self.pending_fold, &mut output.state)?;
        add_touched(&mut output.state, touched);
        Ok((output, block_access_list))
    }

    fn finish_with_prepared_block_access_list(
        mut self,
    ) -> Result<(BlockExecutionOutput<Self::Receipt>, Option<Bal>), BlockExecutionError> {
        self.file();
        let touched = std::mem::take(&mut self.touched);
        let (mut output, block_access_list) =
            self.inner.finish_with_prepared_block_access_list()?;
        fold(self.pending_fold, &mut output.state)?;
        add_touched(&mut output.state, touched);
        Ok((output, block_access_list))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_context_separates_same_parent_and_transactions() {
        let parent = B256::repeat_byte(0xCC);
        let tx = B256::repeat_byte(0xDD);
        let env = evm2::env::BlockEnv::<evm2::BaseEvmTypes>::default();
        let key = |env: &evm2::env::BlockEnv, beacon, withdrawals| {
            let context = restored_slots_execution_key(
                parent,
                restored_slots_env_key(env),
                beacon,
                withdrawals,
                [],
            );
            restored_slots_key(context, [tx])
        };
        let original = key(&env, None, None);
        let mut later = env;
        later.timestamp += U256::from(1);
        assert_ne!(original, key(&later, None, None));
        later = env;
        later.beneficiary = Address::repeat_byte(1);
        assert_ne!(original, key(&later, None, None));
        assert_ne!(original, key(&env, Some(B256::ZERO), None));
        let rewards = [Withdrawal {
            index: 0,
            validator_index: 0,
            address: Address::repeat_byte(2),
            amount: 1,
        }];
        assert_ne!(original, key(&env, None, Some(&rewards)));
        assert_eq!(original, key(&env, None, Some(&[])));
    }

    /// One test for the whole registry: it is process-global, so separate
    /// tests would race each other's evictions.
    #[test]
    fn the_registry_is_keyed_ordered_and_bounded() {
        let key = restored_slots_key(
            B256::repeat_byte(1),
            [B256::repeat_byte(2), B256::repeat_byte(3)],
        );
        assert_ne!(
            key,
            restored_slots_key(
                B256::repeat_byte(1),
                [B256::repeat_byte(3), B256::repeat_byte(2)]
            )
        );
        assert!(restored_slots_for(key).is_none());
        record_restored_slots(
            key,
            vec![(Address::repeat_byte(9), U256::from(1u64), U256::from(2u64))],
        );
        assert_eq!(restored_slots_for(key).unwrap().len(), 1);
        // An empty list is "executed, nothing restored", not "unknown".
        record_restored_slots(key, Vec::new());
        assert!(restored_slots_for(key).unwrap().is_empty());

        let env = evm2::env::BlockEnv::<evm2::BaseEvmTypes>::default();
        let mut later = env;
        later.timestamp += U256::from(1);
        let contexts = [&env, &later].map(|env| {
            restored_slots_execution_key(
                B256::repeat_byte(0xC1),
                restored_slots_env_key(env),
                None,
                None,
                [],
            )
        });
        let contexts =
            contexts.map(|context| restored_slots_key(context, [B256::repeat_byte(0xC2)]));
        let first = vec![(Address::repeat_byte(1), U256::from(1), U256::from(7))];
        let second = vec![(Address::repeat_byte(2), U256::from(2), U256::from(8))];
        record_restored_slots(contexts[0], first.clone());
        record_restored_slots(contexts[1], second.clone());
        assert_eq!(*restored_slots_for(contexts[0]).unwrap(), first);
        assert_eq!(*restored_slots_for(contexts[1]).unwrap(), second);

        // The bound, on a registry of its own with the process's bound:
        // filling the process's one would drop the lists other tests, run
        // beside this one, have filed and not yet read.
        let mut registry = RestoredSlotsRegistry::new(RESTORED_SLOTS_KEPT);
        let keys: Vec<B256> = (0..RESTORED_SLOTS_KEPT + 8)
            .map(|i| restored_slots_key(B256::repeat_byte(0xB0), [B256::from(U256::from(i))]))
            .collect();
        for key in &keys {
            registry.record(*key, Vec::new());
        }
        assert!(registry.get(&keys[0]).is_none(), "the oldest entry is evicted");
        assert!(registry.get(&keys[7]).is_none());
        assert!(registry.get(&keys[8]).is_some(), "and no more than the bound requires");
        assert!(registry.get(keys.last().unwrap()).is_some());
        assert_eq!(registry.slots.len(), RESTORED_SLOTS_KEPT);
        assert_eq!(registry.order.len(), RESTORED_SLOTS_KEPT);
    }
}
