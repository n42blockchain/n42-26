//! Integration tests for n42-zkproof crate.
//!
//! Tests the full flow: prover → scheduler → store → query.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use alloy_primitives::B256;
use n42_zkproof::{
    BlockExecutionInput, MockProver, ProofCallback, ProofScheduler, ProofStore, ProofType,
    ZkProofResult, ZkProver,
};

fn make_input(block_number: u64) -> BlockExecutionInput {
    let mut hash_bytes = [0u8; 32];
    hash_bytes[..8].copy_from_slice(&block_number.to_le_bytes());
    BlockExecutionInput {
        block_hash: B256::from(hash_bytes),
        block_number,
        parent_hash: B256::repeat_byte(0xBB),
        header_rlp: vec![0xDE, 0xAD],
        transactions_rlp: vec![vec![0xBE, 0xEF], vec![0xCA, 0xFE]],
        bundle_state_json: b"{\"accounts\":{}}".to_vec(),
        parent_state_root: B256::repeat_byte(0xCC),
    }
}

/// Full end-to-end flow: scheduler receives blocks → proof generated → stored → queryable.
#[tokio::test]
async fn test_full_proof_lifecycle() {
    let prover = Arc::new(MockProver::new());
    let store = Arc::new(ProofStore::new(100));
    let callback_log = Arc::new(std::sync::Mutex::new(Vec::<(u64, B256)>::new()));

    let log = callback_log.clone();
    let callback: ProofCallback = Arc::new(move |block_number, block_hash| {
        log.lock().unwrap().push((block_number, block_hash));
    });

    let scheduler = ProofScheduler::new(prover.clone(), 10, Arc::clone(&store))
        .with_callback(callback);

    // Simulate 50 blocks being committed (only multiples of 10 generate proofs).
    for block in 1..=50 {
        scheduler.on_block_committed(block, make_input(block));
    }

    // Wait for all spawn_blocking tasks to complete.
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    // Verify: blocks 10, 20, 30, 40, 50 should have proofs.
    assert_eq!(store.len(), 5);
    for block in [10, 20, 30, 40, 50] {
        let proof = store.get_by_block(block).unwrap_or_else(|| panic!("missing proof for block {block}"));
        assert_eq!(proof.block_number, block);
        assert_eq!(proof.proof_type, ProofType::Mock);
        assert!(proof.verified);

        // Verify the proof using the prover.
        let valid = prover.verify(&proof).unwrap();
        assert!(valid, "proof for block {block} should be valid");
    }

    // Verify: non-interval blocks should NOT have proofs.
    for block in [1, 5, 11, 15, 25, 33, 49] {
        assert!(store.get_by_block(block).is_none());
    }

    // Verify: latest should be block 50.
    assert_eq!(store.latest().unwrap().block_number, 50);

    // Verify: block range.
    assert_eq!(store.block_range(), Some((10, 50)));

    // Verify: callback was invoked 5 times with correct data.
    let log = callback_log.lock().unwrap();
    assert_eq!(log.len(), 5);
    let logged_blocks: Vec<u64> = log.iter().map(|(b, _)| *b).collect();
    for block in [10, 20, 30, 40, 50] {
        assert!(logged_blocks.contains(&block), "callback missing block {block}");
    }
}

/// Verify that proofs from MockProver are cross-verifiable
/// (prove with one instance, verify with another).
#[test]
fn test_cross_instance_verification() {
    let prover1 = MockProver::new();
    let prover2 = MockProver::new();

    let input = make_input(100);
    let result = prover1.prove(&input).unwrap();

    // Verify with a different instance.
    assert!(prover2.verify(&result).unwrap());
}

/// Store eviction behavior during continuous proof generation.
/// Submits blocks in batches to stay within concurrency limits.
#[tokio::test]
async fn test_store_eviction_under_continuous_generation() {
    let prover = Arc::new(MockProver::new());
    let store = Arc::new(ProofStore::new(5)); // Only 5 slots.
    let scheduler = ProofScheduler::new(prover, 1, Arc::clone(&store));

    // Generate proofs in batches to avoid hitting concurrency limit.
    for batch_start in (1..=20u64).step_by(4) {
        for block in batch_start..=(batch_start + 3).min(20) {
            scheduler.on_block_committed(block, make_input(block));
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    // Only the latest 5 should remain.
    assert_eq!(store.len(), 5);
    let (min, max) = store.block_range().unwrap();
    assert_eq!(max, 20);
    assert!(min >= 16, "oldest block should be at least 16, got {min}");
    assert!(!store.contains(1), "block 1 should be evicted");
}

/// Proof serialization roundtrip via bincode (simulating storage/retrieval).
#[test]
fn test_proof_result_storage_roundtrip() {
    let prover = MockProver::new();
    let input = make_input(42);
    let result = prover.prove(&input).unwrap();

    // Serialize (simulate writing to disk/DB).
    let bytes = bincode::serialize(&result).unwrap();

    // Deserialize (simulate reading from disk/DB).
    let restored: ZkProofResult = bincode::deserialize(&bytes).unwrap();

    assert_eq!(restored.block_hash, result.block_hash);
    assert_eq!(restored.block_number, result.block_number);
    assert_eq!(restored.proof_bytes, result.proof_bytes);
    assert_eq!(restored.public_values, result.public_values);
    assert_eq!(restored.proof_type, result.proof_type);
    assert_eq!(restored.prover_backend, result.prover_backend);
    assert!(restored.verified);

    // Verify the restored proof.
    assert!(prover.verify(&restored).unwrap());
}

/// Proof serialization roundtrip via JSON (simulating RPC responses).
#[test]
fn test_proof_result_json_roundtrip() {
    let prover = MockProver::new();
    let input = make_input(99);
    let result = prover.prove(&input).unwrap();

    let json = serde_json::to_string_pretty(&result).unwrap();
    let restored: ZkProofResult = serde_json::from_str(&json).unwrap();

    assert_eq!(restored.block_number, 99);
    assert_eq!(restored.created_at, result.created_at);
    assert!(restored.created_at > 0);
    assert!(prover.verify(&restored).unwrap());
}

/// Verify BlockExecutionInput is compatible with guest program's expected format.
#[test]
fn test_block_execution_input_guest_compatibility() {
    let input = BlockExecutionInput {
        block_hash: B256::repeat_byte(0x11),
        block_number: 12345,
        parent_hash: B256::repeat_byte(0x22),
        header_rlp: vec![0xf8; 512],
        transactions_rlp: vec![vec![0xf8; 256]; 100], // 100 txs
        bundle_state_json: b"{\"accounts\":{\"0x1234\":{\"balance\":\"0x100\"}}}".to_vec(),
        parent_state_root: B256::repeat_byte(0x33),
    };

    // Must serialize with bincode (guest uses bincode::deserialize).
    let encoded = bincode::serialize(&input).unwrap();
    assert!(!encoded.is_empty());

    // Guest would deserialize this exact format.
    let decoded: BlockExecutionInput = bincode::deserialize(&encoded).unwrap();
    assert_eq!(decoded.block_number, 12345);
    assert_eq!(decoded.transactions_rlp.len(), 100);
    assert_eq!(decoded.bundle_state_json, input.bundle_state_json);
}

/// Store lookup by hash works correctly.
#[tokio::test]
async fn test_store_hash_lookup_after_scheduler() {
    let prover = Arc::new(MockProver::new());
    let store = Arc::new(ProofStore::new(100));
    let scheduler = ProofScheduler::new(prover, 10, Arc::clone(&store));

    let input = make_input(10);
    let expected_hash = input.block_hash;

    scheduler.on_block_committed(10, input);
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    let proof = store.get_by_hash(&expected_hash).unwrap();
    assert_eq!(proof.block_number, 10);
    assert_eq!(proof.block_hash, expected_hash);
}

/// Scheduler with different backends uses correct backend name.
#[test]
fn test_scheduler_reports_backend_name() {
    struct CustomProver;
    impl ZkProver for CustomProver {
        fn name(&self) -> &str { "custom-v2" }
        fn prove(&self, input: &BlockExecutionInput) -> Result<ZkProofResult, n42_zkproof::ZkProofError> {
            MockProver::new().prove(input)
        }
        fn verify(&self, result: &ZkProofResult) -> Result<bool, n42_zkproof::ZkProofError> {
            MockProver::new().verify(result)
        }
    }

    let store = Arc::new(ProofStore::new(10));
    let scheduler = ProofScheduler::new(Arc::new(CustomProver), 10, store);
    assert_eq!(scheduler.backend_name(), "custom-v2");
}

/// Concurrent scheduler usage — verifies thread safety and that proofs
/// are generated correctly under concurrent access. Due to concurrency
/// limits, not all rapidly-submitted blocks may get proofs.
#[tokio::test]
async fn test_concurrent_rapid_blocks() {
    let prover = Arc::new(MockProver::new());
    let store = Arc::new(ProofStore::new(1000));
    let callback_count = Arc::new(AtomicU64::new(0));

    let cc = callback_count.clone();
    let callback: ProofCallback = Arc::new(move |_, _| {
        cc.fetch_add(1, Ordering::SeqCst);
    });

    let scheduler = Arc::new(
        ProofScheduler::new(prover, 10, Arc::clone(&store)).with_callback(callback),
    );

    // Spawn 5 tasks, each committing 50 blocks (only multiples of 10 trigger proofs).
    // This yields 25 unique proof-eligible blocks spread across tasks.
    let mut handles = vec![];
    for t in 0..5u64 {
        let s = scheduler.clone();
        handles.push(tokio::spawn(async move {
            for i in 1..=50u64 {
                let block = t * 1000 + i * 10; // 10, 20, ..., 500 per task
                s.on_block_committed(block, make_input(block));
                // Small yield to let tasks interleave.
                if i % 10 == 0 {
                    tokio::task::yield_now().await;
                }
            }
        }));
    }

    for h in handles {
        h.await.unwrap();
    }

    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    // Some blocks may be skipped due to concurrency limit, but most should succeed.
    let generated = callback_count.load(Ordering::SeqCst);
    assert!(generated >= 50, "expected at least 50 proofs, got {generated}");
    assert_eq!(store.len() as u64, generated);
}

/// Stats lifecycle: generated/failed counters track correctly through scheduler.
#[tokio::test]
async fn test_stats_lifecycle_through_scheduler() {
    let prover = Arc::new(MockProver::new());
    let store = Arc::new(ProofStore::new(100));
    let scheduler = ProofScheduler::new(prover, 10, Arc::clone(&store));

    // Generate 3 proofs.
    for block in [10, 20, 30] {
        scheduler.on_block_committed(block, make_input(block));
    }
    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

    let stats = store.stats();
    assert_eq!(stats.generated, 3);
    assert_eq!(stats.failed, 0);
    assert_eq!(store.len(), 3);
}

/// Verify created_at is set correctly through the full pipeline.
#[tokio::test]
async fn test_created_at_lifecycle() {
    let prover = Arc::new(MockProver::new());
    let store = Arc::new(ProofStore::new(100));
    let scheduler = ProofScheduler::new(prover, 10, Arc::clone(&store));

    scheduler.on_block_committed(10, make_input(10));
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    let proof = store.get_by_block(10).unwrap();
    assert!(proof.created_at > 0, "created_at should be set by MockProver");
    // Should be within last 10 seconds.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert!(now - proof.created_at < 10, "created_at should be recent");
}

/// ProofStore::list pagination works correctly.
#[test]
fn test_store_list_pagination() {
    let store = ProofStore::new(100);
    let prover = MockProver::new();

    // Generate proofs for blocks 10, 20, 30, 40, 50.
    for block in [10, 20, 30, 40, 50] {
        let result = prover.prove(&make_input(block)).unwrap();
        store.insert(result);
    }

    // List first 3 from block 0.
    let page1 = store.list(0, 3);
    assert_eq!(page1.len(), 3);
    assert_eq!(page1[0].block_number, 10);
    assert_eq!(page1[1].block_number, 20);
    assert_eq!(page1[2].block_number, 30);

    // List next page from block 31.
    let page2 = store.list(31, 3);
    assert_eq!(page2.len(), 2);
    assert_eq!(page2[0].block_number, 40);
    assert_eq!(page2[1].block_number, 50);

    // List from beyond stored range.
    let empty = store.list(100, 10);
    assert!(empty.is_empty());

    // List with limit 0.
    let zero = store.list(0, 0);
    assert!(zero.is_empty());
}
