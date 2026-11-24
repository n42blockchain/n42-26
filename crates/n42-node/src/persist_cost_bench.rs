//! What the RocksDB half of a block's persistence costs: the transaction
//! hash index and the account history shards a block of transfers writes,
//! built and committed as `save_blocks` does, with the account history
//! shards made one account after another and in parallel
//! (`N42_PARALLEL_ACCOUNT_HISTORY`). Each batch's commit, a synced write, is
//! timed on its own: what the index costs is what dropping it would save.
//!
//! `N42_PERSIST_BENCH_DIR` names the directory the databases are made in;
//! it should be on the disk a node's datadir is on, since the commits sync.
//!
//! Also timed: the hashes put in key order, and the two batches committed
//! from two threads at once. `N42_PERSIST_BENCH_LOAD=1` keeps the disk busy
//! beside it.
//!
//! `cargo test --profile profiling -p n42-node --lib persist_cost -- --ignored --nocapture`

use alloy_primitives::{Address, B256, keccak256};
use reth_db_api::tables;
use reth_provider::providers::{RocksDBBuilder, RocksDBProvider};
use std::{collections::BTreeMap, time::Instant};

fn var(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn address(domain: u8, n: u64) -> Address {
    let mut bytes = [0u8; 20];
    bytes[0] = domain;
    bytes[12..].copy_from_slice(&n.to_be_bytes());
    Address::from(bytes)
}

/// A block's transaction hashes with their numbers, and the accounts it
/// changed with the block's number.
type BlockWrites = (Vec<(B256, u64)>, BTreeMap<Address, Vec<u64>>);

/// A block of `txs` transfers from `senders` senders to new recipients.
fn block(number: u64, txs: u64, senders: u64) -> BlockWrites {
    let first_tx = number * txs;
    let hashes = (0..txs)
        .map(|i| (keccak256((first_tx + i).to_be_bytes()), first_tx + i))
        .collect();
    let mut accounts = BTreeMap::new();
    for i in 0..senders.min(txs) {
        accounts.insert(address(0xa1, i), vec![number]);
    }
    for i in 0..txs {
        accounts.insert(address(0xa2, first_tx + i), vec![number]);
    }
    (hashes, accounts)
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

struct Timings {
    hashes: Vec<f64>,
    history: Vec<f64>,
    commit_hashes: Vec<f64>,
    commit_history: Vec<f64>,
    /// Both commits, start to end.
    commit: Vec<f64>,
}

/// How a run builds and commits the two batches.
#[derive(Clone, Copy)]
struct Mode {
    /// Account history shards read and extended in parallel.
    parallel: bool,
    /// The transaction hashes put into their batch in key order.
    sorted: bool,
    /// The two batches committed at once from two threads.
    concurrent: bool,
}

/// Writes `blocks` blocks into `rocksdb`: the two batches built at once, as
/// `write_blocks_data` builds them, then committed.
fn run(rocksdb: &RocksDBProvider, blocks: u64, txs: u64, senders: u64, mode: Mode) -> Timings {
    let mut out = Timings {
        hashes: vec![],
        history: vec![],
        commit_hashes: vec![],
        commit_history: vec![],
        commit: vec![],
    };
    for number in 1..=blocks {
        let (mut hashes, accounts) = block(number, txs, senders);
        let ((hash_batch, hash_ms), (history_batch, history_ms)) = std::thread::scope(|scope| {
            let hashes = scope.spawn(|| {
                let at = Instant::now();
                if mode.sorted {
                    hashes.sort_unstable();
                }
                let mut batch = rocksdb.batch();
                for (hash, number) in hashes {
                    batch.put::<tables::TransactionHashNumbers>(hash, &number).expect("put");
                }
                (batch.into_inner(), at.elapsed().as_secs_f64() * 1e3)
            });
            let history = scope.spawn(|| {
                let at = Instant::now();
                let mut batch = rocksdb.batch();
                if mode.parallel {
                    batch.append_account_history_shards(accounts).expect("shards");
                } else {
                    for (address, indices) in accounts {
                        batch.append_account_history_shard(address, indices).expect("shard");
                    }
                }
                (batch.into_inner(), at.elapsed().as_secs_f64() * 1e3)
            });
            (hashes.join().expect("hashes"), history.join().expect("history"))
        });
        let at = Instant::now();
        if mode.concurrent {
            let (hashes_ms, history_ms) = std::thread::scope(|scope| {
                let commit = |batch| {
                    move || {
                        let at = Instant::now();
                        rocksdb.commit_batch(batch).expect("commit");
                        at.elapsed().as_secs_f64() * 1e3
                    }
                };
                let hashes = scope.spawn(commit(hash_batch));
                let history = scope.spawn(commit(history_batch));
                (hashes.join().expect("hashes"), history.join().expect("history"))
            });
            out.commit_hashes.push(hashes_ms);
            out.commit_history.push(history_ms);
        } else {
            let first = Instant::now();
            rocksdb.commit_batch(hash_batch).expect("commit");
            out.commit_hashes.push(first.elapsed().as_secs_f64() * 1e3);
            let second = Instant::now();
            rocksdb.commit_batch(history_batch).expect("commit");
            out.commit_history.push(second.elapsed().as_secs_f64() * 1e3);
        }
        out.commit.push(at.elapsed().as_secs_f64() * 1e3);
        out.hashes.push(hash_ms);
        out.history.push(history_ms);
    }
    out
}

fn report(name: &str, t: Timings) {
    let (hashes, history) = (median(t.hashes), median(t.history));
    let (commit_hashes, commit_history) = (median(t.commit_hashes), median(t.commit_history));
    let commit = median(t.commit);
    println!(
        "PERSIST_COST {name}: build: tx hash batch {hashes:.0} ms, account history batch {history:.0} ms \
         (write phase, both at once, {:.0} ms); commit: tx hashes {commit_hashes:.0} ms, account history \
         {commit_history:.0} ms, both {commit:.0} ms; rocksdb per block {:.0} ms",
        hashes.max(history),
        hashes.max(history) + commit,
    );
}

/// With `N42_PERSIST_BENCH_LOAD=1`, threads that keep writing and syncing
/// files beside the bench, as a node's other stores do while it persists.
fn disk_load(root: &std::path::Path) -> Option<(std::sync::Arc<std::sync::atomic::AtomicBool>, Vec<std::thread::JoinHandle<()>>)> {
    use std::io::{Seek, Write};
    use std::sync::atomic::Ordering;
    if std::env::var("N42_PERSIST_BENCH_LOAD").map_or(true, |v| v != "1") {
        return None;
    }
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let handles = (0..3)
        .map(|k| {
            let stop = stop.clone();
            let path = root.join(format!("load-{k}.dat"));
            std::thread::spawn(move || {
                let chunk = vec![k as u8 + 1; 4 << 20];
                let mut file = std::fs::File::create(&path).expect("load file");
                let mut n = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    file.write_all(&chunk).expect("load write");
                    n += 1;
                    if n.is_multiple_of(8) {
                        file.sync_data().expect("load sync");
                    }
                    if n.is_multiple_of(64) {
                        file.rewind().expect("load rewind");
                    }
                }
                drop(file);
                let _ = std::fs::remove_file(&path);
            })
        })
        .collect();
    Some((stop, handles))
}

#[test]
#[ignore = "timing run with synced writes, not a check"]
fn persist_cost_of_a_full_block_of_transfers() {
    let txs = var("N42_PERSIST_BENCH_TXS", 95_238);
    let senders = var("N42_PERSIST_BENCH_SENDERS", 20_000);
    let blocks = var("N42_PERSIST_BENCH_BLOCKS", 12);
    let root = std::env::var("N42_PERSIST_BENCH_DIR").map(std::path::PathBuf::from).unwrap_or_else(|_| std::env::temp_dir());
    std::fs::create_dir_all(&root).expect("bench dir");
    println!("persist_cost: {blocks} blocks of {txs} transfers from {senders} senders, in {}", root.display());
    let load = disk_load(&root);
    let modes = [
        ("serial", Mode { parallel: false, sorted: false, concurrent: false }),
        ("parallel", Mode { parallel: true, sorted: false, concurrent: false }),
        ("parallel+sorted", Mode { parallel: true, sorted: true, concurrent: false }),
        ("parallel+concurrent", Mode { parallel: true, sorted: false, concurrent: true }),
        ("parallel+sorted+concurrent", Mode { parallel: true, sorted: true, concurrent: true }),
    ];
    for (name, mode) in modes {
        let dir = tempfile::tempdir_in(&root).expect("dir");
        let rocksdb = RocksDBBuilder::new(dir.path()).with_default_tables().build().expect("rocksdb");
        report(name, run(&rocksdb, blocks, txs, senders, mode));
    }
    if let Some((stop, handles)) = load {
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for handle in handles {
            handle.join().expect("load thread");
        }
    }
    // The same writes, both ways, give the same history.
    let dirs = [tempfile::tempdir_in(&root).expect("dir"), tempfile::tempdir_in(&root).expect("dir")];
    let dbs: Vec<_> = dirs
        .iter()
        .map(|dir| RocksDBBuilder::new(dir.path()).with_default_tables().build().expect("rocksdb"))
        .collect();
    run(&dbs[0], 3, 2_000, 500, Mode { parallel: false, sorted: false, concurrent: false });
    run(&dbs[1], 3, 2_000, 500, Mode { parallel: true, sorted: true, concurrent: true });
    let dump = |db: &RocksDBProvider| {
        db.iter::<tables::AccountsHistory>().expect("iter").map(|entry| entry.expect("entry")).collect::<Vec<_>>()
    };
    assert_eq!(dump(&dbs[0]), dump(&dbs[1]), "parallel shards are the serial shards");
    let hashes = |db: &RocksDBProvider| {
        db.iter::<tables::TransactionHashNumbers>().expect("iter").map(|entry| entry.expect("entry")).collect::<Vec<_>>()
    };
    assert_eq!(hashes(&dbs[0]), hashes(&dbs[1]), "sorted puts write the same hashes");
}
