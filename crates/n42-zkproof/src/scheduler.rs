use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use metrics::{counter, gauge, histogram};
use tokio::sync::Semaphore;
use tracing::{info, warn};

use crate::prover::{BlockExecutionInput, ZkProver};
use crate::store::ProofStore;

/// Default maximum number of concurrent proof generation tasks.
/// Override via `N42_ZK_MAX_CONCURRENT` environment variable.
const DEFAULT_MAX_CONCURRENT_PROOFS: usize = 8;

/// Read max concurrent proofs from env, falling back to the default.
fn max_concurrent_proofs() -> usize {
    std::env::var("N42_ZK_MAX_CONCURRENT")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .map(|v| v.max(1))
        .unwrap_or(DEFAULT_MAX_CONCURRENT_PROOFS)
}

/// Optional callback invoked after a proof is successfully generated.
/// Used to update SharedConsensusState or other external state.
pub type ProofCallback = Arc<dyn Fn(u64, alloy_primitives::B256) + Send + Sync>;

/// Schedules ZK proof generation at configurable block intervals.
///
/// Runs proof generation in `spawn_blocking` to avoid blocking the consensus
/// tokio runtime. Failures are logged and metered but never propagate to the
/// caller — this is a pure sidecar system.
pub struct ProofScheduler {
    prover: Arc<dyn ZkProver>,
    proof_interval: u64,
    proof_store: Arc<ProofStore>,
    on_proof_generated: Option<ProofCallback>,
    concurrency_limit: Arc<Semaphore>,
    in_progress: Arc<Mutex<HashSet<u64>>>,
}

impl ProofScheduler {
    pub fn new(
        prover: Arc<dyn ZkProver>,
        proof_interval: u64,
        proof_store: Arc<ProofStore>,
    ) -> Self {
        let interval = if proof_interval == 0 { 300 } else { proof_interval };
        let max_concurrent = max_concurrent_proofs();
        gauge!("n42_zk_proof_interval").set(interval as f64);
        info!(
            target: "n42::zk",
            backend = prover.name(),
            interval,
            max_concurrent,
            "ZK proof scheduler initialized"
        );
        Self {
            prover,
            proof_interval: interval,
            proof_store,
            on_proof_generated: None,
            concurrency_limit: Arc::new(Semaphore::new(max_concurrent)),
            in_progress: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Sets a callback invoked after each successful proof generation.
    /// Typically used to update `SharedConsensusState.zk_latest_proof`.
    pub fn with_callback(mut self, cb: ProofCallback) -> Self {
        self.on_proof_generated = Some(cb);
        self
    }

    /// Called by the orchestrator after each block commit.
    ///
    /// Only triggers proof generation when `block_number % proof_interval == 0`.
    /// The proof is generated asynchronously via `spawn_blocking` and stored
    /// in the `ProofStore` on success.
    pub fn on_block_committed(&self, block_number: u64, input: BlockExecutionInput) {
        // Skip block 0 (genesis) and non-interval blocks.
        if block_number == 0 || !block_number.is_multiple_of(self.proof_interval) {
            return;
        }

        // Skip if proof already exists for this block.
        if self.proof_store.contains(block_number) {
            return;
        }

        // Skip if proof generation is already in progress for this block.
        {
            let mut in_prog = self.in_progress.lock().unwrap_or_else(|e| e.into_inner());
            if !in_prog.insert(block_number) {
                return;
            }
        }

        // Acquire semaphore permit BEFORE spawning to accurately limit concurrency.
        // If all slots are busy, skip this block rather than queueing unboundedly.
        let permit = match self.concurrency_limit.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                self.remove_in_progress(block_number);
                warn!(
                    target: "n42::zk",
                    block_number,
                    "skipping ZK proof: max concurrent proofs reached"
                );
                return;
            }
        };

        let prover = Arc::clone(&self.prover);
        let store = Arc::clone(&self.proof_store);
        let callback = self.on_proof_generated.clone();
        let in_progress = Arc::clone(&self.in_progress);

        tokio::task::spawn_blocking(move || {
            let _permit = permit; // Hold permit until task completes.
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
                    let block_hash = result.block_hash;
                    store.insert(result);
                    if let Some(cb) = callback {
                        cb(block_number, block_hash);
                    }
                }
                Err(e) => {
                    counter!("n42_zk_proof_failed_total").increment(1);
                    store.record_failure();
                    warn!(
                        target: "n42::zk",
                        block_number,
                        error = %e,
                        "ZK proof generation failed (non-critical)"
                    );
                }
            }
            // Remove from in-progress set.
            in_progress.lock().unwrap_or_else(|e| e.into_inner()).remove(&block_number);
        });
    }

    fn remove_in_progress(&self, block_number: u64) {
        self.in_progress.lock().unwrap_or_else(|e| e.into_inner()).remove(&block_number);
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

    /// Returns a reference to the prover for external re-verification.
    pub fn prover(&self) -> &Arc<dyn ZkProver> {
        &self.prover
    }

    /// Returns the number of blocks currently being proved.
    pub fn in_progress_count(&self) -> usize {
        self.in_progress.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ZkProofError;
    use crate::prover::{MockProver, ProofType, ZkProofResult};
    use std::sync::atomic::{AtomicU64, Ordering};

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

    /// A prover that always fails, for testing scheduler error handling.
    struct FailingProver;
    impl ZkProver for FailingProver {
        fn name(&self) -> &str { "failing" }
        fn prove(&self, _input: &BlockExecutionInput) -> Result<ZkProofResult, ZkProofError> {
            Err(ZkProofError::Prover("intentional test failure".to_string()))
        }
        fn verify(&self, _result: &ZkProofResult) -> Result<bool, ZkProofError> {
            Err(ZkProofError::Verification("intentional test failure".to_string()))
        }
    }

    /// A prover that counts how many times prove() is called.
    struct CountingProver {
        call_count: AtomicU64,
    }
    impl CountingProver {
        fn new() -> Self { Self { call_count: AtomicU64::new(0) } }
        fn count(&self) -> u64 { self.call_count.load(Ordering::SeqCst) }
    }
    impl ZkProver for CountingProver {
        fn name(&self) -> &str { "counting" }
        fn prove(&self, input: &BlockExecutionInput) -> Result<ZkProofResult, ZkProofError> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            // Delegate to MockProver for actual result.
            MockProver::new().prove(input)
        }
        fn verify(&self, result: &ZkProofResult) -> Result<bool, ZkProofError> {
            MockProver::new().verify(result)
        }
    }

    // --- Basic interval logic ---

    #[tokio::test]
    async fn test_scheduler_generates_proof_at_interval() {
        let prover = Arc::new(MockProver::new());
        let store = Arc::new(ProofStore::new(100));
        let scheduler = ProofScheduler::new(prover, 5, Arc::clone(&store));

        scheduler.on_block_committed(5, make_input(5));
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let proof = store.get_by_block(5).expect("proof should be generated for block 5");
        assert_eq!(proof.block_number, 5);
        assert_eq!(proof.proof_type, ProofType::Mock);
    }

    #[tokio::test]
    async fn test_scheduler_skips_non_interval_blocks() {
        let prover = Arc::new(MockProver::new());
        let store = Arc::new(ProofStore::new(100));
        let scheduler = ProofScheduler::new(prover, 10, Arc::clone(&store));

        scheduler.on_block_committed(5, make_input(5));
        scheduler.on_block_committed(7, make_input(7));
        scheduler.on_block_committed(10, make_input(10));
        scheduler.on_block_committed(20, make_input(20));

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        assert!(store.get_by_block(5).is_none());
        assert!(store.get_by_block(7).is_none());
        assert!(store.get_by_block(10).is_some());
        assert!(store.get_by_block(20).is_some());
    }

    #[tokio::test]
    async fn test_scheduler_does_not_generate_off_interval() {
        let prover = Arc::new(MockProver::new());
        let store = Arc::new(ProofStore::new(100));
        let scheduler = ProofScheduler::new(prover, 10, Arc::clone(&store));

        scheduler.on_block_committed(3, make_input(3));
        scheduler.on_block_committed(7, make_input(7));
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        assert!(store.is_empty());
    }

    // --- Block 0 (genesis) should be skipped ---

    #[tokio::test]
    async fn test_scheduler_skips_block_zero() {
        let counting = Arc::new(CountingProver::new());
        let store = Arc::new(ProofStore::new(100));
        let scheduler = ProofScheduler::new(counting.clone(), 10, Arc::clone(&store));

        // Block 0: 0 % 10 == 0, but should be skipped (genesis).
        scheduler.on_block_committed(0, make_input(0));
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        assert_eq!(counting.count(), 0, "block 0 should not trigger proof");
        assert!(store.is_empty());
    }

    // --- Zero interval defaults to 300 ---

    #[test]
    fn test_zero_interval_defaults() {
        let prover = Arc::new(MockProver::new());
        let store = Arc::new(ProofStore::new(100));
        let scheduler = ProofScheduler::new(prover, 0, store);

        assert_eq!(scheduler.proof_interval(), 300);
    }

    // --- Callback invocation ---

    #[tokio::test]
    async fn test_callback_invoked_on_success() {
        let prover = Arc::new(MockProver::new());
        let store = Arc::new(ProofStore::new(100));
        let callback_count = Arc::new(AtomicU64::new(0));
        let callback_block = Arc::new(AtomicU64::new(0));

        let cc = callback_count.clone();
        let cb = callback_block.clone();
        let callback: ProofCallback = Arc::new(move |block_number, _hash| {
            cc.fetch_add(1, Ordering::SeqCst);
            cb.store(block_number, Ordering::SeqCst);
        });

        let scheduler = ProofScheduler::new(prover, 10, store).with_callback(callback);

        scheduler.on_block_committed(10, make_input(10));
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

        assert_eq!(callback_count.load(Ordering::SeqCst), 1);
        assert_eq!(callback_block.load(Ordering::SeqCst), 10);
    }

    #[tokio::test]
    async fn test_callback_not_invoked_on_failure() {
        let prover: Arc<dyn ZkProver> = Arc::new(FailingProver);
        let store = Arc::new(ProofStore::new(100));
        let callback_count = Arc::new(AtomicU64::new(0));

        let cc = callback_count.clone();
        let callback: ProofCallback = Arc::new(move |_, _| {
            cc.fetch_add(1, Ordering::SeqCst);
        });

        let scheduler = ProofScheduler::new(prover, 10, Arc::clone(&store)).with_callback(callback);

        scheduler.on_block_committed(10, make_input(10));
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

        assert_eq!(callback_count.load(Ordering::SeqCst), 0, "callback should not fire on failure");
        assert!(store.is_empty(), "store should be empty on failure");
    }

    // --- Failing prover doesn't crash scheduler ---

    #[tokio::test]
    async fn test_failing_prover_does_not_crash() {
        let prover: Arc<dyn ZkProver> = Arc::new(FailingProver);
        let store = Arc::new(ProofStore::new(100));
        let scheduler = ProofScheduler::new(prover, 10, Arc::clone(&store));

        // This should not panic.
        scheduler.on_block_committed(10, make_input(10));
        scheduler.on_block_committed(20, make_input(20));
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

        assert!(store.is_empty());
    }

    // --- Accessor methods ---

    #[test]
    fn test_proof_interval_accessor() {
        let prover = Arc::new(MockProver::new());
        let store = Arc::new(ProofStore::new(100));
        let scheduler = ProofScheduler::new(prover, 42, store);
        assert_eq!(scheduler.proof_interval(), 42);
    }

    #[test]
    fn test_backend_name_accessor() {
        let prover = Arc::new(MockProver::new());
        let store = Arc::new(ProofStore::new(100));
        let scheduler = ProofScheduler::new(prover, 10, store);
        assert_eq!(scheduler.backend_name(), "mock");
    }

    #[test]
    fn test_proof_store_accessor() {
        let prover = Arc::new(MockProver::new());
        let store = Arc::new(ProofStore::new(100));
        let scheduler = ProofScheduler::new(prover, 10, Arc::clone(&store));
        assert!(scheduler.proof_store().is_empty());
    }

    // --- Multiple proofs ---

    #[tokio::test]
    async fn test_multiple_interval_proofs() {
        let counting = Arc::new(CountingProver::new());
        let store = Arc::new(ProofStore::new(100));
        let scheduler = ProofScheduler::new(counting.clone(), 5, Arc::clone(&store));

        for block in [5, 10, 15, 20, 25] {
            scheduler.on_block_committed(block, make_input(block));
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

        assert_eq!(counting.count(), 5);
        assert_eq!(store.len(), 5);
        for block in [5, 10, 15, 20, 25] {
            assert!(store.contains(block), "missing proof for block {block}");
        }
    }

    // --- Interval 1 generates proof for every block except 0 ---

    #[tokio::test]
    async fn test_interval_one() {
        let counting = Arc::new(CountingProver::new());
        let store = Arc::new(ProofStore::new(100));
        let scheduler = ProofScheduler::new(counting.clone(), 1, Arc::clone(&store));

        scheduler.on_block_committed(0, make_input(0)); // Skipped (genesis)
        scheduler.on_block_committed(1, make_input(1));
        scheduler.on_block_committed(2, make_input(2));
        scheduler.on_block_committed(3, make_input(3));
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

        assert_eq!(counting.count(), 3, "block 0 skipped, 1/2/3 should generate");
        assert!(!store.contains(0));
        assert!(store.contains(1));
        assert!(store.contains(2));
        assert!(store.contains(3));
    }

    // --- Duplicate prevention ---

    #[tokio::test]
    async fn test_duplicate_block_skipped() {
        let counting = Arc::new(CountingProver::new());
        let store = Arc::new(ProofStore::new(100));
        let scheduler = ProofScheduler::new(counting.clone(), 10, Arc::clone(&store));

        scheduler.on_block_committed(10, make_input(10));
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

        // Submit the same block again — should be skipped because proof already exists.
        scheduler.on_block_committed(10, make_input(10));
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        assert_eq!(counting.count(), 1, "second submit should be skipped");
    }

    // --- Prover accessor ---

    #[test]
    fn test_prover_accessor() {
        let prover = Arc::new(MockProver::new());
        let store = Arc::new(ProofStore::new(100));
        let scheduler = ProofScheduler::new(prover, 10, store);
        assert_eq!(scheduler.prover().name(), "mock");
    }

    // --- In-progress count ---

    #[test]
    fn test_in_progress_count_initial() {
        let prover = Arc::new(MockProver::new());
        let store = Arc::new(ProofStore::new(100));
        let scheduler = ProofScheduler::new(prover, 10, store);
        assert_eq!(scheduler.in_progress_count(), 0);
    }

    // --- Stats via store ---

    #[tokio::test]
    async fn test_failure_recorded_in_stats() {
        let prover: Arc<dyn ZkProver> = Arc::new(FailingProver);
        let store = Arc::new(ProofStore::new(100));
        let scheduler = ProofScheduler::new(prover, 10, Arc::clone(&store));

        scheduler.on_block_committed(10, make_input(10));
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

        let stats = store.stats();
        assert_eq!(stats.generated, 0);
        assert_eq!(stats.failed, 1);
    }

    #[tokio::test]
    async fn test_success_recorded_in_stats() {
        let prover = Arc::new(MockProver::new());
        let store = Arc::new(ProofStore::new(100));
        let scheduler = ProofScheduler::new(prover, 5, Arc::clone(&store));

        scheduler.on_block_committed(5, make_input(5));
        scheduler.on_block_committed(10, make_input(10));
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

        let stats = store.stats();
        assert_eq!(stats.generated, 2);
        assert_eq!(stats.failed, 0);
    }
}
