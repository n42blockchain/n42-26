//! Micro-batched BLS verification for mobile receipts.
//!
//! star_hub used to verify every receipt with its own pairing (one
//! `spawn_blocking` per receipt, ~0.5 ms each — 10K phones ≈ 4.8 s of
//! accumulated single-core work per block). All receipts for a block sign the
//! same 72-byte message (`build_signing_message` deliberately excludes
//! `timestamp_ms` for exactly this purpose), and `n42-primitives` ships
//! coefficient-randomized `batch_verify_with_fallback` (rogue-key safe, no
//! proof-of-possession needed). Batching 256 receipts / 50 ms through it cuts
//! verification ~24x (measured: 1000 sigs = 482 ms individually vs 20 ms
//! batched, one core). See `docs/devlog-66`.
//!
//! Invalid signatures are warned + counted exactly like the old per-receipt
//! path (it also only dropped them); valid receipts flow to `HubEvent::
//! ReceiptReceived` with ≤ ~50 ms added latency — receipts are an asynchronous
//! attestation stream, not on the consensus critical path.

// Receipts arrive and leave as `Box<VerificationReceipt>` (the HubEvent protocol
// type); keeping the Box through the batch avoids re-boxing at the boundary.
#![allow(clippy::vec_box)]

use super::star_hub::HubEvent;
use n42_mobile::receipt::VerificationReceipt;
use n42_primitives::{BlsPublicKey, bls::batch_verify_with_fallback};
use tokio::sync::mpsc;

/// Flush when this many receipts are pending…
const BATCH_MAX: usize = 256;
/// …or when this much time has passed since the last flush.
const BATCH_INTERVAL_MS: u64 = 50;

/// Spawns the global receipt batch-verifier task. Sessions send parsed receipts
/// (already identity-checked against their handshake key) into the returned
/// channel; verified receipts are forwarded as [`HubEvent::ReceiptReceived`].
pub(crate) fn spawn(event_tx: mpsc::Sender<HubEvent>) -> mpsc::Sender<Box<VerificationReceipt>> {
    let (tx, mut rx) = mpsc::channel::<Box<VerificationReceipt>>(8192);
    tokio::spawn(async move {
        let mut buf: Vec<Box<VerificationReceipt>> = Vec::with_capacity(BATCH_MAX);
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(BATCH_INTERVAL_MS));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                r = rx.recv() => match r {
                    Some(receipt) => {
                        buf.push(receipt);
                        if buf.len() >= BATCH_MAX {
                            flush(&mut buf, &event_tx).await;
                        }
                    }
                    None => {
                        // All senders dropped (hub shutdown): final flush, exit.
                        flush(&mut buf, &event_tx).await;
                        break;
                    }
                },
                _ = tick.tick() => {
                    if !buf.is_empty() {
                        flush(&mut buf, &event_tx).await;
                    }
                }
            }
        }
        tracing::debug!("receipt batch verifier stopped");
    });
    tx
}

/// Verify the pending batch off the async runtime and forward the valid receipts.
async fn flush(buf: &mut Vec<Box<VerificationReceipt>>, event_tx: &mpsc::Sender<HubEvent>) {
    let batch = std::mem::take(buf);
    let n = batch.len();
    let verified = tokio::task::spawn_blocking(move || verify_batch(batch))
        .await
        .unwrap_or_default();
    let dropped = n - verified.len();
    if dropped > 0 {
        metrics::counter!("n42_mobile_receipt_sig_invalid").increment(dropped as u64);
    }
    for receipt in verified {
        if let Err(e) = event_tx.send(HubEvent::ReceiptReceived(receipt)).await {
            tracing::warn!(error = %e, "hub event channel closed, dropping verified receipts");
            return;
        }
    }
}

/// CPU-bound part: parse pubkeys, batch-verify with per-signature fallback, and
/// return only the receipts whose signatures check out.
fn verify_batch(batch: Vec<Box<VerificationReceipt>>) -> Vec<Box<VerificationReceipt>> {
    // Parse public keys; receipts with malformed keys are invalid outright.
    let mut parsed: Vec<(Box<VerificationReceipt>, BlsPublicKey, Vec<u8>)> =
        Vec::with_capacity(batch.len());
    for receipt in batch {
        match BlsPublicKey::from_bytes(&receipt.verifier_pubkey) {
            Ok(pk) => {
                let msg = receipt.signing_message();
                parsed.push((receipt, pk, msg));
            }
            Err(_) => {
                tracing::warn!(
                    block_number = receipt.block_number,
                    "receipt has malformed BLS pubkey, dropping"
                );
            }
        }
    }
    if parsed.is_empty() {
        return Vec::new();
    }

    let msgs: Vec<&[u8]> = parsed.iter().map(|(_, _, m)| m.as_slice()).collect();
    let sigs: Vec<_> = parsed.iter().map(|(r, _, _)| &r.signature).collect();
    let pks: Vec<_> = parsed.iter().map(|(_, pk, _)| pk).collect();

    match batch_verify_with_fallback(&msgs, &sigs, &pks) {
        Ok(()) => parsed.into_iter().map(|(r, _, _)| r).collect(),
        Err(bad) => {
            let bad: std::collections::HashSet<usize> = bad.into_iter().collect();
            parsed
                .into_iter()
                .enumerate()
                .filter_map(|(i, (r, _, _))| {
                    if bad.contains(&i) {
                        tracing::warn!(
                            block_number = r.block_number,
                            "receipt BLS signature invalid, dropping"
                        );
                        None
                    } else {
                        Some(r)
                    }
                })
                .collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::B256;
    use n42_mobile::receipt::sign_receipt;
    use n42_primitives::BlsSecretKey;

    fn receipt(key: &BlsSecretKey, block: u64) -> Box<VerificationReceipt> {
        Box::new(sign_receipt(
            B256::repeat_byte(0xAB),
            block,
            B256::repeat_byte(0xCD),
            12345,
            key,
        ))
    }

    #[test]
    fn batch_keeps_valid_drops_invalid() {
        let keys: Vec<BlsSecretKey> = (0..5).map(|_| BlsSecretKey::random().unwrap()).collect();
        let mut batch: Vec<Box<VerificationReceipt>> =
            keys.iter().map(|k| receipt(k, 7)).collect();
        // Corrupt one receipt's message binding (signature no longer matches).
        batch[2].block_number = 8;
        let verified = verify_batch(batch);
        assert_eq!(verified.len(), 4, "the tampered receipt must be dropped");
        assert!(verified.iter().all(|r| r.block_number == 7));
    }

    #[test]
    fn all_valid_pass() {
        let keys: Vec<BlsSecretKey> = (0..8).map(|_| BlsSecretKey::random().unwrap()).collect();
        let batch: Vec<Box<VerificationReceipt>> = keys.iter().map(|k| receipt(k, 1)).collect();
        assert_eq!(verify_batch(batch).len(), 8);
    }
}
