use alloy_primitives::{Address, U256};
use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use n42_execution::state_diff::{AccountChangeType, AccountDiff, StateDiff, ValueChange};
use n42_jmt::{N42JmtTree, PersistentJmt, Sbmt, ShardedJmt, ShardedSbmt, account_key};
use std::collections::BTreeMap;

/// Distinct 20-byte address for index `i` (avoids the 256-collision of
/// `Address::with_last_byte`).
fn addr_of(i: usize) -> Address {
    let mut b = [0u8; 20];
    b[..8].copy_from_slice(&(i as u64).to_le_bytes());
    Address::from(b)
}

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
        accounts.insert(
            addr,
            AccountDiff {
                change_type: if i % 10 == 0 {
                    AccountChangeType::Created
                } else {
                    AccountChangeType::Modified
                },
                balance: Some(ValueChange::new(
                    U256::from(1000u64),
                    U256::from(1000u64 + i as u64),
                )),
                nonce: Some(ValueChange::new(i as u64, i as u64 + 1)),
                code_change: None,
                storage,
            },
        );
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
            BenchmarkId::new(
                format!("{}accts_{}slots", accounts, slots),
                accounts * (1 + slots),
            ),
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

/// Disk-backed (MDBX) apply_diff — the current production path. Every tree node
/// is persisted to MDBX and reads pointer-chase through the cache/disk. Compare
/// against the in-memory `bench_apply_diff` above to quantify node-SSD-IO cost.
fn bench_apply_diff_disk(c: &mut Criterion) {
    let mut group = c.benchmark_group("apply_diff_disk");
    group.sample_size(10); // disk path is slow; fewer samples keep the run bounded.

    for &account_count in &[100, 1_000, 10_000] {
        let diff = make_diff(account_count, 0);
        let dir = tempfile::tempdir().unwrap();
        let mut jmt = ShardedJmt::open_disk(dir.path(), 10_000).unwrap();
        group.bench_with_input(
            BenchmarkId::new("accounts_only", account_count),
            &diff,
            |b, diff| {
                b.iter(|| {
                    black_box(jmt.apply_diff_atomic(diff).unwrap());
                });
            },
        );
    }

    group.finish();
}

/// Persistent (Firewood-style) apply_diff — in-memory tree, snapshots disabled
/// (`interval = u64::MAX`) so this measures the pure apply cost with zero node
/// IO. Should track the in-memory `bench_apply_diff` and beat `apply_diff_disk`.
fn bench_apply_diff_persistent(c: &mut Criterion) {
    let mut group = c.benchmark_group("apply_diff_persistent");

    for &account_count in &[100, 1_000, 10_000] {
        let diff = make_diff(account_count, 0);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.snapshot");
        let mut jmt = PersistentJmt::open(&path, u64::MAX).unwrap();
        group.bench_with_input(
            BenchmarkId::new("accounts_only", account_count),
            &diff,
            |b, diff| {
                b.iter(|| {
                    black_box(jmt.apply_diff(diff).unwrap());
                });
            },
        );
    }

    group.finish();
}

/// Same-keys, same-machine head-to-head: self-built SBMT (binary) vs single-tree
/// JMT (16-ary, jmt 0.12), both in-memory, both doing batch insert + root. This
/// is the apples-to-apples "binary vs 16-ary tree engine" comparison driving the
/// JMT→BMT switch decision.
/// End-to-end sharded comparison: `ShardedSbmt::apply_diff` vs the existing
/// `apply_diff` (ShardedJmt) group, same `make_diff`, same machine. Both run the
/// 16-shard rayon path, so this isolates the tree-engine difference at the level
/// the consensus state-root actually uses.
fn bench_apply_diff_sharded_bmt(c: &mut Criterion) {
    let mut group = c.benchmark_group("apply_diff_sharded_bmt");

    for &account_count in &[100, 1_000, 10_000, 50_000] {
        let diff = make_diff(account_count, 0);
        group.bench_with_input(
            BenchmarkId::new("accounts_only", account_count),
            &diff,
            |b, diff| {
                b.iter(|| {
                    let mut jmt = ShardedSbmt::new();
                    black_box(jmt.apply_diff(diff));
                });
            },
        );
    }

    group.finish();
}

fn bench_bmt_vs_jmt(c: &mut Criterion) {
    let mut group = c.benchmark_group("bmt_vs_jmt");

    for &n in &[100, 1_000, 10_000, 50_000] {
        // Shared key set + value blob.
        let keys: Vec<[u8; 32]> = (0..n).map(|i| account_key(&addr_of(i)).0).collect();
        let value = 0xABu8.to_le_bytes().to_vec();

        // SBMT batch (raw [u8;32] keys).
        let sbmt_updates: Vec<([u8; 32], Option<Vec<u8>>)> =
            keys.iter().map(|k| (*k, Some(value.clone()))).collect();
        group.bench_with_input(BenchmarkId::new("sbmt", n), &sbmt_updates, |b, updates| {
            b.iter(|| {
                let mut t = Sbmt::new();
                t.apply_batch(updates);
                black_box(t.root_hash());
            });
        });

        // JMT single tree (KeyHash keys).
        let jmt_updates: Vec<(jmt::KeyHash, Option<Vec<u8>>)> = keys
            .iter()
            .map(|k| (jmt::KeyHash(*k), Some(value.clone())))
            .collect();
        group.bench_with_input(BenchmarkId::new("jmt", n), &jmt_updates, |b, updates| {
            b.iter(|| {
                let mut tree = N42JmtTree::new();
                black_box(tree.apply_batch(updates.clone()).unwrap());
            });
        });
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
            proof.verify(&root).unwrap();
            black_box(());
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
    bench_apply_diff_disk,
    bench_apply_diff_persistent,
    bench_apply_diff_sharded_bmt,
    bench_bmt_vs_jmt,
    bench_root_hash,
    bench_proof_generation,
    bench_proof_verify,
    bench_incremental_updates,
);
criterion_main!(benches);
