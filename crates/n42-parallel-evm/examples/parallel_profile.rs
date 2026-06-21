//! Focused profile of the parallel execution path's per-tx overhead.
//!
//! After deferred coinbase removed the O(n^2) cascade, the parallel path is
//! near-linear but still ~14 us/tx on trivial transfers (vs ~5 us sequential) —
//! pure scaffolding overhead (per-tx EVM context build, read-set Arc<Mutex>,
//! MvMemory DashMap ops, env-var lookups). This bench isolates that path so a
//! sampler (samply/ETW) attributes the overhead.
//!
//! Build:  cargo build --profile profiling --example parallel_profile -p n42-parallel-evm
//! Sample: samply record --save-only --unstable-presymbolicate -o p.json.gz \
//!           ./target/profiling/examples/parallel_profile.exe
//! Env: PP_TXS (default 5000), PP_ITERS (default 40)

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use alloy_primitives::{Address, TxKind, U256};
use revm::context::{BlockEnv, CfgEnv, TxEnv};
use revm::database::{CacheDB, EmptyDB};
use revm::state::AccountInfo;
use std::time::Instant;

fn addr(n: u64) -> Address {
    Address::from_word(U256::from(n).into())
}

fn main() {
    let n: u64 = std::env::var("PP_TXS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5000);
    let iters: usize = std::env::var("PP_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(40);

    // Independent transfers paying gas to a shared coinbase (deferred-coinbase
    // makes them non-conflicting — pure per-tx overhead, no aborts).
    let mut db = CacheDB::<EmptyDB>::default();
    let mut txs = Vec::with_capacity(n as usize);
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
                .kind(TxKind::Call(addr(0x2_0000 + i)))
                .value(U256::from(1))
                .gas_limit(21_000)
                .gas_price(7)
                .nonce(0)
                .build()
                .unwrap(),
        );
    }
    let cfg = CfgEnv::default();
    let block = BlockEnv {
        beneficiary: addr(0xC01D_B175),
        ..Default::default()
    };
    unsafe { std::env::set_var("N42_PARALLEL_THRESHOLD", "1") };

    // Warm up rayon's pool + caches.
    let _ = n42_parallel_evm::parallel_execute(&txs, &db, cfg.clone(), block.clone());

    let t = Instant::now();
    let mut acc = 0usize;
    for _ in 0..iters {
        let out =
            n42_parallel_evm::parallel_execute(&txs, &db, cfg.clone(), block.clone()).unwrap();
        acc = acc.wrapping_add(out.results.len());
    }
    let secs = t.elapsed().as_secs_f64();
    let total_tx = (n as usize) * iters;
    println!("parallel_execute: {n} txs x {iters} iters = {total_tx} txs in {secs:.2}s");
    println!(
        "  {:.0} txs/s, {:.2} us/tx  (acc={acc})",
        total_tx as f64 / secs,
        secs * 1e6 / total_tx as f64
    );
}
