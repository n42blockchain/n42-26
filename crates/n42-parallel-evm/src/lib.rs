//! Block-STM parallel EVM execution engine for N42.
//!
//! Optimistic parallel transaction execution inspired by Grevm 2.1 and Aptos
//! Block-STM. Transactions execute against an MVCC store ([`mv_memory`]); the
//! [`scheduler`] coordinates execute/validate tasks across rayon workers
//! ([`worker`]); conflicts are detected ([`execution::validate_read_set`]) and
//! re-executed. The block beneficiary's gas fees are credited commutatively
//! ([`coinbase`]) so independent txs do not cascade-abort on the shared coinbase.
//!
//! Module layout:
//! - [`mv_memory`] / [`parallel_db`] — MVCC store + per-tx database view;
//! - [`scheduler`] — Block-STM task coordination;
//! - [`execution`] — run one tx, validate one read set;
//! - [`worker`] — the rayon worker loop;
//! - [`output`] — per-tx output + final block-state assembly;
//! - [`coinbase`] — deferred-coinbase decision / delta / materialization;
//! - this root — `parallel_execute` orchestration + `sequential_execute` fallback.

mod coinbase;
mod execution;
pub mod mv_memory;
mod output;
pub mod parallel_db;
pub mod scheduler;
pub mod types;
mod worker;

use alloy_primitives::{Address, Log};
use coinbase::{CoinbasePlan, DeferredCoinbase};
use execution::execute_single_tx;
use mv_memory::MvMemory;
use output::{TxOutputInternal, build_output, merge_tx_state};
use parking_lot::Mutex;
use revm::context::{BlockEnv, CfgEnv, TxEnv};
use revm::database_interface::DatabaseRef;
use revm::state::Account;
use scheduler::Scheduler;
use std::collections::HashMap;
use std::fmt;
use tracing::{info, warn};
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

/// Worker count for the parallel path. Block-STM's in-order validation is partly
/// serial, so idle workers spin/contend on the shared scheduler atoms; past a
/// point adding threads makes cheap-tx blocks slower on a contended allocator
/// (the Windows default heap; jemalloc on production Linux scales far better).
/// Cap to a sane default; raise `N42_PARALLEL_WORKERS` on contract-heavy chains.
/// See `docs/devlog-67`.
fn worker_count(num_txs: usize) -> usize {
    let cores = rayon::current_num_threads();
    let cap = std::env::var("N42_PARALLEL_WORKERS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&w| w > 0)
        .unwrap_or_else(|| cores.min(8));
    cap.min(cores).min(num_txs).max(1)
}

/// Execute transactions in parallel using Block-STM.
///
/// Falls back to sequential for small batches (configurable via
/// `N42_PARALLEL_THRESHOLD`), for non-deferrable beneficiaries (see [`coinbase`]),
/// and on convergence failure.
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

    // Decide how to handle the block beneficiary; a non-deferrable one (tx sender
    // or contract) is unsound in parallel, so run the whole block sequentially.
    let coinbase = match DeferredCoinbase::plan(txs, base_db, &block_env)? {
        CoinbasePlan::Sequential => return sequential_execute(txs, base_db, &cfg_env, &block_env),
        CoinbasePlan::NoDefer => None,
        CoinbasePlan::Defer(dc) => Some(dc),
    };

    let num_workers = worker_count(num_txs);
    let start = std::time::Instant::now();
    let mv = MvMemory::new();
    let scheduler = Scheduler::new(num_txs);
    let outputs: Vec<Mutex<Option<TxOutputInternal>>> =
        (0..num_txs).map(|_| Mutex::new(None)).collect();

    let max_rounds = 10;
    let mut round = 0;
    while !scheduler.all_done() && round < max_rounds {
        round += 1;
        rayon::scope(|s| {
            for _ in 0..num_workers {
                s.spawn(|_| {
                    worker::worker_loop(
                        &scheduler,
                        txs,
                        base_db,
                        &mv,
                        &cfg_env,
                        &block_env,
                        &outputs,
                        coinbase.as_ref(),
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
    if let Some(cb) = &coinbase {
        cb.materialize(mv.sum_bene_deltas(), &mut output);
    }
    Ok(output)
}

/// Sequential fallback for small batches, non-deferrable beneficiaries, or
/// convergence failures. Credits the beneficiary normally in tx order (no
/// deferral), giving the canonical reference result.
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
        let output = execute_single_tx(tx_idx, tx_env, base_db, &mv, cfg_env, block_env, None)
            .ok_or_else(|| {
                ParallelEvmError::Database(format!("execution failed for tx {tx_idx}"))
            })?;

        // Publish to MvMemory so subsequent txs see this tx's state.
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
    use alloy_primitives::{TxKind, U256};
    use revm::database::{CacheDB, EmptyDB};
    use revm::state::AccountInfo;

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
        // post-hoc `new - base` delta would saturate to 0. The plan returns
        // `Sequential`, so the result equals sequential.
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
        // Beneficiary is also tx 0's sender (nonce-changing ⇒ non-commutative): the
        // plan returns `Sequential` so the whole block falls back to sequential, and
        // every account (incl. the beneficiary) matches exactly.
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
        assert_eq!(balances(&seq), balances(&par), "must match sequential exactly");
    }
}
