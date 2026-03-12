use criterion::{black_box, criterion_group, criterion_main, Criterion, BenchmarkId};
use alloy_primitives::{Address, B256, U256};
use n42_execution::{StateDiff, AccountDiff, AccountChangeType, ValueChange};
use n42_jmt::ShardedJmt;
use std::collections::BTreeMap;

/// Build a synthetic StateDiff with `n` account changes, each with `slots_per_account` storage changes.
fn make_diff(n: usize, slots_per_account: usize) -> StateDiff {
    let mut accounts = BTreeMap::new();
    for i in 0..n {
        let addr = Address::with_last_byte((i % 256) as u8);
        let mut storage = BTreeMap::new();
        for s in 0..slots_per_account {
            storage.insert(
                U256::from(s),
                ValueChange::new(U256::from(s), U256::from(s + i + 1)),
            );
        }
        accounts.insert(addr, AccountDiff {
            change_type: if i % 10 == 0 { AccountChangeType::Created } else { AccountChangeType::Modified },
            balance: Some(ValueChange::new(U256::from(1000u64), U256::from(1000u64 + i as u64))),
            nonce: Some(ValueChange::new(i as u64, i as u64 + 1)),
            code_change: None,
            storage,
        });
    }
    StateDiff { accounts }
}

fn bench_apply_diff(c: &mut Criterion) {
    let mut group = c.benchmark_group("apply_diff");

    for &account_count in &[100, 1_000, 10_000, 50_000] {
        let diff = make_diff(account_count, 0);
        group.bench_with_input(
            BenchmarkId::new("accounts_only", account_count),
            &diff,
            |b, diff| {
                b.iter(|| {
                    let mut jmt = ShardedJmt::new();
                    black_box(jmt.apply_diff(diff).unwrap());
                });
            },
        );
    }

    // With storage slots
    for &(accounts, slots) in &[(1_000, 5), (10_000, 2), (5_000, 10)] {
        let diff = make_diff(accounts, slots);
        group.bench_with_input(
            BenchmarkId::new(format!("{}accts_{}slots", accounts, slots), accounts * (1 + slots)),
            &diff,
            |b, diff| {
                b.iter(|| {
                    let mut jmt = ShardedJmt::new();
                    black_box(jmt.apply_diff(diff).unwrap());
                });
            },
        );
    }

    group.finish();
}

fn bench_root_hash(c: &mut Criterion) {
    let mut group = c.benchmark_group("root_hash");

    for &n in &[1_000, 10_000] {
        let diff = make_diff(n, 0);
        let mut jmt = ShardedJmt::new();
        jmt.apply_diff(&diff).unwrap();

        group.bench_with_input(BenchmarkId::from_parameter(n), &jmt, |b, jmt| {
            b.iter(|| {
                black_box(jmt.root_hash().unwrap());
            });
        });
    }

    group.finish();
}

fn bench_proof_generation(c: &mut Criterion) {
    let mut group = c.benchmark_group("proof_generation");

    let diff = make_diff(10_000, 0);
    let mut jmt = ShardedJmt::new();
    jmt.apply_diff(&diff).unwrap();

    let key = n42_jmt::account_key(&Address::with_last_byte(42));
    group.bench_function("account_proof_10k", |b| {
        b.iter(|| {
            black_box(jmt.get_proof(key).unwrap());
        });
    });

    let storage_key = n42_jmt::storage_key(&Address::with_last_byte(1), &U256::from(0));
    group.bench_function("storage_proof_10k", |b| {
        b.iter(|| {
            black_box(jmt.get_proof(storage_key).unwrap());
        });
    });

    group.finish();
}

fn bench_proof_verify(c: &mut Criterion) {
    let diff = make_diff(1_000, 2);
    let mut jmt = ShardedJmt::new();
    jmt.apply_diff(&diff).unwrap();

    let key = n42_jmt::account_key(&Address::with_last_byte(1));
    let proof = n42_jmt::proof::build_proof(&jmt, key).unwrap().unwrap();
    let root = jmt.root_hash().unwrap();

    c.bench_function("proof_verify", |b| {
        b.iter(|| {
            black_box(proof.verify(&root).unwrap());
        });
    });
}

fn bench_incremental_updates(c: &mut Criterion) {
    let mut group = c.benchmark_group("incremental_update");

    // Simulate a chain of blocks, each modifying a subset of accounts.
    let mut jmt = ShardedJmt::new();
    // Warm up with 1000 accounts
    let warmup = make_diff(1_000, 0);
    jmt.apply_diff(&warmup).unwrap();

    for &block_size in &[100, 500, 1_000] {
        let diff = make_diff(block_size, 1);
        group.bench_with_input(
            BenchmarkId::new("block_update", block_size),
            &diff,
            |b, diff| {
                b.iter(|| {
                    let mut jmt_clone = ShardedJmt::new();
                    // Apply warmup first
                    jmt_clone.apply_diff(&warmup).unwrap();
                    black_box(jmt_clone.apply_diff(diff).unwrap());
                });
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_apply_diff,
    bench_root_hash,
    bench_proof_generation,
    bench_proof_verify,
    bench_incremental_updates,
);
criterion_main!(benches);
