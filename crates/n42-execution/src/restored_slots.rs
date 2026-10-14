//! Storage slots a block changed and restored — the writes gov5 records and
//! revm forgets.
//!
//! gov5 commits a block's state to QMDB by writing every dirty account and
//! storage slot of the block — dirty meaning written, not changed. A slot
//! that one transaction moves and a later one moves back is dirty with its
//! original value, and gov5 writes it (`BufferedPlainStateWriter`, which
//! refuses to short-circuit on `original == value`); QMDB appends a fresh
//! slot for the key and the root moves. revm's bundle, which is all reth
//! hands the state-root job, drops exactly that slot
//! (`TransitionAccount::update`: "if new value is same as original value,
//! remove storage entry"). Chain 94 showed the case at block 13,561,251: an
//! ERC-20 balance moved and moved back within the block, gov5's root counted
//! the rewrite, revm's bundle had no trace of it.
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
//! restored within one transaction is not lost in revm's net result.

use alloy_consensus::transaction::TxHashRef;
use alloy_eips::eip4895::Withdrawal;
use alloy_evm::{
    Evm, EvmFactory, RecoveredTx,
    block::{
        BlockExecutionError, BlockExecutionResult, BlockExecutor, BlockExecutorFactory,
        ExecutableTx, GasOutput, StateDB, TxResult,
    },
    eth::EthBlockExecutionCtx,
};
use alloy_primitives::{Address, B256, U256, keccak256};
use reth_primitives_traits::SignedTransaction;
use revm::{Inspector, context_interface::Block as EvmBlock};
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex, PoisonError},
};

/// A storage slot a block changed and then restored to its value at the
/// block's start: `(address, slot, that value)`.
pub type RestoredSlot = (Address, U256, U256);

/// Bind the committed transactions, in order, to an execution context.
pub fn restored_slots_key(context_key: B256, tx_hashes: impl IntoIterator<Item = B256>) -> B256 {
    let mut preimage = Vec::with_capacity(32 * 64);
    preimage.extend_from_slice(context_key.as_slice());
    for hash in tx_hashes {
        preimage.extend_from_slice(hash.as_slice());
    }
    keccak256(&preimage)
}

/// Bind restored writes to the block environment, not just parent and txs.
/// Two speculative builds may execute identical transactions at different
/// timestamps, coinbases or blob prices and restore different storage slots.
pub fn restored_slots_env_key(block: &impl EvmBlock) -> B256 {
    let mut bytes = Vec::with_capacity(256);
    bytes.extend_from_slice(&block.number().to_be_bytes::<32>());
    bytes.extend_from_slice(&block.timestamp().to_be_bytes::<32>());
    bytes.extend_from_slice(block.beneficiary().as_slice());
    bytes.extend_from_slice(&block.gas_limit().to_be_bytes());
    bytes.extend_from_slice(&block.basefee().to_be_bytes());
    bytes.extend_from_slice(&block.difficulty().to_be_bytes::<32>());
    bytes.push(u8::from(block.prevrandao().is_some()));
    bytes.extend_from_slice(block.prevrandao().unwrap_or_default().as_slice());
    bytes.push(u8::from(block.blob_excess_gas().is_some()));
    bytes.extend_from_slice(&block.blob_excess_gas().unwrap_or_default().to_be_bytes());
    bytes.extend_from_slice(&block.blob_gasprice().unwrap_or_default().to_be_bytes());
    bytes.extend_from_slice(&block.slot_num().to_be_bytes());
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
}

/// Blocks whose lists are kept. The root job runs right after execution;
/// the builder reads its own block's list before sealing.
const RESTORED_SLOTS_KEPT: usize = 512;

static RESTORED_SLOTS: Mutex<Option<RestoredSlotsRegistry>> = Mutex::new(None);

/// Files a block's restored slots under `key`; an empty list is filed too,
/// so a look-up can tell "none" from "not executed here".
pub fn record_restored_slots(key: B256, slots: Vec<RestoredSlot>) {
    let mut guard = RESTORED_SLOTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let registry = guard.get_or_insert_with(|| RestoredSlotsRegistry {
        slots: HashMap::new(),
        order: VecDeque::new(),
    });
    if registry.slots.insert(key, Arc::new(slots)).is_none() {
        registry.order.push_back(key);
    }
    while registry.order.len() > RESTORED_SLOTS_KEPT {
        if let Some(old) = registry.order.pop_front() {
            registry.slots.remove(&old);
        }
    }
}

/// The restored slots filed under `key`, if the block was executed here.
pub fn restored_slots_for(key: B256) -> Option<Arc<Vec<RestoredSlot>>> {
    let guard = RESTORED_SLOTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    guard.as_ref()?.slots.get(&key).cloned()
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

impl<F> BlockExecutorFactory for TrackingBlockExecutorFactory<F>
where
    F: for<'a> BlockExecutorFactory<
            ExecutionCtx<'a> = EthBlockExecutionCtx<'a>,
            Transaction: SignedTransaction + TxHashRef,
        >,
{
    type EvmFactory = F::EvmFactory;
    type TxExecutionResult = F::TxExecutionResult;
    type ExecutionCtx<'a> = EthBlockExecutionCtx<'a>;
    type Transaction = F::Transaction;
    type Receipt = F::Receipt;
    type Executor<'a, DB: StateDB, I: Inspector<<F::EvmFactory as EvmFactory>::Context<DB>>> =
        TrackingExecutor<F::Executor<'a, DB, I>>;

    fn evm_factory(&self) -> &Self::EvmFactory {
        self.inner.evm_factory()
    }

    fn create_executor<'a, DB, I>(
        &'a self,
        evm: <F::EvmFactory as EvmFactory>::Evm<DB, I>,
        ctx: Self::ExecutionCtx<'a>,
    ) -> Self::Executor<'a, DB, I>
    where
        DB: StateDB,
        I: Inspector<<F::EvmFactory as EvmFactory>::Context<DB>>,
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
            pending: None,
        }
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
    /// The transaction executed but not yet committed: its hash and the
    /// slots it changed, with their values before it ran.
    pending: Option<(B256, Vec<RestoredSlot>)>,
}

impl<E: BlockExecutor> TrackingExecutor<E> {
    /// Record a locally executed transfer whose changes were already grafted.
    /// Only the execution module may bypass the ordinary per-transaction path.
    pub(crate) fn commit_preexecuted_transfer(
        &mut self,
        hash: B256,
        output: E::Result,
    ) -> Result<GasOutput, BlockExecutionError> {
        if self.pending.is_some() || !output.result().state.is_empty() {
            return Err(BlockExecutionError::other(std::io::Error::other(
                "preexecuted transfer overlaps pending execution or contains ungrafted state",
            )));
        }
        self.tx_hashes.push(hash);
        Ok(self.inner.commit_transaction(output))
    }
}

impl<E> BlockExecutor for TrackingExecutor<E>
where
    E: BlockExecutor,
    E::Transaction: SignedTransaction + TxHashRef,
{
    type Transaction = E::Transaction;
    type Receipt = E::Receipt;
    type Evm = E::Evm;
    type Result = E::Result;

    fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
        self.inner.apply_pre_execution_changes()
    }

    fn execute_transaction_without_commit(
        &mut self,
        tx: impl ExecutableTx<Self>,
    ) -> Result<Self::Result, BlockExecutionError> {
        let (tx_env, recovered) = tx.into_parts();
        let hash = *recovered.tx().tx_hash();
        let capture = crate::dirty_storage::Capture::start();
        let executed = self
            .inner
            .execute_transaction_without_commit((tx_env, recovered));
        let stores = capture.finish();
        let result = executed?;
        // Every slot this transaction changed, with the value it saw at its
        // start — the state it was loaded from, which is the block's start
        // for the first transaction to touch it.
        let mut changed = Vec::new();
        let mut listed = std::collections::HashSet::new();
        for (address, account) in &result.result().state {
            if account.is_selfdestructed() {
                continue;
            }
            for (slot, value) in &account.storage {
                if value.is_changed() {
                    changed.push((*address, *slot, value.original_value()));
                    listed.insert((*address, *slot));
                }
            }
        }
        for (address, slot, written) in stores {
            if listed.contains(&(address, slot)) {
                continue;
            }
            let Some(account) = result.result().state.get(&address) else {
                continue;
            };
            if account.is_selfdestructed() {
                continue;
            }
            let Some(entry) = account.storage.get(&slot) else {
                continue;
            };
            if written != entry.original_value() {
                changed.push((address, slot, entry.original_value()));
                listed.insert((address, slot));
            }
        }
        self.pending = Some((hash, changed));
        Ok(result)
    }

    fn commit_transaction(&mut self, output: Self::Result) -> GasOutput {
        if let Some((hash, changed)) = self.pending.take() {
            self.tx_hashes.push(hash);
            for (address, slot, original) in changed {
                self.first_seen.entry((address, slot)).or_insert(original);
            }
        }
        self.inner.commit_transaction(output)
    }

    fn finish(
        self,
    ) -> Result<(Self::Evm, BlockExecutionResult<Self::Receipt>), BlockExecutionError> {
        // Filed whole; the root job keeps the ones the bundle lost.
        let mut restored: Vec<RestoredSlot> = self
            .first_seen
            .into_iter()
            .map(|((address, slot), original)| (address, slot, original))
            .collect();
        restored.sort_unstable();
        record_restored_slots(
            restored_slots_key(self.context_key, self.tx_hashes.iter().copied()),
            restored,
        );
        self.inner.finish()
    }

    fn evm_mut(&mut self) -> &mut Self::Evm {
        self.inner.evm_mut()
    }

    fn evm(&self) -> &Self::Evm {
        self.inner.evm()
    }

    fn receipts(&self) -> &[Self::Receipt] {
        self.inner.receipts()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_context_separates_same_parent_and_transactions() {
        let parent = B256::repeat_byte(0xCC);
        let tx = B256::repeat_byte(0xDD);
        let env = revm::context::BlockEnv::default();
        let key = |env: &revm::context::BlockEnv, beacon, withdrawals| {
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
        let mut later = env.clone();
        later.timestamp += U256::from(1);
        assert_ne!(original, key(&later, None, None));
        later = env.clone();
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

        let env = revm::context::BlockEnv::default();
        let mut later = env.clone();
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

        let keys: Vec<B256> = (0..RESTORED_SLOTS_KEPT + 8)
            .map(|i| restored_slots_key(B256::repeat_byte(0xB0), [B256::from(U256::from(i))]))
            .collect();
        for key in &keys {
            record_restored_slots(*key, Vec::new());
        }
        assert!(
            restored_slots_for(keys[0]).is_none(),
            "the oldest entry is evicted"
        );
        assert!(restored_slots_for(*keys.last().unwrap()).is_some());
    }
}
