//! Single-transaction execution + read-set validation (the per-tx primitives the
//! worker loop drives).

use crate::coinbase::DeferredCoinbase;
use crate::mv_memory::{MvMemory, MvRead};
use crate::output::TxOutputInternal;
use crate::parallel_db::{ParallelDb, SharedReadSet};
use crate::types::{AccountSnapshot, AccountWrite, LocationKey, ReadEntry, ReadValue, TxIdx};
use revm::context::{BlockEnv, CfgEnv, Context, Journal, TxEnv};
use revm::context_interface::result::ResultAndState;
use revm::database_interface::DatabaseRef;
use revm::handler::MainBuilder;
use std::fmt;
use std::sync::Arc;
use tracing::debug;

/// Extract the read set from its `Arc` wrapper, avoiding a clone when possible.
fn unwrap_read_set(read_set: SharedReadSet) -> Vec<ReadEntry> {
    match Arc::try_unwrap(read_set) {
        Ok(mutex) => mutex.into_inner(),
        Err(arc) => arc.lock().clone(),
    }
}

/// Execute a single transaction with the parallel database adapter.
///
/// `coinbase` is `Some` only when deferral is active; its beneficiary is read
/// "blind" by [`ParallelDb`] and its post-state delta is recorded commutatively
/// instead of as a versioned write.
pub(crate) fn execute_single_tx<DB>(
    tx_idx: TxIdx,
    tx_env: &TxEnv,
    base_db: &DB,
    mv: &MvMemory,
    cfg_env: &CfgEnv,
    block_env: &BlockEnv,
    coinbase: Option<&DeferredCoinbase>,
) -> Option<TxOutputInternal>
where
    DB: DatabaseRef + Send + Sync,
    DB::Error: fmt::Display + Send,
{
    let read_set: SharedReadSet = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let deferred_bene = coinbase.map(|c| c.address());
    let db = ParallelDb::new(tx_idx, base_db, mv, read_set.clone(), deferred_bene);

    // Build the EVM context and execute.
    let mut ctx: Context<
        BlockEnv,
        TxEnv,
        CfgEnv,
        ParallelDb<'_, DB>,
        Journal<ParallelDb<'_, DB>>,
        (),
    > = Context::new(db, cfg_env.spec);
    ctx.block = block_env.clone();
    ctx.cfg = cfg_env.clone();

    let mut evm = ctx.build_mainnet();

    let ResultAndState { result, state } =
        match revm::handler::ExecuteEvm::transact(&mut evm, tx_env.clone()) {
            Ok(r) => r,
            Err(e) => {
                debug!(target: "n42::parallel_evm", tx_idx, error = %e, "tx execution error");
                let rs = unwrap_read_set(read_set);
                return Some(TxOutputInternal {
                    gas_used: 0,
                    success: false,
                    logs: vec![],
                    account_writes: vec![],
                    storage_writes: vec![],
                    read_set: rs,
                    bene_delta: None,
                });
            }
        };

    // EIP-8037 split gas_used into tx_gas_used (execution) and state_gas_used.
    // For N42 metrics + receipt totals we want the transaction-level gas, which
    // matches the pre-EIP semantics of the deprecated `gas_used()`.
    let gas_used = result.tx_gas_used();
    let success = result.is_success();
    let logs = result.into_logs();

    // Extract state changes.
    let mut account_writes = Vec::new();
    let mut storage_writes = Vec::new();
    let mut bene_delta = None;

    for (addr, account) in &state {
        if !account.is_touched() {
            continue;
        }

        // Deferred beneficiary: record the commutative balance delta instead of a
        // versioned write (so it never conflicts). `extract_delta` returns None if
        // nonce/code changed — defensive fallback to a normal write. Storage writes
        // stay versioned regardless.
        if let Some(cb) = coinbase.filter(|c| c.address() == *addr) {
            match cb.extract_delta(account) {
                Some(delta) => bene_delta = Some(delta),
                None => account_writes.push((*addr, AccountWrite::Updated(account.info.clone()))),
            }
            for (slot, storage) in &account.storage {
                if storage.is_changed() {
                    storage_writes.push((*addr, *slot, storage.present_value));
                }
            }
            continue;
        }

        account_writes.push((*addr, AccountWrite::Updated(account.info.clone())));
        for (slot, storage) in &account.storage {
            if storage.is_changed() {
                storage_writes.push((*addr, *slot, storage.present_value));
            }
        }
    }

    Some(TxOutputInternal {
        gas_used,
        success,
        logs,
        account_writes,
        storage_writes,
        read_set: unwrap_read_set(read_set),
        bene_delta,
    })
}

/// Validate a transaction's read set: every recorded read must still observe the
/// SAME value now (re-reading what is visible to `tx_idx`). Value-based — not
/// writer-identity-based — so a lower tx that re-executed and wrote a *different*
/// value under the same index correctly invalidates this tx (the hot-account
/// soundness fix; see `docs/devlog-67`).
pub(crate) fn validate_read_set<DB>(
    tx_idx: TxIdx,
    read_set: &[ReadEntry],
    mv: &MvMemory,
    base_db: &DB,
) -> bool
where
    DB: DatabaseRef,
    DB::Error: fmt::Display,
{
    read_set.iter().all(|entry| match &entry.key {
        LocationKey::Account(addr) => {
            let now = match mv.read_account(tx_idx, *addr) {
                MvRead::Value(_, info) => info,
                MvRead::NotFound => base_db.basic_ref(*addr).ok().flatten(),
            };
            entry.value == ReadValue::Account(AccountSnapshot::of(&now))
        }
        LocationKey::Storage(addr, slot) => {
            let now = match mv.read_storage(tx_idx, *addr, *slot) {
                MvRead::Value(_, v) => v,
                MvRead::NotFound => base_db.storage_ref(*addr, *slot).unwrap_or_default(),
            };
            entry.value == ReadValue::Storage(now)
        }
    })
}
