//! Profile the twig engine on REAL mainnet account state.
//!
//! Input: a JSON dump of reth `HashedAccounts` (key = 32-byte hashed address,
//! value = {nonce, balance, bytecode_hash}), produced by:
//!   reth.exe db --datadir <reth2k> list HashedAccounts --len 1000000 --json > accounts.json
//!
//! Run:
//!   PROFILE_ACCOUNTS=D:/reth2k-accounts-1m.json cargo run --release --example profile_real
//! On Linux/WSL it also writes `twig_flamegraph.svg` (pprof). On Windows it prints
//! the phase breakdown + per-op time + RSS (pprof's sampler is unix-only).

use n42_twig_core::{Hash, ShardedTwig};
use std::io::Read;
use std::time::Instant;

fn env_usize(k: &str, d: usize) -> usize {
    std::env::var(k)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(d)
}

fn hex32(s: &str) -> Hash {
    let s = s.strip_prefix("0x").unwrap_or(s);
    let mut h = [0u8; 32];
    for (i, b) in h.iter_mut().enumerate() {
        if let Some(slice) = s.get(i * 2..i * 2 + 2) {
            *b = u8::from_str_radix(slice, 16).unwrap_or(0);
        }
    }
    h
}

/// Variable-length balance hex (e.g. "0xe94bdaa400") into 32 big-endian bytes.
fn hex_to_be32(s: &str) -> [u8; 32] {
    let s = s.strip_prefix("0x").unwrap_or(s);
    let s = if s.is_empty() { "0" } else { s };
    let padded = format!("{s:0>64}");
    hex32(&padded)
}

fn main() {
    let path = std::env::var("PROFILE_ACCOUNTS")
        .unwrap_or_else(|_| "D:/reth2k-accounts-1m.json".to_string());
    let limit = env_usize("PROFILE_LIMIT", usize::MAX);

    // ── Parse real accounts -> (key, 72-byte value). The parsed JSON `Value` is
    // dropped at the end of this block so it does not pollute the engine RSS. ──
    let t = Instant::now();
    let ops: Vec<(Hash, Option<Vec<u8>>)> = {
        let mut buf = String::new();
        std::fs::File::open(&path)
            .unwrap_or_else(|e| panic!("open {path}: {e}"))
            .read_to_string(&mut buf)
            .unwrap();
        // reth may prepend tracing logs (ANSI, lines start with ESC 0x1b) to
        // stdout; the JSON array starts at the first newline-then-'[' (logs don't).
        let json = match buf.find("\n[") {
            Some(i) if !buf.trim_start().starts_with('[') => &buf[i + 1..],
            _ => buf.as_str(),
        };
        let v: serde_json::Value = serde_json::from_str(json).unwrap();
        let arr = v.as_array().unwrap();
        let mut ops = Vec::with_capacity(arr.len().min(limit));
        for e in arr.iter().take(limit) {
            let pair = e.as_array().unwrap();
            let key = hex32(pair[0].as_str().unwrap());
            let acct = &pair[1];
            let nonce = acct["nonce"].as_u64().unwrap_or(0);
            let bal = acct["balance"].as_str().unwrap_or("0x0");
            let code = acct["bytecode_hash"].as_str();
            let mut value = Vec::with_capacity(72);
            value.extend_from_slice(&nonce.to_le_bytes());
            value.extend_from_slice(&hex_to_be32(bal));
            value.extend_from_slice(&code.map(hex32).unwrap_or([0u8; 32]));
            ops.push((key, Some(value)));
        }
        ops
    };
    let parse_s = t.elapsed().as_secs_f64();
    let n = ops.len();

    // All keys kept for proof timing + the update phase (32 B/key harness cost,
    // noted separately from the engine RSS).
    let all_keys: Vec<Hash> = ops.iter().map(|o| o.0).collect();
    let samples = env_usize("PROFILE_PROOF_SAMPLES", 10_000).min(n);
    let step = (n / samples.max(1)).max(1);
    let sample_keys: Vec<Hash> = all_keys
        .iter()
        .step_by(step)
        .take(samples)
        .copied()
        .collect();

    #[cfg(unix)]
    let guard = pprof::ProfilerGuardBuilder::default()
        .frequency(2000)
        .build()
        .ok();

    // ── Build (genesis load): one canonical apply_batch over all accounts. ──
    let mut tree = ShardedTwig::new();
    let t = Instant::now();
    let (_ver, _build_root) = tree.apply_batch(&ops);
    let build_s = t.elapsed().as_secs_f64();
    drop(ops); // free the harness ops/clones so RSS reflects the engine only
    let rss = memory_stats::memory_stats()
        .map(|m| m.physical_mem)
        .unwrap_or(0);

    // ── Update phase: overwrite random existing accounts in blocks (the
    // consensus-relevant + C2-comparable workload). apply_batch parallelizes
    // across the 16 shards (rayon feature). ──
    let updates = env_usize("PROFILE_UPDATES", 0);
    if updates > 0 && n > 0 {
        let block = env_usize("PROFILE_UPD_BLOCK", 25_000);
        let mut rng = 0x9e3779b97f4a7c15u64;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        tree.reserve(updates, updates * 72); // avoid realloc contention mid-apply
        // Stage each block's values in one reusable buffer + pass borrowed slices
        // (the owned API's per-op Vec<u8> was the profiled allocator hot spot).
        let mut valbuf = vec![0u8; block * 72];
        let mut keys: Vec<Hash> = Vec::with_capacity(block);
        let mut done = 0usize;
        let t = Instant::now();
        while done < updates {
            let cnt = block.min(updates - done);
            keys.clear();
            for c in 0..cnt {
                keys.push(all_keys[(next() as usize) % n]);
                valbuf[c * 72..c * 72 + 8].copy_from_slice(&next().to_le_bytes());
            }
            let ops: Vec<(Hash, Option<&[u8]>)> = (0..cnt)
                .map(|c| (keys[c], Some(&valbuf[c * 72..(c + 1) * 72])))
                .collect();
            tree.apply_batch_refs(&ops);
            done += cnt;
        }
        let upd_s = t.elapsed().as_secs_f64();
        println!(
            "updates:   {updates} ops in blocks of {block}: {:.2}s ({:.0} upd/s)",
            upd_s,
            updates as f64 / upd_s
        );
    }

    // ── Proof sampling on real keys (against the CURRENT root — the update
    // phase above changes it). ──
    let root = tree.root();
    let mut gen_us = 0.0f64;
    let mut ver_us = 0.0f64;
    let mut sizes = 0usize;
    let mut counted = 0usize;
    for &key in &sample_keys {
        let t = Instant::now();
        let p = tree.prove(&key).unwrap();
        gen_us += t.elapsed().as_secs_f64() * 1e6;
        sizes += bincode::serialize(&p).unwrap().len();
        let t = Instant::now();
        let ok = p.verify_for_key(&root, &key).is_ok();
        ver_us += t.elapsed().as_secs_f64() * 1e6;
        assert!(ok);
        counted += 1;
    }

    #[cfg(unix)]
    if let Some(g) = guard
        && let Ok(report) = g.report().build()
        && let Ok(f) = std::fs::File::create("twig_flamegraph.svg")
    {
        let _ = report.flamegraph(f);
        eprintln!("wrote twig_flamegraph.svg");
    }

    println!("\n=== twig engine on REAL mainnet accounts ===");
    println!("source:    {path}");
    println!("accounts:  {n}");
    println!("parse:     {parse_s:.2}s");
    println!(
        "build:     {build_s:.2}s  ({:.0} accts/s, {:.3} us/acct)",
        n as f64 / build_s,
        build_s * 1e6 / n as f64
    );
    println!(
        "RSS:       {:.0} MB  ({:.1} B/acct)",
        rss as f64 / 1e6,
        rss as f64 / n as f64
    );
    println!("root:      0x{}", hex::encode_hash(&root));
    println!(
        "proof:     avg {:.0} B, gen {:.2} us, verify {:.2} us  ({counted} samples)",
        sizes as f64 / counted.max(1) as f64,
        gen_us / counted.max(1) as f64,
        ver_us / counted.max(1) as f64,
    );
    #[cfg(not(unix))]
    println!("(pprof flamegraph: run on Linux/WSL — sampler is unix-only)");
}

mod hex {
    pub fn encode_hash(h: &[u8; 32]) -> String {
        let mut s = String::with_capacity(64);
        for b in h {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }
}
