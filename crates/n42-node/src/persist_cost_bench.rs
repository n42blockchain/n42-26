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
}

/// Writes `blocks` blocks into `rocksdb`: the two batches built at once, as
/// `write_blocks_data` builds them, then committed.
fn run(rocksdb: &RocksDBProvider, blocks: u64, txs: u64, senders: u64, parallel: bool) -> Timings {
    let mut out =
        Timings { hashes: vec![], history: vec![], commit_hashes: vec![], commit_history: vec![] };
    for number in 1..=blocks {
        let (hashes, accounts) = block(number, txs, senders);
        let ((hash_batch, hash_ms), (history_batch, history_ms)) = std::thread::scope(|scope| {
            let hashes = scope.spawn(|| {
                let at = Instant::now();
                let mut batch = rocksdb.batch();
                for (hash, number) in hashes {
                    batch.put::<tables::TransactionHashNumbers>(hash, &number).expect("put");
                }
                (batch.into_inner(), at.elapsed().as_secs_f64() * 1e3)
            });
            let history = scope.spawn(|| {
                let at = Instant::now();
                let mut batch = rocksdb.batch();
                if parallel {
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
        rocksdb.commit_batch(hash_batch).expect("commit");
        out.commit_hashes.push(at.elapsed().as_secs_f64() * 1e3);
        let at = Instant::now();
        rocksdb.commit_batch(history_batch).expect("commit");
        out.commit_history.push(at.elapsed().as_secs_f64() * 1e3);
        out.hashes.push(hash_ms);
        out.history.push(history_ms);
    }
    out
}

fn report(name: &str, t: Timings) {
    let (hashes, history) = (median(t.hashes), median(t.history));
    let (commit_hashes, commit_history) = (median(t.commit_hashes), median(t.commit_history));
    println!(
        "PERSIST_COST {name}: build: tx hash batch {hashes:.0} ms, account history batch {history:.0} ms \
         (write phase, both at once, {:.0} ms); commit: tx hashes {commit_hashes:.0} ms, account history \
         {commit_history:.0} ms; rocksdb per block {:.0} ms",
        hashes.max(history),
        hashes.max(history) + commit_hashes + commit_history,
    );
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
    for (name, parallel) in [("serial", false), ("parallel", true)] {
        let dir = tempfile::tempdir_in(&root).expect("dir");
        let rocksdb = RocksDBBuilder::new(dir.path()).with_default_tables().build().expect("rocksdb");
        report(name, run(&rocksdb, blocks, txs, senders, parallel));
    }
    // The same writes, both ways, give the same history.
    let dirs = [tempfile::tempdir_in(&root).expect("dir"), tempfile::tempdir_in(&root).expect("dir")];
    let dbs: Vec<_> = dirs
        .iter()
        .map(|dir| RocksDBBuilder::new(dir.path()).with_default_tables().build().expect("rocksdb"))
        .collect();
    run(&dbs[0], 3, 2_000, 500, false);
    run(&dbs[1], 3, 2_000, 500, true);
    let dump = |db: &RocksDBProvider| {
        db.iter::<tables::AccountsHistory>().expect("iter").map(|entry| entry.expect("entry")).collect::<Vec<_>>()
    };
    assert_eq!(dump(&dbs[0]), dump(&dbs[1]), "parallel shards are the serial shards");
}
