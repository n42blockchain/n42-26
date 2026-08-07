//! Bench the full node-path twig pipeline: `StateDiff` → `TwigState::apply_diff`
//! (key derivation + value encode + engine apply + root). This is the layer
//! `profile_real` skips (it feeds pre-derived keys straight to `ShardedTwig`),
//! and where per-op blake3 key derivation lives.
//!
//! Run: cargo run --release --example twig_state_bench -p n42-jmt
//! Env: TS_BENCH_ACCOUNTS (default 1_000_000), TS_BENCH_BLOCK (default 25_000).

use alloy_primitives::{Address, U256};
use n42_execution::state_diff::{AccountChangeType, AccountDiff, StateDiff, ValueChange};
use n42_jmt::TwigState;
use std::collections::BTreeMap;
use std::time::Instant;

fn env_usize(k: &str, d: usize) -> usize {
    std::env::var(k)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(d)
}

fn addr_of(i: usize) -> Address {
    let mut b = [0u8; 20];
    b[..8].copy_from_slice(&(i as u64).to_le_bytes());
    b[8..16].copy_from_slice(&((i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)).to_le_bytes());
    Address::from(b)
}

fn block_diff(start: usize, count: usize, with_storage: bool) -> StateDiff {
    let mut accounts = BTreeMap::new();
    for i in start..start + count {
        let mut storage = BTreeMap::new();
        if with_storage && i % 4 == 0 {
            storage.insert(
                U256::from(i),
                ValueChange::new(U256::ZERO, U256::from(i + 1)),
            );
        }
        accounts.insert(
            addr_of(i),
            AccountDiff {
                change_type: AccountChangeType::Created,
                balance: Some(ValueChange::new(
                    U256::ZERO,
                    U256::from(1_000u64 + i as u64),
                )),
                nonce: Some(ValueChange::new(0, 1)),
                code_change: None,
                storage,
            },
        );
    }
    StateDiff { accounts }
}

fn main() {
    let n = env_usize("TS_BENCH_ACCOUNTS", 1_000_000);
    let block = env_usize("TS_BENCH_BLOCK", 25_000).max(1);

    // Pre-build all diffs so the timed loop measures apply_diff only.
    let mut diffs = Vec::new();
    let mut done = 0usize;
    while done < n {
        let cnt = block.min(n - done);
        diffs.push(block_diff(done, cnt, true));
        done += cnt;
    }
    let total_ops: usize = diffs
        .iter()
        .map(|d| {
            d.accounts
                .values()
                .map(|a| 1 + a.storage.len())
                .sum::<usize>()
        })
        .sum();

    let mut t = TwigState::new();
    let start = Instant::now();
    let mut root = Default::default();
    for d in &diffs {
        let (_v, r) = t.apply_diff(d).unwrap();
        root = r;
    }
    let secs = start.elapsed().as_secs_f64();

    println!("=== TwigState (StateDiff node path) ===");
    println!(
        "accounts:  {n} (+{} storage ops) in blocks of {block}",
        total_ops - n
    );
    println!(
        "apply:     {secs:.2}s  ({:.0} ops/s incl key derivation + encode + root)",
        total_ops as f64 / secs
    );
    println!("root:      {root}");
}
