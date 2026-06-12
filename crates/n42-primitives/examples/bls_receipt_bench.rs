//! Mobile-receipt BLS verification: per-signature verify (current star_hub path)
//! vs same-message aggregate verify (one pairing) — the receipts of one block all
//! sign the same `(block_hash, number, receipts_root)` message, so aggregation
//! applies directly.
//!
//! Run: cargo run --release --example bls_receipt_bench -p n42-primitives
//! Env: BLS_BENCH_N (default 1000)

use n42_primitives::BlsSecretKey;
use n42_primitives::bls::{AggregateSignature, batch_verify};
use std::time::Instant;

fn main() {
    let n: usize = std::env::var("BLS_BENCH_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1000);
    let msg = b"block_hash || number || receipts_root (shared per block)";

    eprintln!("keygen+sign {n} receipts...");
    let keys: Vec<BlsSecretKey> = (0..n).map(|_| BlsSecretKey::random().unwrap()).collect();
    let pks: Vec<_> = keys.iter().map(|k| k.public_key()).collect();
    let sigs: Vec<_> = keys.iter().map(|k| k.sign(msg)).collect();

    // ── Current path: one pairing per receipt. ──
    let t = Instant::now();
    for (pk, sig) in pks.iter().zip(&sigs) {
        pk.verify(msg, sig).expect("valid");
    }
    let per_sig = t.elapsed();

    // ── Aggregate path: point-adds + ONE pairing. ──
    let t = Instant::now();
    let sig_refs: Vec<_> = sigs.iter().collect();
    let agg = AggregateSignature::aggregate(&sig_refs).expect("agg");
    let agg_time = t.elapsed();

    let t = Instant::now();
    let pk_refs: Vec<_> = pks.iter().collect();
    AggregateSignature::verify_aggregate(msg, &agg, &pk_refs).expect("agg valid");
    let agg_verify = t.elapsed();

    // ── Batch verify (random coefficients -- rogue-key SAFE, no PoP needed). ──
    let t = Instant::now();
    let msgs: Vec<&[u8]> = (0..n).map(|_| msg.as_slice()).collect();
    let sig_refs2: Vec<_> = sigs.iter().collect();
    let pk_refs2: Vec<_> = pks.iter().collect();
    batch_verify(&msgs, &sig_refs2, &pk_refs2).expect("batch valid");
    let batch_time = t.elapsed();

    let total_agg = agg_time + agg_verify;
    println!(
        "batch_verify (coef):  {:>10.1} ms  (rogue-key safe, no PoP)",
        batch_time.as_secs_f64() * 1e3
    );
    println!("\n=== BLS receipt verification, N = {n} (same message) ===");
    println!(
        "per-signature verify: {:>10.1} ms  ({:.0} us/sig)  [current star_hub path, 1 core]",
        per_sig.as_secs_f64() * 1e3,
        per_sig.as_secs_f64() * 1e6 / n as f64
    );
    println!(
        "aggregate (sig adds): {:>10.1} ms",
        agg_time.as_secs_f64() * 1e3
    );
    println!(
        "aggregate verify:     {:>10.1} ms  (pk adds + 1 pairing)",
        agg_verify.as_secs_f64() * 1e3
    );
    println!(
        "aggregate TOTAL:      {:>10.1} ms  -> {:.0}x speedup, 1 core",
        total_agg.as_secs_f64() * 1e3,
        per_sig.as_secs_f64() / total_agg.as_secs_f64()
    );
    println!(
        "\nextrapolated to 10K phones: per-sig {:.1} s vs aggregate {:.0} ms",
        per_sig.as_secs_f64() * 10_000.0 / n as f64,
        total_agg.as_secs_f64() * 1e3 * 10_000.0 / n as f64
    );
}
