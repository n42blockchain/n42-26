//! Measures what the H2-v4 batch path buys over per-signature verification, at
//! the committee sizes the interop plan actually targets.
//!
//! Run with: `cargo run --release -p n42-primitives --example h2_v4_batch_probe`

use n42_primitives::BlsSecretKey;
use n42_primitives::bls::{batch_verify_h2_v4, batch_verify_h2_v4_with_fallback};
use std::time::Instant;

fn main() {
    let message = b"h2-v4 quorum certificate probe";

    for size in [7usize, 21, 100, 500] {
        let secret_keys: Vec<_> = (0..size)
            .map(|index| {
                let mut seed = [0u8; 32];
                seed[..8].copy_from_slice(&(index as u64).to_le_bytes());
                BlsSecretKey::key_gen(&seed).expect("deterministic key")
            })
            .collect();
        let public_keys: Vec<_> = secret_keys.iter().map(|sk| sk.public_key()).collect();
        let signatures: Vec<_> = secret_keys
            .iter()
            .map(|sk| sk.sign_h2_v4(message))
            .collect();

        let messages: Vec<&[u8]> = vec![message.as_ref(); size];
        let signature_refs: Vec<_> = signatures.iter().collect();
        let public_key_refs: Vec<_> = public_keys.iter().collect();

        let start = Instant::now();
        for (public_key, signature) in public_keys.iter().zip(&signatures) {
            public_key
                .verify_h2_v4_prevalidated(message, signature)
                .expect("valid signature");
        }
        let single = start.elapsed();

        let start = Instant::now();
        batch_verify_h2_v4(&messages, &signature_refs, &public_key_refs).expect("valid batch");
        let batch = start.elapsed();

        // The QC build path goes through the fallback wrapper, so time that too.
        let start = Instant::now();
        batch_verify_h2_v4_with_fallback(&messages, &signature_refs, &public_key_refs)
            .expect("valid batch");
        let fallback = start.elapsed();

        println!(
            "n={size:3}  per-signature {:>8.2?}  batch {:>8.2?}  ({:.2}x)  with-fallback {:>8.2?}",
            single,
            batch,
            single.as_secs_f64() / batch.as_secs_f64(),
            fallback,
        );
    }
}
