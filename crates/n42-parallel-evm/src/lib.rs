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
        assert_eq!(
            cb,
            Some(U256::from(200u64 * 21_000 * 7)),
            "coinbase fee total"
        );
        assert_eq!(
            balances(&seq),
            balances(&par),
            "every account balance must match"
        );
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
        assert_eq!(
            balances(&seq),
            balances(&par),
            "hot-recipient block must match sequential"
        );
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
        assert_eq!(
            balances(&seq),
            balances(&par),
            "contract-beneficiary block must match sequential"
        );
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
        assert_eq!(
            balances(&seq),
            balances(&par),
            "must match sequential exactly"
        );
    }
}

/// Differential regression: randomized mixed-workload blocks run through the
/// parallel engine must produce **byte-for-byte the same post-state** as the
/// sequential reference — not just balances, but every account's nonce,
/// code_hash, and storage slot, plus per-tx gas/success. Each block is run
/// through the parallel path several times to catch nondeterministic races
/// (the class of bug that made `hot_recipient` flaky before the in-order
/// validation + value-based read-set fix; see `docs/devlog-67`/`-69`).
#[cfg(test)]
mod differential_tests {
    use super::*;
    use alloy_primitives::{B256, Bytes, TxKind, U256};
    use revm::database::{CacheDB, EmptyDB};
    use revm::state::{AccountInfo, Bytecode};
    use std::collections::BTreeMap;

    /// Tiny deterministic xorshift64 PRNG — reproducible blocks, no `rand` dep.
    struct Rng(u64);
    impl Rng {
        fn new(seed: u64) -> Self {
            Self(seed ^ 0x9E37_79B9_7F4A_7C15)
        }
        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        fn below(&mut self, n: u64) -> u64 {
            if n == 0 { 0 } else { self.next_u64() % n }
        }
    }

    fn addr(n: u64) -> Address {
        Address::from_word(U256::from(n).into())
    }

    /// Minimal "counter" contract: `slot[0] += 1` then STOP. Every CALL to it
    /// SLOADs slot 0, adds 1, SSTOREs — so N calls must serialize and leave the
    /// slot at N. This is the heavy read-write storage-conflict probe for
    /// Block-STM (forces abort/re-execute cascades).
    fn counter_code() -> Bytecode {
        // 60 00  PUSH1 0   | 54 SLOAD | 60 01 PUSH1 1 | 01 ADD
        // 60 00  PUSH1 0   | 55 SSTORE | 00 STOP
        Bytecode::new_raw(Bytes::from_static(&[
            0x60, 0x00, 0x54, 0x60, 0x01, 0x01, 0x60, 0x00, 0x55, 0x00,
        ]))
    }

    /// Full post-state fingerprint of one account: balance, nonce, code_hash,
    /// and every changed storage slot (present value).
    #[derive(PartialEq, Eq, Debug)]
    struct AcctFp {
        balance: U256,
        nonce: u64,
        code_hash: B256,
        storage: BTreeMap<U256, U256>,
    }

    /// Canonical, order-independent fingerprint of the whole block output.
    fn fingerprint(out: &ParallelExecutionOutput) -> BTreeMap<Address, AcctFp> {
        out.state_changes
            .iter()
            .map(|(a, acc)| {
                let storage = acc
                    .storage
                    .iter()
                    .map(|(k, slot)| (*k, slot.present_value))
                    .collect();
                (
                    *a,
                    AcctFp {
                        balance: acc.info.balance,
                        nonce: acc.info.nonce,
                        code_hash: acc.info.code_hash,
                        storage,
                    },
                )
            })
            .collect()
    }

    fn results_fp(out: &ParallelExecutionOutput) -> Vec<(u64, bool)> {
        out.results
            .iter()
            .map(|r| (r.gas_used, r.success))
            .collect()
    }

    /// Build a randomized mixed block: `n` txs, each from a unique sender (nonce
    /// 0, well funded), randomly either (a) CALLing one of `k` shared counter
    /// contracts (storage conflict) or (b) transferring a random value to one of
    /// a few shared "hot" recipients or a unique cold recipient (balance
    /// conflict). Beneficiary is a clean EOA, basefee 0 ⇒ all gas accrues to it
    /// (exercises deferred coinbase under mixed load).
    fn mixed_block(seed: u64, n: u64, k: u64) -> (CacheDB<EmptyDB>, Vec<TxEnv>, BlockEnv) {
        let mut rng = Rng::new(seed);
        let mut db = CacheDB::<EmptyDB>::default();

        // Counter contracts.
        let code = counter_code();
        let code_hash = code.hash_slow();
        for c in 0..k {
            db.insert_account_info(
                addr(0xC000_0000 + c),
                AccountInfo {
                    balance: U256::ZERO,
                    nonce: 1,
                    code_hash,
                    code: Some(code.clone()),
                    ..Default::default()
                },
            );
        }
        // A handful of hot recipient EOAs.
        let hot_count = 3u64;
        for h in 0..hot_count {
            db.insert_account_info(
                addr(0x8000_0000 + h),
                AccountInfo {
                    balance: U256::from(1u64),
                    nonce: 0,
                    ..Default::default()
                },
            );
        }

        let mut txs = Vec::with_capacity(n as usize);
        for i in 0..n {
            let sender = addr(0x10_0000 + i);
            db.insert_account_info(
                sender,
                AccountInfo {
                    balance: U256::from(u128::MAX),
                    nonce: 0,
                    ..Default::default()
                },
            );

            let (target, value, gas) = if rng.below(2) == 0 && k > 0 {
                // CALL a shared counter contract (storage conflict).
                (addr(0xC000_0000 + rng.below(k)), U256::ZERO, 100_000)
            } else if rng.below(3) != 0 {
                // Transfer to a shared hot recipient (balance conflict).
                (
                    addr(0x8000_0000 + rng.below(hot_count)),
                    U256::from(rng.below(1_000) + 1),
                    21_000,
                )
            } else {
                // Transfer to a unique cold recipient.
                (
                    addr(0x4000_0000 + i),
                    U256::from(rng.below(1_000) + 1),
                    21_000,
                )
            };

            txs.push(
                TxEnv::builder()
                    .caller(sender)
                    .kind(TxKind::Call(target))
                    .value(value)
                    .gas_limit(gas)
                    .gas_price(7) // basefee 0 ⇒ all 7/gas to the beneficiary
                    .nonce(0)
                    .build()
                    .unwrap(),
            );
        }

        let block = BlockEnv {
            beneficiary: addr(0xBE0E_F1C1),
            ..Default::default()
        };
        (db, txs, block)
    }

    /// Run one randomized block through sequential (reference) and parallel
    /// (4 repeats), asserting full post-state + per-tx results match every time.
    fn assert_block_matches(seed: u64, n: u64, k: u64) {
        let (db, txs, block) = mixed_block(seed, n, k);
        let cfg = CfgEnv::default();

        let seq = sequential_execute(&txs, &db, &cfg, &block).unwrap();
        let seq_fp = fingerprint(&seq);
        let seq_res = results_fp(&seq);

        unsafe { std::env::set_var("N42_PARALLEL_THRESHOLD", "1") };
        for run in 0..4 {
            let par = parallel_execute(&txs, &db, cfg.clone(), block.clone()).unwrap();
            assert_eq!(
                fingerprint(&par),
                seq_fp,
                "seed={seed} n={n} k={k} run={run}: post-state diverged from sequential"
            );
            assert_eq!(
                results_fp(&par),
                seq_res,
                "seed={seed} n={n} k={k} run={run}: per-tx gas/success diverged"
            );
        }
        unsafe { std::env::remove_var("N42_PARALLEL_THRESHOLD") };
    }

    #[test]
    fn differential_mixed_workloads_match_sequential() {
        // Varied conflict densities: many seeds, varying tx counts and contract
        // counts (fewer contracts ⇒ hotter storage ⇒ more aborts).
        for seed in 0..8u64 {
            assert_block_matches(seed, 120, 4);
        }
    }

    #[test]
    fn differential_high_storage_contention() {
        // A single shared counter ⇒ every contract call serializes on slot 0.
        // The final counter must equal the number of successful calls, identical
        // in parallel and sequential.
        assert_block_matches(0xABCD, 150, 1);
        assert_block_matches(0x1234, 150, 1);
    }

    #[test]
    fn differential_storage_conflict_counter_is_exact() {
        // Pin down the counter semantics: K=1, all-CALL block (no transfers) must
        // leave slot 0 == number of txs, matching sequential exactly.
        let n = 100u64;
        let mut db = CacheDB::<EmptyDB>::default();
        let code = counter_code();
        let code_hash = code.hash_slow();
        let contract = addr(0xC000_0000);
        db.insert_account_info(
            contract,
            AccountInfo {
                balance: U256::ZERO,
                nonce: 1,
                code_hash,
                code: Some(code),
                ..Default::default()
            },
        );
        let mut txs = Vec::new();
        for i in 0..n {
            let sender = addr(0x10_0000 + i);
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
                    .kind(TxKind::Call(contract))
                    .value(U256::ZERO)
                    .gas_limit(100_000)
                    .gas_price(7)
                    .nonce(0)
                    .build()
                    .unwrap(),
            );
        }
        let cfg = CfgEnv::default();
        let block = BlockEnv {
            beneficiary: addr(0xBE0E_F1C1),
            ..Default::default()
        };
        let seq = sequential_execute(&txs, &db, &cfg, &block).unwrap();
        unsafe { std::env::set_var("N42_PARALLEL_THRESHOLD", "1") };
        let par = parallel_execute(&txs, &db, cfg.clone(), block.clone()).unwrap();
        unsafe { std::env::remove_var("N42_PARALLEL_THRESHOLD") };

        let counter = par
            .state_changes
            .get(&contract)
            .and_then(|a| a.storage.get(&U256::ZERO))
            .map(|s| s.present_value);
        assert_eq!(counter, Some(U256::from(n)), "counter must equal tx count");
        assert_eq!(
            fingerprint(&seq),
            fingerprint(&par),
            "full state must match"
        );
        assert_eq!(
            results_fp(&seq),
            results_fp(&par),
            "per-tx results must match"
        );
    }
}
