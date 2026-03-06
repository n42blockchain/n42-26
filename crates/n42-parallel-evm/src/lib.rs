//! Block-STM parallel EVM execution engine for N42.
//!
//! Implements optimistic parallel transaction execution inspired by Grevm 2.1 and Aptos Block-STM.
//! Transactions execute against an MVCC store; conflicts are detected and re-executed automatically.

pub mod mv_memory;
pub mod parallel_db;
pub mod scheduler;
pub mod types;

use alloy_primitives::{Address, Log, U256};
use mv_memory::MvMemory;
use parallel_db::{ParallelDb, SharedReadSet};
use parking_lot::Mutex;
use revm::context::{BlockEnv, CfgEnv, Context, TxEnv};
use revm::context::Journal;
use revm::context_interface::result::ResultAndState;
use revm::database_interface::DatabaseRef;
use revm::handler::MainBuilder;
use revm::state::{Account, EvmStorageSlot};
use scheduler::{Scheduler, Task};
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use tracing::{debug, info, warn};
use types::*;

/// Output of parallel execution.
pub struct ParallelExecutionOutput {
    /// Per-transaction results, in block order.
    pub results: Vec<TxResult>,
    /// Merged state changes (address → account).
    pub state_changes: HashMap<Address, Account>,
}

/// Single transaction result.
pub struct TxResult {
    pub gas_used: u64,
    pub success: bool,
    pub logs: Vec<Log>,
}

/// Execute transactions in parallel using Block-STM.
///
/// Falls back to sequential for small batches (configurable via `N42_PARALLEL_THRESHOLD`).
pub fn parallel_execute<DB>(
    txs: &[TxEnv],
    base_db: &DB,
    cfg_env: CfgEnv,
    block_env: BlockEnv,
) -> Result<ParallelExecutionOutput, ParallelEvmError>
where
    DB: DatabaseRef + Send + Sync,
    DB::Error: fmt::Display + Send,
{
    let num_txs = txs.len();
    if num_txs == 0 {
        return Ok(ParallelExecutionOutput {
            results: vec![],
            state_changes: HashMap::new(),
        });
    }

    let parallel_threshold: usize = std::env::var("N42_PARALLEL_THRESHOLD")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8);

    if num_txs <= parallel_threshold {
        return sequential_execute(txs, base_db, &cfg_env, &block_env);
    }

    let start = std::time::Instant::now();
    let mv = MvMemory::new();
    let scheduler = Scheduler::new(num_txs);

    // Per-tx output storage.
    let outputs: Vec<Mutex<Option<TxOutputInternal>>> =
        (0..num_txs).map(|_| Mutex::new(None)).collect();

    let max_rounds = 10;
    let mut round = 0;

    while !scheduler.all_done() && round < max_rounds {
        round += 1;

        rayon::scope(|s| {
            let num_workers = rayon::current_num_threads().min(num_txs);
            for _ in 0..num_workers {
                s.spawn(|_| {
                    worker_loop(&scheduler, txs, base_db, &mv, &cfg_env, &block_env, &outputs);
                });
            }
        });
    }

    if !scheduler.all_done() {
        warn!(target: "n42::parallel_evm", round, num_txs, "did not converge, falling back to sequential");
        return sequential_execute(txs, base_db, &cfg_env, &block_env);
    }

    let elapsed = start.elapsed();
    info!(
        target: "n42::parallel_evm",
        num_txs,
        rounds = round,
        elapsed_ms = elapsed.as_millis() as u64,
        "parallel execution completed"
    );
    metrics::counter!("n42_parallel_evm_blocks_total").increment(1);
    metrics::histogram!("n42_parallel_evm_duration_ms").record(elapsed.as_millis() as f64);

    build_output(outputs)
}

/// Internal per-tx output (before merging).
struct TxOutputInternal {
    gas_used: u64,
    success: bool,
    logs: Vec<Log>,
    account_writes: Vec<(Address, AccountWrite)>,
    storage_writes: Vec<(Address, U256, U256)>,
    read_set: Vec<ReadEntry>,
}

/// Worker loop: repeatedly fetches and executes tasks from the scheduler.
fn worker_loop<DB>(
    scheduler: &Scheduler,
    txs: &[TxEnv],
    base_db: &DB,
    mv: &MvMemory,
    cfg_env: &CfgEnv,
    block_env: &BlockEnv,
    outputs: &[Mutex<Option<TxOutputInternal>>],
) where
    DB: DatabaseRef + Send + Sync,
    DB::Error: fmt::Display + Send,
{
    let mut retries = 0;
    loop {
        let task = match scheduler.next_task() {
            Some(t) => {
                retries = 0;
                t
            }
            None => {
                if scheduler.all_done() {
                    break;
                }
                retries += 1;
                if retries > 100 {
                    break; // Avoid infinite spin.
                }
                std::thread::yield_now();
                continue;
            }
        };

        match task {
            Task::Execute(tx_idx) => {
                mv.clear_tx(tx_idx);

                let output = execute_single_tx(tx_idx, &txs[tx_idx], base_db, mv, cfg_env, block_env);

                if let Some(ref out) = output {
                    for (addr, write) in &out.account_writes {
                        mv.write_account(tx_idx, *addr, write);
                    }
                    for &(addr, slot, value) in &out.storage_writes {
                        mv.write_storage(tx_idx, addr, slot, value);
                    }
                }

                *outputs[tx_idx].lock() = output;
                scheduler.finish_execution(tx_idx);
            }
            Task::Validate(tx_idx) => {
                let guard = outputs[tx_idx].lock();
                let valid = guard
                    .as_ref()
                    .map(|out| validate_read_set(tx_idx, &out.read_set, mv))
                    .unwrap_or(false);
                drop(guard);

                if valid {
                    scheduler.finish_validation(tx_idx);
                } else {
                    scheduler.abort_and_reschedule(tx_idx);
                }
            }
        }
    }
}

/// Execute a single transaction using revm with the parallel database adapter.
fn execute_single_tx<DB>(
    tx_idx: TxIdx,
    tx_env: &TxEnv,
    base_db: &DB,
    mv: &MvMemory,
    cfg_env: &CfgEnv,
    block_env: &BlockEnv,
) -> Option<TxOutputInternal>
where
    DB: DatabaseRef + Send + Sync,
    DB::Error: fmt::Display + Send,
{
    let read_set: SharedReadSet = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let db = ParallelDb::new(tx_idx, base_db, mv, read_set.clone());

    // Build the EVM context and execute.
    let mut ctx: Context<BlockEnv, TxEnv, CfgEnv, ParallelDb<'_, DB>, Journal<ParallelDb<'_, DB>>, ()> =
        Context::new(db, cfg_env.spec);
    ctx.block = block_env.clone();
    ctx.cfg = cfg_env.clone();

    let mut evm = ctx.build_mainnet();

    let ResultAndState { result, state } = match revm::handler::ExecuteEvm::transact(&mut evm, tx_env.clone()) {
        Ok(r) => r,
        Err(e) => {
            debug!(target: "n42::parallel_evm", tx_idx, error = %e, "tx execution error");
            let rs = match Arc::try_unwrap(read_set) {
                Ok(mutex) => mutex.into_inner(),
                Err(arc) => arc.lock().clone(),
            };
            return Some(TxOutputInternal {
                gas_used: 0,
                success: false,
                logs: vec![],
                account_writes: vec![],
                storage_writes: vec![],
                read_set: rs,
            });
        }
    };

    let gas_used = result.gas_used();
    let success = result.is_success();
    let logs = result.into_logs();

    // Extract state changes.
    let mut account_writes = Vec::new();
    let mut storage_writes = Vec::new();

    for (addr, account) in &state {
        if account.is_touched() {
            account_writes.push((*addr, AccountWrite::Updated(account.info.clone())));

            for (slot, storage) in &account.storage {
                if storage.is_changed() {
                    storage_writes.push((*addr, *slot, storage.present_value));
                }
            }
        }
    }

    let rs = match Arc::try_unwrap(read_set) {
        Ok(mutex) => mutex.into_inner(),
        Err(arc) => arc.lock().clone(),
    };

    Some(TxOutputInternal {
        gas_used,
        success,
        logs,
        account_writes,
        storage_writes,
        read_set: rs,
    })
}

/// Validate a transaction's read set against current MvMemory state.
fn validate_read_set(tx_idx: TxIdx, read_set: &[ReadEntry], mv: &MvMemory) -> bool {
    for entry in read_set {
        let current_origin = match &entry.key {
            LocationKey::Account(addr) => mv
                .latest_account_writer(tx_idx, addr)
                .map(ReadOrigin::Tx)
                .unwrap_or(ReadOrigin::Base),
            LocationKey::Storage(addr, slot) => mv
                .latest_storage_writer(tx_idx, addr, slot)
                .map(ReadOrigin::Tx)
                .unwrap_or(ReadOrigin::Base),
        };

        if current_origin != entry.origin {
            return false;
        }
    }
    true
}

/// Build final output from per-tx results.
fn build_output(
    outputs: Vec<Mutex<Option<TxOutputInternal>>>,
) -> Result<ParallelExecutionOutput, ParallelEvmError> {
    let mut results = Vec::with_capacity(outputs.len());
    let mut state_changes: HashMap<Address, Account> = HashMap::new();

    for (tx_idx, output_mutex) in outputs.into_iter().enumerate() {
        let output = output_mutex
            .into_inner()
            .ok_or_else(|| ParallelEvmError::Database(format!("missing output for tx {tx_idx}")))?;

        results.push(TxResult {
            gas_used: output.gas_used,
            success: output.success,
            logs: output.logs,
        });

        // Merge state changes (later tx overwrites earlier for same key).
        for (addr, write) in output.account_writes {
            let account = state_changes
                .entry(addr)
                .or_insert_with(|| Account::new_not_existing(tx_idx));

            match write {
                AccountWrite::Updated(info) => {
                    account.info = info;
                    account.mark_touch();
                }
                AccountWrite::Destroyed => {
                    account.mark_selfdestruct();
                }
            }
        }

        for (addr, slot, value) in output.storage_writes {
            let account = state_changes
                .entry(addr)
                .or_insert_with(|| Account::new_not_existing(tx_idx));
            account.storage.insert(
                slot,
                EvmStorageSlot::new_changed(U256::ZERO, value, tx_idx),
            );
        }
    }

    Ok(ParallelExecutionOutput {
        results,
        state_changes,
    })
}

/// Sequential fallback for small batches or convergence failures.
fn sequential_execute<DB>(
    txs: &[TxEnv],
    base_db: &DB,
    cfg_env: &CfgEnv,
    block_env: &BlockEnv,
) -> Result<ParallelExecutionOutput, ParallelEvmError>
where
    DB: DatabaseRef + Send + Sync,
    DB::Error: fmt::Display + Send,
{
    let mv = MvMemory::new();
    let mut results = Vec::with_capacity(txs.len());
    let mut state_changes: HashMap<Address, Account> = HashMap::new();

    for (tx_idx, tx_env) in txs.iter().enumerate() {
        let output = execute_single_tx(tx_idx, tx_env, base_db, &mv, cfg_env, block_env)
            .ok_or_else(|| ParallelEvmError::Database(format!("execution failed for tx {tx_idx}")))?;

        // Write to MvMemory so subsequent txs see this tx's state.
        for (addr, write) in &output.account_writes {
            mv.write_account(tx_idx, *addr, write);
        }
        for &(addr, slot, value) in &output.storage_writes {
            mv.write_storage(tx_idx, addr, slot, value);
        }

        results.push(TxResult {
            gas_used: output.gas_used,
            success: output.success,
            logs: output.logs,
        });

        for (addr, write) in output.account_writes {
            let account = state_changes
                .entry(addr)
                .or_insert_with(|| Account::new_not_existing(tx_idx));
            match write {
                AccountWrite::Updated(info) => {
                    account.info = info;
                    account.mark_touch();
                }
                AccountWrite::Destroyed => {
                    account.mark_selfdestruct();
                }
            }
        }

        for (addr, slot, value) in output.storage_writes {
            let account = state_changes
                .entry(addr)
                .or_insert_with(|| Account::new_not_existing(tx_idx));
            account.storage.insert(
                slot,
                EvmStorageSlot::new_changed(U256::ZERO, value, tx_idx),
            );
        }
    }

    Ok(ParallelExecutionOutput { results, state_changes })
}
