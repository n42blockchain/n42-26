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
use revm::context::Journal;
use revm::context::{BlockEnv, CfgEnv, Context, TxEnv};
use revm::context_interface::result::ResultAndState;
use revm::database_interface::DatabaseRef;
use revm::handler::MainBuilder;
use revm::state::{Account, AccountInfo, EvmStorageSlot, TransactionId};
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
#[tracing::instrument(
    target = "n42.el.parallel_execute",
    name = "parallel_execute",
    skip_all,
    fields(tx_count = txs.len(), parallelism = tracing::field::Empty)
)]
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
    tracing::Span::current().record("parallelism", rayon::current_num_threads());
    let num_txs = txs.len();
    if num_txs == 0 {
        return Ok(ParallelExecutionOutput {
            results: vec![],
            state_changes: HashMap::new(),
        });
    }
    // Reject impossibly large blocks up front so the hot loop can construct
    // TransactionId without falling back to a panic. NonMaxU32 forbids u32::MAX
    // itself, so the largest legal tx_idx is u32::MAX - 1.
    if num_txs >= u32::MAX as usize {
        return Err(ParallelEvmError::BlockTooLarge { tx_count: num_txs });
    }

    // Read per call (once per block, negligible) so tests/benches can vary it;
    // a OnceLock here silently froze the first value for the process lifetime.
    let parallel_threshold: usize = std::env::var("N42_PARALLEL_THRESHOLD")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8);

    if num_txs <= parallel_threshold {
        return sequential_execute(txs, base_db, &cfg_env, &block_env);
    }

    // Deferred coinbase (devlog-67): credit the block beneficiary's gas fees as a
    // commutative accumulator instead of a versioned write, so independent txs no
    // longer cascade-abort on the shared coinbase. Sound only while the
    // beneficiary's balance change is a pure commutative increment:
    //   - exclude when it is a tx sender (nonce changes — not commutative);
    //   - exclude when it is a CONTRACT (non-empty code): a tx could CALL it and
    //     DECREASE its balance with nonce/code unchanged, which the post-hoc
    //     `new - base` delta would saturate to 0 and corrupt the final balance.
    // A validator reward address is an EOA, so deferral stays on in practice.
    // Toggle with N42_DEFERRED_COINBASE=0.
    let enabled = std::env::var("N42_DEFERRED_COINBASE")
        .map(|v| v != "0")
        .unwrap_or(true);
    let bene = block_env.beneficiary;
    let bene_base: Option<AccountInfo> = if enabled {
        base_db
            .basic_ref(bene)
            .map_err(|e| ParallelEvmError::Database(e.to_string()))?
    } else {
        None
    };
    let bene_is_sender = txs.iter().any(|t| t.caller == bene);
    // None base (account absent) ⇒ empty code ⇒ EOA.
    let bene_is_eoa = bene_base.as_ref().is_none_or(|i| i.is_empty_code_hash());

    // When the default deferral is on but the beneficiary is NOT a deferrable EOA
    // (it is a tx sender, or a contract that a tx could CALL), neither path is
    // sound in parallel: deferral's commutative delta breaks (nonce change /
    // balance decrease), and the non-deferred path lets every tx's fee credit turn
    // the beneficiary into a hot account whose optimistic ordering does not exactly
    // reproduce sequential crediting. These cases are rare (a validator reward
    // address is an EOA non-sender), so execute the whole block sequentially for
    // guaranteed correctness. (`N42_DEFERRED_COINBASE=0` is an explicit debug knob
    // and keeps the non-deferred parallel path, accepting its limitations.)
    if enabled && (bene_is_sender || !bene_is_eoa) {
        return sequential_execute(txs, base_db, &cfg_env, &block_env);
    }
    let deferred_bene: Option<Address> = enabled.then_some(bene);
    let bene_base: Option<AccountInfo> = deferred_bene.and(bene_base);

    // Worker count. Block-STM's in-order validation is partly serial, so idle
    // workers spin/contend on the shared scheduler atoms; past a point adding
    // threads makes cheap-tx blocks SLOWER (measured: 32 workers ~4x slower than
    // 4 on plain transfers). Cap to a sane default; raise N42_PARALLEL_WORKERS on
    // contract-heavy chains where per-tx work amortizes the coordination cost.
    // See `docs/devlog-67`.
    let num_workers = {
        let cores = rayon::current_num_threads();
        let cap = std::env::var("N42_PARALLEL_WORKERS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&w| w > 0)
            .unwrap_or_else(|| cores.min(8));
        cap.min(cores).min(num_txs).max(1)
    };

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
            for _ in 0..num_workers {
                s.spawn(|_| {
                    worker_loop(
                        &scheduler,
                        txs,
                        base_db,
                        &mv,
                        &cfg_env,
                        &block_env,
                        &outputs,
                        deferred_bene,
                        bene_base.as_ref(),
                    );
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

    let mut output = build_output(outputs)?;
    // Materialize the deferred coinbase: final balance = base + Σ per-tx deltas.
    // Order-independent (addition commutes); applied once here at commit.
    if let Some(bene) = deferred_bene {
        let sum = mv.sum_bene_deltas();
        if !sum.is_zero() {
            let mut info = bene_base.clone().unwrap_or_default();
            info.balance = info.balance.saturating_add(sum);
            let tx_id = TransactionId::new(0).expect("0 is a valid TransactionId");
            let account = output
                .state_changes
                .entry(bene)
                .or_insert_with(|| Account::new_not_existing(tx_id));
            account.info = info;
            account.mark_touch();
        }
    }
    Ok(output)
}

/// Internal per-tx output (before merging).
struct TxOutputInternal {
    gas_used: u64,
    success: bool,
    logs: Vec<Log>,
    account_writes: Vec<(Address, AccountWrite)>,
    storage_writes: Vec<(Address, U256, U256)>,
    read_set: Vec<ReadEntry>,
    /// Commutative balance delta this tx credited to the deferred beneficiary
    /// (gas fee + value sent to it), if deferral is active. Applied at commit.
    bene_delta: Option<U256>,
}

/// Worker loop: repeatedly fetches and executes tasks from the scheduler.
#[allow(clippy::too_many_arguments)]
fn worker_loop<DB>(
    scheduler: &Scheduler,
    txs: &[TxEnv],
    base_db: &DB,
    mv: &MvMemory,
    cfg_env: &CfgEnv,
    block_env: &BlockEnv,
    outputs: &[Mutex<Option<TxOutputInternal>>],
    deferred_bene: Option<Address>,
    bene_base: Option<&AccountInfo>,
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

                let output = execute_single_tx(
                    tx_idx,
                    &txs[tx_idx],
                    base_db,
                    mv,
                    cfg_env,
                    block_env,
                    deferred_bene,
                    bene_base,
                );

                if let Some(ref out) = output {
                    for (addr, write) in &out.account_writes {
                        mv.write_account(tx_idx, *addr, write);
                    }
                    for &(addr, slot, value) in &out.storage_writes {
                        mv.write_storage(tx_idx, addr, slot, value);
                    }
                    if let Some(delta) = out.bene_delta {
                        mv.record_bene_delta(tx_idx, delta);
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

/// Extract the read set from its Arc wrapper, avoiding a clone when possible.
fn unwrap_read_set(read_set: SharedReadSet) -> Vec<ReadEntry> {
    match Arc::try_unwrap(read_set) {
        Ok(mutex) => mutex.into_inner(),
        Err(arc) => arc.lock().clone(),
    }
}

/// Execute a single transaction using revm with the parallel database adapter.
#[allow(clippy::too_many_arguments)]
fn execute_single_tx<DB>(
    tx_idx: TxIdx,
    tx_env: &TxEnv,
    base_db: &DB,
    mv: &MvMemory,
    cfg_env: &CfgEnv,
    block_env: &BlockEnv,
    deferred_bene: Option<Address>,
    bene_base: Option<&AccountInfo>,
) -> Option<TxOutputInternal>
where
    DB: DatabaseRef + Send + Sync,
    DB::Error: fmt::Display + Send,
{
    let read_set: SharedReadSet = Arc::new(parking_lot::Mutex::new(Vec::new()));
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
        if account.is_touched() {
            // Deferred beneficiary: if it was only balance-credited (nonce + code
            // unchanged from base — the universal fee/transfer case), record the
            // commutative delta instead of a versioned account write so it never
            // conflicts. Any nonce/code change means it was a genuine participant
            // (e.g. a contract coinbase whose call mutated it) — fall back to a
            // normal write for correctness. Its storage writes (if any) stay
            // versioned regardless.
            if deferred_bene == Some(*addr) {
                let base = bene_base.cloned().unwrap_or_default();
                let only_balance =
                    account.info.nonce == base.nonce && account.info.code_hash == base.code_hash;
                if only_balance {
                    bene_delta = Some(account.info.balance.saturating_sub(base.balance));
                } else {
                    account_writes.push((*addr, AccountWrite::Updated(account.info.clone())));
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
    }

    let rs = unwrap_read_set(read_set);

    Some(TxOutputInternal {
        gas_used,
        success,
        logs,
        account_writes,
        storage_writes,
        read_set: rs,
        bene_delta,
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

/// Merge a single transaction's writes into the accumulated state.
fn merge_tx_state(
    state_changes: &mut HashMap<Address, Account>,
    tx_idx: TxIdx,
    account_writes: Vec<(Address, AccountWrite)>,
    storage_writes: Vec<(Address, U256, U256)>,
) {
    // revm 40 tracks the originating transaction via a TransactionId (NonMaxU32).
    // The parallel_execute entry rejects blocks with >= u32::MAX txs, so tx_idx
    // is always a valid NonMaxU32 here.
    let tx_id = TransactionId::new(tx_idx)
        .expect("tx_idx < u32::MAX guaranteed by parallel_execute entry check");
    for (addr, write) in account_writes {
        let account = state_changes
            .entry(addr)
            .or_insert_with(|| Account::new_not_existing(tx_id));
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
    for (addr, slot, value) in storage_writes {
        let account = state_changes
            .entry(addr)
            .or_insert_with(|| Account::new_not_existing(tx_id));
        account
            .storage
            .insert(slot, EvmStorageSlot::new_changed(U256::ZERO, value, tx_id));
    }
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

        merge_tx_state(
            &mut state_changes,
            tx_idx,
            output.account_writes,
            output.storage_writes,
        );
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
        // Sequential path credits the beneficiary normally (deferral is a
        // parallel-only optimization); pass None so the coinbase write flows
        // through the standard account-write path in tx order.
        let output =
            execute_single_tx(tx_idx, tx_env, base_db, &mv, cfg_env, block_env, None, None)
                .ok_or_else(|| {
                    ParallelEvmError::Database(format!("execution failed for tx {tx_idx}"))
                })?;

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

        merge_tx_state(
            &mut state_changes,
            tx_idx,
            output.account_writes,
            output.storage_writes,
        );
    }

    Ok(ParallelExecutionOutput {
        results,
        state_changes,
    })
}

#[cfg(test)]
mod deferred_coinbase_tests {
    use super::*;
    use alloy_primitives::TxKind;
    use revm::database::{CacheDB, EmptyDB};

    fn addr(n: u64) -> Address {
        Address::from_word(U256::from(n).into())
    }

    /// N independent transfers (unique sender→unique receiver) paying gas to a
    /// shared beneficiary — the exact pattern that used to cascade.
    fn workload(n: u64) -> (CacheDB<EmptyDB>, Vec<TxEnv>) {
        let mut db = CacheDB::<EmptyDB>::default();
        let mut txs = Vec::new();
        for i in 0..n {
            let sender = addr(0x1000 + i);
            db.insert_account_info(
                sender,
                AccountInfo {
                    balance: U256::from(u128::MAX),
                    nonce: 0,
                    ..Default::default()
                },
            );
            txs.push(
                TxEnv::builder()
                    .caller(sender)
                    .kind(TxKind::Call(addr(0x2000 + i)))
                    .value(U256::from(1))
                    .gas_limit(21_000)
                    .gas_price(7) // basefee=0 ⇒ all 7/gas goes to the coinbase
                    .nonce(0)
                    .build()
                    .unwrap(),
            );
        }
        (db, txs)
    }

    fn balances(out: &ParallelExecutionOutput) -> std::collections::BTreeMap<Address, U256> {
        out.state_changes
            .iter()
            .map(|(a, acc)| (*a, acc.info.balance))
            .collect()
    }

    #[test]
    fn deferred_parallel_matches_sequential_state() {
        let bene = addr(0xC01D_B175);
        let (db, txs) = workload(200);
        let cfg = CfgEnv::default();
        let block = BlockEnv {
            beneficiary: bene,
            ..Default::default()
        };

        // Ground truth: sequential (normal in-order coinbase credit).
        let seq = sequential_execute(&txs, &db, &cfg, &block).unwrap();

        // Deferred parallel (threshold=1 forces the parallel path).
        unsafe { std::env::set_var("N42_PARALLEL_THRESHOLD", "1") };
        let par = parallel_execute(&txs, &db, cfg.clone(), block.clone()).unwrap();
        unsafe { std::env::remove_var("N42_PARALLEL_THRESHOLD") };

        // Coinbase actually accrued fees, and matches sequential exactly.
        let cb = seq.state_changes.get(&bene).map(|a| a.info.balance);
        assert_eq!(cb, Some(U256::from(200u64 * 21_000 * 7)), "coinbase fee total");
        assert_eq!(balances(&seq), balances(&par), "every account balance must match");
    }

    #[test]
    fn hot_recipient_matches_sequential() {
        // Every tx sends value to the SAME recipient (a hot account written by all
        // txs via NORMAL tx writes, not the coinbase reward). This probes whether
        // Block-STM handles a generic hot account correctly (vs the coinbase, which
        // is special). Beneficiary is a distinct EOA so deferral handles the fees.
        let bene = addr(0xB0B0_B175);
        let hot = addr(0x9999_9999);
        let mut db = CacheDB::<EmptyDB>::default();
        let mut txs = Vec::new();
        for i in 0..150u64 {
            let sender = addr(0x1000 + i);
            db.insert_account_info(
                sender,
                AccountInfo {
                    balance: U256::from(u128::MAX),
                    nonce: 0,
                    ..Default::default()
                },
            );
            txs.push(
                TxEnv::builder()
                    .caller(sender)
                    .kind(TxKind::Call(hot)) // all send to the same recipient
                    .value(U256::from(100))
                    .gas_limit(21_000)
                    .gas_price(7)
                    .nonce(0)
                    .build()
                    .unwrap(),
            );
        }
        let cfg = CfgEnv::default();
        let block = BlockEnv {
            beneficiary: bene,
            ..Default::default()
        };
        let seq = sequential_execute(&txs, &db, &cfg, &block).unwrap();
        unsafe { std::env::set_var("N42_PARALLEL_THRESHOLD", "1") };
        let par = parallel_execute(&txs, &db, cfg.clone(), block.clone()).unwrap();
        unsafe { std::env::remove_var("N42_PARALLEL_THRESHOLD") };
        // The hot recipient accrues 150*100 regardless of order (commutative adds),
        // but Block-STM must still reproduce it exactly.
        assert_eq!(
            par.state_changes.get(&hot).map(|a| a.info.balance),
            Some(U256::from(150u64 * 100)),
            "hot recipient balance"
        );
        assert_eq!(balances(&seq), balances(&par), "hot-recipient block must match sequential");
    }

    #[test]
    fn contract_beneficiary_disables_deferral() {
        // A contract beneficiary (non-empty code) must NOT be deferred: a tx could
        // CALL it and reduce its balance with nonce/code unchanged, which the
        // post-hoc `new - base` delta would saturate to 0. With the guard, deferral
        // is off and the result equals sequential. Here we just confirm the engine
        // runs correctly + matches sequential when the beneficiary has code.
        let bene = addr(0xC0DE_B175);
        let (mut db, txs) = workload(80);
        // Give the beneficiary contract code (STOP) + a starting balance.
        db.insert_account_info(
            bene,
            AccountInfo {
                balance: U256::from(1_000_000u64),
                nonce: 1,
                code_hash: alloy_primitives::keccak256([0x00u8]),
                code: Some(revm::state::Bytecode::new_raw(vec![0x00].into())),
                ..Default::default()
            },
        );
        let cfg = CfgEnv::default();
        let block = BlockEnv {
            beneficiary: bene,
            ..Default::default()
        };
        let seq = sequential_execute(&txs, &db, &cfg, &block).unwrap();
        unsafe { std::env::set_var("N42_PARALLEL_THRESHOLD", "1") };
        let par = parallel_execute(&txs, &db, cfg.clone(), block.clone()).unwrap();
        unsafe { std::env::remove_var("N42_PARALLEL_THRESHOLD") };
        assert_eq!(balances(&seq), balances(&par), "contract-beneficiary block must match sequential");
    }

    #[test]
    fn beneficiary_as_sender_disables_deferral() {
        // Beneficiary is also tx 0's sender (nonce-changing ⇒ non-commutative):
        // the `bene_is_sender` guard auto-disables deferral, so the coinbase flows
        // through the normal versioned path. We assert the NON-beneficiary accounts
        // match sequential (the guard disturbs nothing); the coinbase-that-is-also-
        // a-sender hot account is the pre-existing Block-STM path, out of scope for
        // the deferred-coinbase feature.
        let bene = addr(0x1000); // == sender of tx 0
        let (db, txs) = workload(50);
        let cfg = CfgEnv::default();
        let block = BlockEnv {
            beneficiary: bene,
            ..Default::default()
        };
        let seq = sequential_execute(&txs, &db, &cfg, &block).unwrap();
        unsafe { std::env::set_var("N42_PARALLEL_THRESHOLD", "1") };
        let par = parallel_execute(&txs, &db, cfg.clone(), block.clone()).unwrap();
        unsafe { std::env::remove_var("N42_PARALLEL_THRESHOLD") };
        // Beneficiary-as-sender is non-deferrable ⇒ whole block falls back to
        // sequential ⇒ every account (incl. the beneficiary) matches exactly.
        assert_eq!(balances(&seq), balances(&par), "must match sequential exactly");
    }
}
