//! Sender verification ahead of the engine (range catch-up Stage 2,
//! `docs/devlog-195`).
//!
//! A range catch-up imports blocks whose transactions this node never
//! admitted, so the engine's payload conversion misses both sender caches
//! and recovers every sender itself, on its own CPU pool, while execution
//! waits on the first transactions. [`SenderWarmup`] verifies the senders of
//! blocks the catch-up has queued for the engine, on a small pool of its own,
//! and records them in the caches that conversion reads:
//!
//! * secp256k1: reth's shared [`SenderRecoveryCache`], through its own
//!   recover-or-insert (`SignedTransaction::try_recover`, low-s and every
//!   other rule unchanged);
//! * Ed25519 (0x50): [`AltSigSenderCache::global`], through
//!   [`n42_tx_types::verify_batch`], whose verdicts are those of a single
//!   `verify` per transaction.
//!
//! A failure is never cached: the engine meets that transaction as a miss,
//! verifies it again, and rejects the block exactly as before. The caller
//! bounds how far ahead of the engine it warms, so a long reply never pushes
//! out the entries the engine is about to read.

pub use reth_evm::SenderRecoveryCache;

use n42_tx_types::{
    AltSigSenderCache, AltSigTx, N42TxEnvelope, alt_sig_enabled, ed25519_batch_size,
};
use rayon::prelude::*;
use reth_primitives_traits::SignedTransaction;
use std::sync::OnceLock;

/// What one [`SenderWarmup::warm`] call did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WarmStats {
    /// Transactions looked at.
    pub transactions: usize,
    /// Already in a cache: nothing verified.
    pub cached: usize,
    /// Verified here and recorded.
    pub recovered: usize,
    /// Failed verification here: not recorded (the engine decides).
    pub failed: usize,
    /// Not handled here (no cache for its kind, or 0x50 off on this chain).
    pub skipped: usize,
}

impl WarmStats {
    fn add(mut self, other: Self) -> Self {
        self.transactions += other.transactions;
        self.cached += other.cached;
        self.recovered += other.recovered;
        self.failed += other.failed;
        self.skipped += other.skipped;
        self
    }

    fn one(outcome: Outcome) -> Self {
        let mut stats = Self {
            transactions: 1,
            ..Self::default()
        };
        match outcome {
            Outcome::Cached => stats.cached = 1,
            Outcome::Recovered => stats.recovered = 1,
            Outcome::Failed => stats.failed = 1,
            Outcome::Skipped => stats.skipped = 1,
        }
        stats
    }
}

#[derive(Clone, Copy)]
enum Outcome {
    Cached,
    Recovered,
    Failed,
    Skipped,
}

/// Verifies transaction senders into the engine's caches on a dedicated
/// pool, so the work never queues on the engine's own rayon pool.
pub struct SenderWarmup {
    cache: Option<SenderRecoveryCache>,
    pool: rayon::ThreadPool,
}

impl std::fmt::Debug for SenderWarmup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SenderWarmup")
            .field("threads", &self.pool.current_num_threads())
            .field("secp256k1_cache", &self.cache.is_some())
            .finish()
    }
}

/// Default pool size: a quarter of the cores, between one and four.
fn default_threads() -> usize {
    std::thread::available_parallelism()
        .map_or(2, |cores| cores.get() / 4)
        .clamp(1, 4)
}

impl SenderWarmup {
    /// A warm-up over reth's shared secp256k1 cache (`None`: secp256k1
    /// senders are left to the engine) with a pool of `threads` threads.
    pub fn new(
        cache: Option<SenderRecoveryCache>,
        threads: usize,
    ) -> Result<Self, rayon::ThreadPoolBuildError> {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads.max(1))
            .thread_name(|index| format!("n42-sender-ahead-{index}"))
            .build()?;
        Ok(Self { cache, pool })
    }

    /// `N42_RANGE_SENDER_THREADS` (default a quarter of the cores, one to
    /// four; at most 16), read once.
    pub fn threads_from_env() -> usize {
        static THREADS: OnceLock<usize> = OnceLock::new();
        *THREADS.get_or_init(|| {
            std::env::var("N42_RANGE_SENDER_THREADS")
                .ok()
                .and_then(|value| value.parse::<usize>().ok())
                .filter(|threads| *threads > 0)
                .unwrap_or_else(default_threads)
                .min(16)
        })
    }

    /// Verifies the senders of `transactions` not cached yet and records
    /// those that verify. Runs on the warm-up pool; blocks the caller.
    pub fn warm(&self, transactions: &[N42TxEnvelope]) -> WarmStats {
        self.pool.install(|| {
            let eth = transactions
                .par_iter()
                .with_min_len(32)
                .filter(|tx| matches!(tx, N42TxEnvelope::Eth(_)))
                .map(|tx| WarmStats::one(self.warm_secp256k1(tx)))
                .reduce(WarmStats::default, WarmStats::add);
            eth.add(self.warm_ed25519(transactions))
        })
    }

    fn warm_secp256k1(&self, tx: &N42TxEnvelope) -> Outcome {
        let Some(cache) = self.cache.as_ref() else {
            return Outcome::Skipped;
        };
        // The closure runs only on a miss; a failure is not inserted.
        let mut verified = false;
        match cache.recover_with(tx, |tx| {
            verified = true;
            tx.try_recover()
        }) {
            Ok(_) if verified => Outcome::Recovered,
            Ok(_) => Outcome::Cached,
            Err(_) => Outcome::Failed,
        }
    }

    fn warm_ed25519(&self, transactions: &[N42TxEnvelope]) -> WarmStats {
        let alt: Vec<&AltSigTx> = transactions
            .iter()
            .filter_map(|tx| match tx {
                N42TxEnvelope::AltSig(alt) => Some(alt),
                N42TxEnvelope::Eth(_) => None,
            })
            .collect();
        if alt.is_empty() {
            return WarmStats::default();
        }
        if !alt_sig_enabled() {
            // The engine refuses such a block whatever its senders.
            return WarmStats {
                transactions: alt.len(),
                skipped: alt.len(),
                ..WarmStats::default()
            };
        }
        let cache = AltSigSenderCache::global();
        let pending: Vec<&AltSigTx> = alt
            .iter()
            .copied()
            .filter(|tx| cache.get(tx.hash()).is_none())
            .collect();
        let cached = alt.len() - pending.len();
        let verified = pending
            .par_chunks(ed25519_batch_size())
            .map(|chunk| {
                let mut stats = WarmStats::default();
                for (tx, verdict) in chunk.iter().zip(n42_tx_types::verify_batch(chunk)) {
                    match verdict {
                        Ok(sender) => {
                            cache.insert(*tx.hash(), sender);
                            stats.recovered += 1;
                        }
                        Err(_) => stats.failed += 1,
                    }
                }
                stats
            })
            .reduce(WarmStats::default, WarmStats::add);
        WarmStats {
            transactions: alt.len(),
            cached,
            ..verified
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::TxLegacy;
    use alloy_consensus::transaction::TxHashRef as _;
    use alloy_primitives::{Signature, U256};
    use reth_ethereum_primitives::{Transaction, TransactionSigned};

    fn legacy(nonce: u64, signature: Signature) -> N42TxEnvelope {
        N42TxEnvelope::Eth(TransactionSigned::new_unhashed(
            Transaction::Legacy(TxLegacy {
                nonce,
                ..Default::default()
            }),
            signature,
        ))
    }

    #[test]
    fn warms_valid_senders_and_never_caches_a_failure() {
        // Direct-mapped: entries may displace each other, so the checks
        // below hold for what the cache keeps.
        let cache = SenderRecoveryCache::new(1 << 16);
        let warmup = SenderWarmup::new(Some(cache.clone()), 2).expect("pool");
        let bad = legacy(99, Signature::new(U256::ZERO, U256::ZERO, false));
        // High s: `try_recover` refuses it, so the warm-up must too.
        let test = Signature::test_signature();
        let high_s = Signature::new(
            test.r(),
            alloy_primitives::U256::from_be_slice(&secp256k1_order()) - test.s(),
            !test.v(),
        );
        let malleable = legacy(98, high_s);
        assert!(
            malleable.try_recover().is_err(),
            "the rule warm-up must keep"
        );
        let mut txs: Vec<_> = (0..40).map(|nonce| legacy(nonce, test)).collect();
        txs.push(bad.clone());
        txs.push(malleable.clone());

        let stats = warmup.warm(&txs);
        assert_eq!(stats.transactions, 42);
        assert_eq!(stats.recovered, 40);
        assert_eq!(stats.failed, 2);
        let mut held = 0;
        for tx in &txs[..40] {
            if let Some(sender) = cache.get(tx.tx_hash()) {
                assert_eq!(sender, tx.try_recover().unwrap());
                held += 1;
            }
        }
        assert!(held >= 38, "held {held}");
        assert_eq!(cache.get(bad.tx_hash()), None);
        assert_eq!(cache.get(malleable.tx_hash()), None);

        let again = warmup.warm(&txs);
        assert!(again.cached >= 38, "{again:?}");
        assert_eq!((again.cached + again.recovered, again.failed), (40, 2));
    }

    fn secp256k1_order() -> [u8; 32] {
        alloy_primitives::hex!("fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141")
    }

    #[test]
    fn warms_ed25519_senders_in_batches_without_caching_a_bad_one() {
        n42_tx_types::set_alt_sig_enabled(true);
        let key = ed25519_dalek::SigningKey::from_bytes(&[42; 32]);
        let fields = |nonce: u64| n42_tx_types::TxAltSig {
            chain_id: 1144,
            nonce,
            gas_limit: 21_000,
            alg_type: n42_tx_types::ALG_ED25519,
            pubkey: alloy_primitives::Bytes::copy_from_slice(key.verifying_key().as_bytes()),
            ..Default::default()
        };
        let mut txs: Vec<N42TxEnvelope> = (0..10u64)
            .map(|nonce| N42TxEnvelope::AltSig(fields(7_000_000 + nonce).sign_ed25519(&key)))
            .collect();
        // Signed by another key under this key's name: verifies for nobody.
        let other = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);
        let forged = N42TxEnvelope::AltSig(fields(7_100_000).sign_ed25519(&other));
        txs.push(forged.clone());

        let warmup = SenderWarmup::new(None, 2).expect("pool");
        let stats = warmup.warm(&txs);
        assert_eq!(
            (stats.transactions, stats.recovered, stats.failed),
            (11, 10, 1)
        );
        let cache = AltSigSenderCache::global();
        for tx in &txs[..10] {
            let N42TxEnvelope::AltSig(alt) = tx else {
                unreachable!()
            };
            assert_eq!(cache.get(alt.hash()), Some(alt.verify().unwrap()));
        }
        let N42TxEnvelope::AltSig(forged) = &forged else {
            unreachable!()
        };
        assert_eq!(cache.get(forged.hash()), None);
    }
}
