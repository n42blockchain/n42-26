use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use metrics::{counter, gauge, histogram};
use tracing::{info, warn};

use crate::prover::{BlockExecutionInput, ZkProver};
use crate::store::ProofStore;

/// Schedules ZK proof generation at configurable block intervals.
///
/// Runs proof generation in `spawn_blocking` to avoid blocking the consensus
/// tokio runtime. Failures are logged and metered but never propagate to the
/// caller — this is a pure sidecar system.
pub struct ProofScheduler {
    prover: Arc<dyn ZkProver>,
    proof_interval: u64,
    proof_store: Arc<ProofStore>,
    proofs_generated: AtomicU64,
    proofs_failed: AtomicU64,
}

impl ProofScheduler {
    pub fn new(
        prover: Arc<dyn ZkProver>,
        proof_interval: u64,
        proof_store: Arc<ProofStore>,
    ) -> Self {
        let interval = if proof_interval == 0 { 300 } else { proof_interval };
        gauge!("n42_zk_proof_interval").set(interval as f64);
        info!(
            target: "n42::zk",
            backend = prover.name(),
            interval,
            "ZK proof scheduler initialized"
        );
        Self {
            prover,
            proof_interval: interval,
            proof_store,
            proofs_generated: AtomicU64::new(0),
            proofs_failed: AtomicU64::new(0),
        }
    }

    /// Called by the orchestrator after each block commit.
    ///
    /// Only triggers proof generation when `block_number % proof_interval == 0`.
    /// The proof is generated asynchronously via `spawn_blocking` and stored
    /// in the `ProofStore` on success.
    pub fn on_block_committed(&self, block_number: u64, input: BlockExecutionInput) {
        if !block_number.is_multiple_of(self.proof_interval) {
            return;
        }

        let prover = Arc::clone(&self.prover);
        let store = Arc::clone(&self.proof_store);

        tokio::task::spawn_blocking(move || {
            info!(target: "n42::zk", block_number, "starting ZK proof generation");
            let start = std::time::Instant::now();
            match prover.prove(&input) {
                Ok(result) => {
                    let ms = start.elapsed().as_millis() as u64;
                    counter!("n42_zk_proof_generated_total").increment(1);
                    histogram!("n42_zk_proof_generation_ms").record(ms as f64);
                    info!(
                        target: "n42::zk",
                        block_number,
                        ms,
                        proof_type = %result.proof_type,
                        proof_size = result.proof_bytes.len(),
                        verified = result.verified,
                        "ZK proof generated"
                    );
                    store.insert(result);
                }
                Err(e) => {
                    counter!("n42_zk_proof_failed_total").increment(1);
                    warn!(
                        target: "n42::zk",
                        block_number,
                        error = %e,
                        "ZK proof generation failed (non-critical)"
                    );
                }
            }
        });

        // Update local counters (approximate — the spawn_blocking hasn't
        // finished yet, but we track the *attempt* count).
        self.proofs_generated.fetch_add(1, Ordering::Relaxed);
    }

    /// Returns the configured proof interval.
    pub fn proof_interval(&self) -> u64 {
        self.proof_interval
    }

    /// Returns the proof store for RPC queries.
    pub fn proof_store(&self) -> &Arc<ProofStore> {
        &self.proof_store
    }

    /// Returns the name of the prover backend.
    pub fn backend_name(&self) -> &str {
        self.prover.name()
    }

    /// Returns (attempts, failures) counts.
    pub fn stats(&self) -> (u64, u64) {
        (
            self.proofs_generated.load(Ordering::Relaxed),
            self.proofs_failed.load(Ordering::Relaxed),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prover::MockProver;

    fn make_input(block_number: u64) -> BlockExecutionInput {
        let mut hash_bytes = [0u8; 32];
        hash_bytes[..8].copy_from_slice(&block_number.to_le_bytes());
        BlockExecutionInput {
            block_hash: alloy_primitives::B256::from(hash_bytes),
            block_number,
            parent_hash: alloy_primitives::B256::ZERO,
            header_rlp: vec![],
            transactions_rlp: vec![],
            bundle_state_json: vec![],
            parent_state_root: alloy_primitives::B256::ZERO,
        }
    }

    #[tokio::test]
    async fn test_scheduler_skips_non_interval_blocks() {
        let prover = Arc::new(MockProver::new());
        let store = Arc::new(ProofStore::new(100));
        let scheduler = ProofScheduler::new(prover, 10, Arc::clone(&store));

        // Non-interval blocks should not trigger proof generation.
        scheduler.on_block_committed(5, make_input(5));
        scheduler.on_block_committed(7, make_input(7));

        // Interval-aligned blocks should trigger.
        scheduler.on_block_committed(10, make_input(10));
        scheduler.on_block_committed(20, make_input(20));

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        assert!(store.get_by_block(5).is_none());
        assert!(store.get_by_block(7).is_none());
        assert!(store.get_by_block(10).is_some());
        assert!(store.get_by_block(20).is_some());
    }

    #[tokio::test]
    async fn test_scheduler_generates_proof_at_interval() {
        let prover = Arc::new(MockProver::new());
        let store = Arc::new(ProofStore::new(100));
        let scheduler = ProofScheduler::new(prover, 5, Arc::clone(&store));

        // Block 5 is interval-aligned.
        scheduler.on_block_committed(5, make_input(5));

        // Wait for spawn_blocking to complete.
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let proof = store.get_by_block(5);
        assert!(proof.is_some(), "proof should be generated for block 5");
        assert_eq!(proof.unwrap().block_number, 5);
    }

    #[tokio::test]
    async fn test_scheduler_does_not_generate_off_interval() {
        let prover = Arc::new(MockProver::new());
        let store = Arc::new(ProofStore::new(100));
        let scheduler = ProofScheduler::new(prover, 10, Arc::clone(&store));

        scheduler.on_block_committed(3, make_input(3));
        scheduler.on_block_committed(7, make_input(7));

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        assert!(store.is_empty(), "no proofs should be generated for non-interval blocks");
    }
}
