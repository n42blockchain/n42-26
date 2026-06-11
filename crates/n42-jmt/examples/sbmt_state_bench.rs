//! Standalone SBMT state-tree benchmark, emitting metrics directly comparable to
//! gov5's Go `bench_state` BMT report (`bench_state_*.json`).
//!
//! Measures, over a synthetic replay of `ENTRIES` account creations split into
//! blocks of `BLOCK` entries:
//!   - per-block apply+root time (`root_time_avg_us`), `blocks_per_sec`,
//!     `entries_per_sec`
//!   - live-tree node count + serialized bytes (gov5 store accounting) and the
//!     real KV `snapshot_bytes`
//!   - proof size avg/p50/p99, depth, gen + verify time (sampled)
//!   - resident memory after build (`rss_after_build_bytes`)
//!
//! Run:
//!   cargo run --release --example sbmt_state_bench
//!   SBMT_BENCH_ENTRIES=5000000 SBMT_BENCH_BLOCK=200 cargo run --release --example sbmt_state_bench
//!
//! Env knobs: SBMT_BENCH_ENTRIES (default 1_000_000), SBMT_BENCH_BLOCK (200),
//! SBMT_BENCH_PROOF_SAMPLES (10_000), SBMT_BENCH_OUT (target/sbmt_bench.json).
//!
//! Fairness note: gov5's BMT `storage_bytes`/`total_nodes` are *cumulative
//! archival* (every copy-on-write node version across all blocks). This tool's
//! `storage_bytes_live_nodes`/`total_nodes` are the *live* tree only, so they are
//! NOT directly comparable to gov5's archival totals — `proof_*`, `avg_node_size`,
//! and per-block timing ARE the apples-to-apples metrics.

use alloy_primitives::{Address, U256};
use n42_execution::state_diff::{AccountChangeType, AccountDiff, StateDiff, ValueChange};
use n42_jmt::{ShardedSbmt, account_key};
use rand::Rng;
use std::collections::BTreeMap;
use std::time::Instant;

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Distinct 20-byte address for index `i` (no 256-collision).
fn addr_of(i: usize) -> Address {
    let mut b = [0u8; 20];
    b[..8].copy_from_slice(&(i as u64).to_le_bytes());
    // spread entropy into a second word so the top nibble (shard) is well mixed
    b[8..16].copy_from_slice(&((i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)).to_le_bytes());
    Address::from(b)
}

fn block_diff(start: usize, count: usize) -> StateDiff {
    let mut accounts = BTreeMap::new();
    for i in start..start + count {
        accounts.insert(
            addr_of(i),
            AccountDiff {
                change_type: AccountChangeType::Created,
                balance: Some(ValueChange::new(U256::ZERO, U256::from(1_000u64 + i as u64))),
                nonce: Some(ValueChange::new(0, 1)),
                code_change: None,
                storage: BTreeMap::new(),
            },
        );
    }
    StateDiff { accounts }
}

fn percentile(sorted: &[usize], p: f64) -> usize {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((p / 100.0) * (sorted.len() - 1) as f64).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn main() {
    let entries = env_usize("SBMT_BENCH_ENTRIES", 1_000_000);
    let block = env_usize("SBMT_BENCH_BLOCK", 200).max(1);
    let proof_samples = env_usize("SBMT_BENCH_PROOF_SAMPLES", 10_000);
    let out_path =
        std::env::var("SBMT_BENCH_OUT").unwrap_or_else(|_| "target/sbmt_bench.json".to_string());

    let blocks = entries.div_ceil(block);
    eprintln!(
        "SBMT bench: entries={entries} block={block} blocks={blocks} proof_samples={proof_samples}"
    );

    // ── Build: apply block-by-block, timing each block's apply+root. ──
    let mut tree = ShardedSbmt::new();
    let mut per_block_us: Vec<f64> = Vec::with_capacity(blocks);
    let build_start = Instant::now();
    let mut produced = 0usize;
    for _ in 0..blocks {
        let count = block.min(entries - produced);
        let diff = block_diff(produced, count);
        let t = Instant::now();
        let _ = tree.apply_diff(&diff);
        per_block_us.push(t.elapsed().as_secs_f64() * 1e6);
        produced += count;
        if produced >= entries {
            break;
        }
    }
    let build_elapsed = build_start.elapsed().as_secs_f64();

    let rss_after_build = memory_stats::memory_stats().map(|m| m.physical_mem).unwrap_or(0);

    // ── Optional update phase: overwrite random existing accounts (Modified),
    // the apples-to-apples workload for comparing against QMDB-style update
    // benchmarks (no tree growth: in-place value-hash update, no split/alloc). ──
    let updates = env_usize("SBMT_BENCH_UPDATES", 0);
    let mut upd_per_sec = 0.0;
    if updates > 0 {
        let mut rng = rand::rng();
        let mut upd_us = 0.0f64;
        let mut done = 0usize;
        while done < updates {
            let count = block.min(updates - done);
            let mut accounts = BTreeMap::new();
            for _ in 0..count {
                let i = rng.random_range(0..entries);
                accounts.insert(
                    addr_of(i),
                    AccountDiff {
                        change_type: AccountChangeType::Modified,
                        balance: Some(ValueChange::new(U256::ZERO, U256::from(rng.random::<u64>()))),
                        nonce: Some(ValueChange::new(0, 1)),
                        code_change: None,
                        storage: BTreeMap::new(),
                    },
                );
            }
            let diff = StateDiff { accounts };
            let t = Instant::now();
            let _ = tree.apply_diff(&diff);
            upd_us += t.elapsed().as_secs_f64() * 1e6;
            done += count;
        }
        upd_per_sec = updates as f64 / (upd_us / 1e6).max(f64::MIN_POSITIVE);
        println!(
            "updates:      {updates} ops, {upd_per_sec:.0} upd/s (block={block}, overwrite)"
        );
    }
    let _ = upd_per_sec;

    // ── Footprint. ──
    let ns = tree.node_stats();
    let total_nodes = ns.total_nodes();
    let snapshot = tree.snapshot();
    let snapshot_bytes: u64 = snapshot
        .entries
        .iter()
        .map(|(k, v)| (k.len() + v.len()) as u64)
        .sum();

    // ── Proof sampling. ──
    let root = tree.root_hash();
    let mut rng = rand::rng();
    let mut sizes: Vec<usize> = Vec::with_capacity(proof_samples);
    let mut depths: Vec<usize> = Vec::with_capacity(proof_samples);
    let mut gen_us_total = 0.0f64;
    let mut verify_us_total = 0.0f64;
    let mut verify_ok = 0usize;
    let n_samples = proof_samples.min(entries);
    for _ in 0..n_samples {
        let i = rng.random_range(0..entries);
        let key = account_key(&addr_of(i)).0;

        let t = Instant::now();
        let proof = tree.prove(key);
        gen_us_total += t.elapsed().as_secs_f64() * 1e6;

        sizes.push(proof.estimated_size());
        depths.push(proof.inner.siblings.len() + proof.shard_path.len());

        let t = Instant::now();
        let ok = proof.verify_for_key(&root.0, &key).is_ok();
        verify_us_total += t.elapsed().as_secs_f64() * 1e6;
        if ok {
            verify_ok += 1;
        }
    }
    assert_eq!(verify_ok, n_samples, "all sampled proofs must verify (bound)");

    sizes.sort_unstable();
    let proof_size_avg = sizes.iter().sum::<usize>() as f64 / sizes.len().max(1) as f64;
    let proof_depth_avg = depths.iter().sum::<usize>() as f64 / depths.len().max(1) as f64;

    let total_block_us: f64 = per_block_us.iter().sum();
    let root_time_avg_us = total_block_us / per_block_us.len().max(1) as f64;
    let blocks_per_sec = per_block_us.len() as f64 / (total_block_us / 1e6).max(f64::MIN_POSITIVE);
    let entries_per_sec = entries as f64 / build_elapsed.max(f64::MIN_POSITIVE);

    let report = serde_json::json!({
        "tree": "n42-sbmt",
        "entries_processed": entries,
        "blocks_processed": per_block_us.len(),
        "entries_per_block": block,
        "elapsed_build_sec": build_elapsed,
        "sbmt": {
            "storage_bytes_live_nodes": ns.serialized_bytes,
            "snapshot_bytes": snapshot_bytes,
            "total_nodes": total_nodes,
            "internal_nodes": ns.internal_nodes,
            "leaf_nodes": ns.leaf_nodes,
            "avg_node_size": ns.serialized_bytes as f64 / total_nodes.max(1) as f64,
            "root_time_avg_us": root_time_avg_us,
            "blocks_per_sec": blocks_per_sec,
            "entries_per_sec": entries_per_sec,
            "proof_size_avg": proof_size_avg,
            "proof_size_p50": percentile(&sizes, 50.0),
            "proof_size_p99": percentile(&sizes, 99.0),
            "proof_depth_avg": proof_depth_avg,
            "proof_gen_time_avg_us": gen_us_total / n_samples.max(1) as f64,
            "proof_verify_time_avg_us": verify_us_total / n_samples.max(1) as f64,
            "rss_after_build_bytes": rss_after_build,
        }
    });

    let pretty = serde_json::to_string_pretty(&report).unwrap();
    if let Some(parent) = std::path::Path::new(&out_path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&out_path, &pretty).expect("write report");

    // ── Human summary + gov5 BMT @5M reference. ──
    let s = &report["sbmt"];
    println!("\n=== n42-SBMT state bench ({entries} entries / {} blocks) ===", per_block_us.len());
    println!("build:        {build_elapsed:.2}s  ({entries_per_sec:.0} entries/s)");
    println!("per-block:    {root_time_avg_us:.2} us  ({blocks_per_sec:.1} blocks/s)  [apply+root]");
    println!("live nodes:   {total_nodes}  (int {} / leaf {})", ns.internal_nodes, ns.leaf_nodes);
    println!("avg node sz:  {:.1} B", s["avg_node_size"].as_f64().unwrap());
    println!("live node B:  {:.2} MB", ns.serialized_bytes as f64 / 1e6);
    println!("snapshot B:   {:.2} MB (KV)", snapshot_bytes as f64 / 1e6);
    println!("RSS:          {:.2} MB", rss_after_build as f64 / 1e6);
    println!(
        "proof:        avg {:.1} B (p50 {} / p99 {}), depth {:.2}, gen {:.2} us, verify {:.2} us",
        proof_size_avg,
        percentile(&sizes, 50.0),
        percentile(&sizes, 99.0),
        proof_depth_avg,
        gen_us_total / n_samples.max(1) as f64,
        verify_us_total / n_samples.max(1) as f64,
    );
    println!("\n--- gov5 Go BMT reference (bench_state_5m_bmt.json, 18.34M entries/5M blocks) ---");
    println!("per-block:    4010.67 us  (249.3 blocks/s)  [root only]");
    println!("avg node sz:  95.9 B   (archival: 350.9M nodes, 33.65 GB cumulative)");
    println!("proof:        avg 713.9 B (p50 704 / p99 928), depth 22.31, gen 66.82 us, verify 2.56 us");
    println!("\nreport written: {out_path}");
}
