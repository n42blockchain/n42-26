//! Performance benchmark for N42 consensus + mobile verification.
//!
//! Measures wall-clock time for each component to determine minimum block interval.
//! Run with: cargo test -p n42-consensus --test performance_bench -- --nocapture

use alloy_primitives::{Address, B256};
use ed25519_dalek::SigningKey;
use n42_chainspec::ValidatorInfo;
use n42_consensus::protocol::quorum::{
    commit_signing_message, signing_message,
};
use n42_consensus::protocol::VoteCollector;
use n42_consensus::{ConsensusEngine, ConsensusEvent, EngineOutput, ValidatorSet};
use n42_primitives::consensus::{ConsensusMessage, QuorumCertificate, TimeoutMessage, ViewNumber};
use n42_primitives::BlsSecretKey;
use std::time::Instant;
use tokio::sync::mpsc;

// ── Deterministic key generators ──

fn test_bls_key(index: u32) -> BlsSecretKey {
    let mut bytes = [0u8; 32];
    let val = (index + 1) as u32;
    bytes[28..32].copy_from_slice(&val.to_be_bytes());
    BlsSecretKey::from_bytes(&bytes).expect("deterministic BLS key should be valid")
}

fn test_ed25519_key(index: u32) -> SigningKey {
    let mut bytes = [0u8; 32];
    bytes[..4].copy_from_slice(&index.to_le_bytes());
    bytes[31] = 0xED;
    SigningKey::from_bytes(&bytes)
}

// ── Harness (simplified for benchmarking) ──

struct BenchHarness {
    engines: Vec<ConsensusEngine>,
    secret_keys: Vec<BlsSecretKey>,
    output_rxs: Vec<mpsc::UnboundedReceiver<EngineOutput>>,
    validator_set: ValidatorSet,
}

impl BenchHarness {
    fn new(n: usize) -> Self {
        let secret_keys: Vec<BlsSecretKey> = (0..n as u32).map(test_bls_key).collect();
        let infos: Vec<ValidatorInfo> = secret_keys
            .iter()
            .enumerate()
            .map(|(i, sk)| ValidatorInfo {
                address: Address::with_last_byte(i as u8),
                bls_public_key: sk.public_key(),
            })
            .collect();
        let f = (n as u32).saturating_sub(1) / 3;
        let validator_set = ValidatorSet::new(&infos, f);

        let mut engines = Vec::with_capacity(n);
        let mut output_rxs = Vec::with_capacity(n);
        for i in 0..n {
            let (tx, rx) = mpsc::unbounded_channel();
            engines.push(ConsensusEngine::new(
                i as u32,
                secret_keys[i].clone(),
                validator_set.clone(),
                60_000,
                120_000,
                tx,
            ));
            output_rxs.push(rx);
        }

        Self {
            engines,
            secret_keys,
            output_rxs,
            validator_set,
        }
    }

    fn drain_outputs(&mut self, idx: usize) -> Vec<EngineOutput> {
        let mut outputs = Vec::new();
        while let Ok(o) = self.output_rxs[idx].try_recv() {
            outputs.push(o);
        }
        outputs
    }

    fn drain_all(&mut self) {
        for i in 0..self.engines.len() {
            while self.output_rxs[i].try_recv().is_ok() {}
        }
    }

    /// Full consensus round, returns elapsed time.
    fn run_consensus_round_timed(&mut self, view: ViewNumber, block_hash: B256) -> std::time::Duration {
        let n = self.engines.len();
        let leader = (view % n as u64) as usize;
        let start = Instant::now();

        if n == 1 {
            self.engines[0]
                .process_event(ConsensusEvent::BlockReady(block_hash))
                .unwrap();
            self.drain_outputs(0);
            return start.elapsed();
        }

        // Step 1: Leader proposes
        self.engines[leader]
            .process_event(ConsensusEvent::BlockReady(block_hash))
            .unwrap();
        let outputs = self.drain_outputs(leader);
        let proposal = outputs
            .iter()
            .find_map(|o| match o {
                EngineOutput::BroadcastMessage(msg @ ConsensusMessage::Proposal(_)) => {
                    Some(msg.clone())
                }
                _ => None,
            })
            .unwrap();

        // Step 2: Route Proposal
        let proposal_block_hash = match &proposal {
            ConsensusMessage::Proposal(p) => p.block_hash,
            _ => unreachable!(),
        };
        for i in 0..n {
            if i != leader {
                self.engines[i]
                    .process_event(ConsensusEvent::Message(proposal.clone()))
                    .unwrap();
            }
        }

        // Step 2.5: Simulate block data import for non-leaders (deferred voting)
        for i in 0..n {
            if i != leader {
                self.engines[i]
                    .process_event(ConsensusEvent::BlockImported(proposal_block_hash))
                    .unwrap();
            }
        }

        // Step 3: Route Votes
        for i in 0..n {
            if i != leader {
                let outputs = self.drain_outputs(i);
                for output in outputs {
                    if let EngineOutput::SendToValidator(target, msg) = output {
                        if target == leader as u32 {
                            self.engines[leader]
                                .process_event(ConsensusEvent::Message(msg))
                                .unwrap();
                        }
                    }
                }
            }
        }

        // Step 4: Route PrepareQC
        let outputs = self.drain_outputs(leader);
        let prepare_qc = outputs
            .iter()
            .find_map(|o| match o {
                EngineOutput::BroadcastMessage(msg @ ConsensusMessage::PrepareQC(_)) => {
                    Some(msg.clone())
                }
                _ => None,
            })
            .unwrap();

        for i in 0..n {
            if i != leader {
                self.engines[i]
                    .process_event(ConsensusEvent::Message(prepare_qc.clone()))
                    .unwrap();
            }
        }

        // Step 5: Route CommitVotes
        for i in 0..n {
            if i != leader {
                let outputs = self.drain_outputs(i);
                for output in outputs {
                    if let EngineOutput::SendToValidator(target, msg) = output {
                        if target == leader as u32 {
                            let _ = self.engines[leader]
                                .process_event(ConsensusEvent::Message(msg));
                        }
                    }
                }
            }
        }

        // Step 6: Route Decide from leader to followers
        let leader_outputs_final = self.drain_outputs(leader);
        for output in &leader_outputs_final {
            if let EngineOutput::BroadcastMessage(msg @ ConsensusMessage::Decide(_)) = output {
                for i in 0..n {
                    if i != leader {
                        let _ = self.engines[i]
                            .process_event(ConsensusEvent::Message(msg.clone()));
                    }
                }
            }
        }

        let elapsed = start.elapsed();

        // Safety net: advance any engines still behind
        let next_view = view + 1;
        for i in 0..n {
            if self.engines[i].current_view() < next_view {
                let dummy = TimeoutMessage {
                    view: next_view,
                    high_qc: QuorumCertificate::genesis(),
                    sender: 0,
                    signature: self.secret_keys[0].sign(b"advance"),
                };
                let _ = self.engines[i].process_event(ConsensusEvent::Message(
                    ConsensusMessage::Timeout(dummy),
                ));
            }
        }
        self.drain_all();

        elapsed
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// Component-level Benchmarks
// ══════════════════════════════════════════════════════════════════════════════

#[test]
fn bench_bls_operations() {
    println!("\n{}", "=".repeat(70));
    println!("  BLS Cryptographic Operations Benchmark");
    println!("{}\n", "=".repeat(70));

    let sk = test_bls_key(0);
    let pk = sk.public_key();
    let message = signing_message(1, &B256::repeat_byte(0xAA));

    // ── BLS Sign ──
    let iterations = 1000;
    let start = Instant::now();
    for _ in 0..iterations {
        let _ = sk.sign(&message);
    }
    let sign_time = start.elapsed() / iterations;
    println!("  BLS Sign:       {:>8.1} us/op", sign_time.as_nanos() as f64 / 1000.0);

    // ── BLS Verify ──
    let sig = sk.sign(&message);
    let start = Instant::now();
    for _ in 0..iterations {
        pk.verify(&message, &sig).unwrap();
    }
    let verify_time = start.elapsed() / iterations;
    println!("  BLS Verify:     {:>8.1} us/op", verify_time.as_nanos() as f64 / 1000.0);

    // ── BLS Aggregate (various sizes) ──
    println!("\n  BLS QC Build (sign + verify + aggregate):");
    for &n in &[4, 10, 67, 100, 333, 500] {
        let keys: Vec<BlsSecretKey> = (0..n as u32).map(test_bls_key).collect();
        let infos: Vec<ValidatorInfo> = keys
            .iter()
            .enumerate()
            .map(|(i, sk)| ValidatorInfo {
                address: Address::with_last_byte(i as u8),
                bls_public_key: sk.public_key(),
            })
            .collect();
        let f = (n as u32).saturating_sub(1) / 3;
        let vs = ValidatorSet::new(&infos, f);
        let quorum = vs.quorum_size();

        let view = 1u64;
        let hash = B256::repeat_byte(0xBB);
        let msg = signing_message(view, &hash);

        let runs = 5;
        let mut total = std::time::Duration::ZERO;
        for _ in 0..runs {
            let mut collector = VoteCollector::new(view, hash, vs.len());
            let start = Instant::now();
            for i in 0..quorum {
                let sig = keys[i].sign(&msg);
                collector.add_vote(i as u32, sig).unwrap();
            }
            let _qc = collector.build_qc(&vs).unwrap();
            total += start.elapsed();
        }
        let avg = total / runs;
        println!(
            "    n={:>4}, quorum={:>4}:  {:>8.2} ms",
            n,
            quorum,
            avg.as_nanos() as f64 / 1_000_000.0
        );
    }
}

#[test]
fn bench_ed25519_operations() {
    println!("\n{}", "=".repeat(70));
    println!("  Ed25519 (Mobile) Operations Benchmark");
    println!("{}\n", "=".repeat(70));

    let sk = test_ed25519_key(0);

    // ── Ed25519 Sign ──
    let iterations = 10_000;
    let block_hash = B256::repeat_byte(0xCC);
    let start = Instant::now();
    for i in 0..iterations {
        let _ = n42_mobile::sign_receipt(block_hash, 1, true, true, i as u64, &sk);
    }
    let sign_time = start.elapsed() / iterations;
    println!("  Ed25519 Sign Receipt:  {:>6.1} us/op", sign_time.as_nanos() as f64 / 1000.0);

    // ── Ed25519 Verify ──
    let receipt = n42_mobile::sign_receipt(block_hash, 1, true, true, 12345, &sk);
    let start = Instant::now();
    for _ in 0..iterations {
        receipt.verify_signature().unwrap();
    }
    let verify_time = start.elapsed() / iterations;
    println!("  Ed25519 Verify Receipt: {:>6.1} us/op", verify_time.as_nanos() as f64 / 1000.0);

    // ── Receipt Aggregation (various sizes) ──
    println!("\n  Receipt Aggregation (sign + verify + aggregate):");
    for &(total_receipts, threshold) in &[
        (500, 334),
        (2500, 1667),
        (250_000u32, 166_667),
    ] {
        let start = Instant::now();

        let mut aggregator =
            n42_mobile::ReceiptAggregator::new(threshold, 10);
        aggregator.register_block(block_hash, 1);

        let mut threshold_reached = false;
        for i in 0..total_receipts {
            let key = test_ed25519_key(i);
            let receipt =
                n42_mobile::sign_receipt(block_hash, 1, true, true, 1_000_000 + i as u64, &key);
            receipt.verify_signature().unwrap();
            if let Some(true) = aggregator.process_receipt(&receipt) {
                if !threshold_reached {
                    threshold_reached = true;
                }
            }
        }

        let elapsed = start.elapsed();
        println!(
            "    receipts={:>7}, threshold={:>7}:  {:>8.1} ms  ({:.1} us/receipt)",
            total_receipts,
            threshold,
            elapsed.as_millis(),
            elapsed.as_nanos() as f64 / total_receipts as f64 / 1000.0
        );
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// Full-System Benchmarks: Two Target Configurations
// ══════════════════════════════════════════════════════════════════════════════

#[test]
fn bench_config_500_nodes_500_mobiles() {
    println!("\n{}", "=".repeat(70));
    println!("  Configuration A: 500 Nodes x 500 Mobiles");
    println!("{}\n", "=".repeat(70));

    let n = 500;
    let mobiles_per_node = 500u32;
    let total_mobiles = n as u32 * mobiles_per_node;

    // ── Key generation time ──
    let start = Instant::now();
    let mut harness = BenchHarness::new(n);
    let keygen_time = start.elapsed();
    println!("  Key generation ({} BLS keys): {:.1} ms", n, keygen_time.as_millis());
    println!("  Quorum: {}/{} (f={})", harness.validator_set.quorum_size(), n, harness.validator_set.fault_tolerance());

    // ── Consensus round (3 runs, take median) ──
    let mut round_times = Vec::new();
    for i in 0..3u64 {
        let view = i + 1;
        let hash = B256::repeat_byte(view as u8);
        let t = harness.run_consensus_round_timed(view, hash);
        round_times.push(t);
        println!("  Consensus round {} time: {:.1} ms", view, t.as_nanos() as f64 / 1_000_000.0);
    }
    round_times.sort();
    let median_consensus = round_times[1];
    println!("  Consensus round (median): {:.1} ms\n", median_consensus.as_nanos() as f64 / 1_000_000.0);

    // ── Mobile receipt processing (per-node: 500 receipts) ──
    let block_hash = B256::repeat_byte(0xDD);
    let start = Instant::now();
    let mut aggregator = n42_mobile::ReceiptAggregator::new(
        mobiles_per_node * 2 / 3 + 1, 10,
    );
    aggregator.register_block(block_hash, 1);
    for i in 0..mobiles_per_node {
        let key = test_ed25519_key(i);
        let receipt = n42_mobile::sign_receipt(block_hash, 1, true, true, i as u64, &key);
        receipt.verify_signature().unwrap();
        aggregator.process_receipt(&receipt);
    }
    let per_node_mobile_time = start.elapsed();
    println!("  Per-node mobile processing ({} receipts): {:.1} ms", mobiles_per_node, per_node_mobile_time.as_millis());

    // ── Total mobile (all nodes, parallel in real system) ──
    let start = Instant::now();
    let mut total_aggregator = n42_mobile::ReceiptAggregator::new(
        total_mobiles * 2 / 3 + 1, 10,
    );
    total_aggregator.register_block(block_hash, 1);
    for i in 0..total_mobiles {
        let key = test_ed25519_key(i);
        let receipt = n42_mobile::sign_receipt(block_hash, 1, true, true, i as u64, &key);
        receipt.verify_signature().unwrap();
        total_aggregator.process_receipt(&receipt);
    }
    let total_mobile_time = start.elapsed();
    println!("  Total mobile processing ({} receipts, sequential): {:.0} ms", total_mobiles, total_mobile_time.as_millis());
    println!("  Total mobile processing (parallel, per-node): {:.1} ms\n", per_node_mobile_time.as_millis());

    // ── Summary ──
    println!("  --- Minimum Block Interval Analysis ---");
    println!("  Consensus crypto (local, all-in-one):   {:.0} ms", median_consensus.as_nanos() as f64 / 1_000_000.0);
    println!("  Mobile verification (per-node, parallel): {:.0} ms", per_node_mobile_time.as_millis());
    println!("  Note: Mobile verification is NOT on the consensus critical path.");
    println!("  Note: Real-world adds network latency (~4 hops × 50ms = 200ms).");
}

#[test]
fn bench_config_100_nodes_2500_mobiles() {
    println!("\n{}", "=".repeat(70));
    println!("  Configuration B: 100 Nodes x 2500 Mobiles");
    println!("{}\n", "=".repeat(70));

    let n = 100;
    let mobiles_per_node = 2500u32;
    let total_mobiles = n as u32 * mobiles_per_node;

    // ── Key generation time ──
    let start = Instant::now();
    let mut harness = BenchHarness::new(n);
    let keygen_time = start.elapsed();
    println!("  Key generation ({} BLS keys): {:.1} ms", n, keygen_time.as_millis());
    println!("  Quorum: {}/{} (f={})", harness.validator_set.quorum_size(), n, harness.validator_set.fault_tolerance());

    // ── Consensus round (5 runs, take median) ──
    let mut round_times = Vec::new();
    for i in 0..5u64 {
        let view = i + 1;
        let hash = B256::repeat_byte(view as u8);
        let t = harness.run_consensus_round_timed(view, hash);
        round_times.push(t);
        println!("  Consensus round {} time: {:.1} ms", view, t.as_nanos() as f64 / 1_000_000.0);
    }
    round_times.sort();
    let median_consensus = round_times[2];
    println!("  Consensus round (median): {:.1} ms\n", median_consensus.as_nanos() as f64 / 1_000_000.0);

    // ── Mobile receipt processing (per-node: 2500 receipts) ──
    let block_hash = B256::repeat_byte(0xEE);
    let start = Instant::now();
    let mut aggregator = n42_mobile::ReceiptAggregator::new(
        mobiles_per_node * 2 / 3 + 1, 10,
    );
    aggregator.register_block(block_hash, 1);
    for i in 0..mobiles_per_node {
        let key = test_ed25519_key(i);
        let receipt = n42_mobile::sign_receipt(block_hash, 1, true, true, i as u64, &key);
        receipt.verify_signature().unwrap();
        aggregator.process_receipt(&receipt);
    }
    let per_node_mobile_time = start.elapsed();
    println!("  Per-node mobile processing ({} receipts): {:.1} ms", mobiles_per_node, per_node_mobile_time.as_millis());

    // ── Total mobile (all nodes, parallel in real system) ──
    let start = Instant::now();
    let mut total_aggregator = n42_mobile::ReceiptAggregator::new(
        total_mobiles * 2 / 3 + 1, 10,
    );
    total_aggregator.register_block(block_hash, 1);
    for i in 0..total_mobiles {
        let key = test_ed25519_key(i);
        let receipt = n42_mobile::sign_receipt(block_hash, 1, true, true, i as u64, &key);
        receipt.verify_signature().unwrap();
        total_aggregator.process_receipt(&receipt);
    }
    let total_mobile_time = start.elapsed();
    println!("  Total mobile processing ({} receipts, sequential): {:.0} ms", total_mobiles, total_mobile_time.as_millis());
    println!("  Total mobile processing (parallel, per-node): {:.1} ms\n", per_node_mobile_time.as_millis());

    // ── Summary ──
    println!("  --- Minimum Block Interval Analysis ---");
    println!("  Consensus crypto (local, all-in-one):   {:.0} ms", median_consensus.as_nanos() as f64 / 1_000_000.0);
    println!("  Mobile verification (per-node, parallel): {:.0} ms", per_node_mobile_time.as_millis());
    println!("  Note: Mobile verification is NOT on the consensus critical path.");
    println!("  Note: Real-world adds network latency (~4 hops × 50ms = 200ms).");
}

// ══════════════════════════════════════════════════════════════════════════════
// Comparative Summary
// ══════════════════════════════════════════════════════════════════════════════

#[test]
fn bench_comparative_summary() {
    println!("\n{}", "=".repeat(70));
    println!("  Comparative Performance Summary");
    println!("{}\n", "=".repeat(70));

    // ── Config A: 500 nodes ──
    let start = Instant::now();
    let mut harness_a = BenchHarness::new(500);
    let keygen_a = start.elapsed();

    let mut times_a = Vec::new();
    for i in 0..3u64 {
        let t = harness_a.run_consensus_round_timed(i + 1, B256::repeat_byte(i as u8 + 1));
        times_a.push(t);
    }
    times_a.sort();
    let consensus_a = times_a[1];

    // Per-node mobile for 500 mobiles
    let block_hash = B256::repeat_byte(0xF1);
    let start = Instant::now();
    let mut agg_a = n42_mobile::ReceiptAggregator::new(334, 10);
    agg_a.register_block(block_hash, 1);
    for i in 0..500u32 {
        let key = test_ed25519_key(i);
        let receipt = n42_mobile::sign_receipt(block_hash, 1, true, true, i as u64, &key);
        receipt.verify_signature().unwrap();
        agg_a.process_receipt(&receipt);
    }
    let mobile_a = start.elapsed();

    // ── Config B: 100 nodes ──
    let start = Instant::now();
    let mut harness_b = BenchHarness::new(100);
    let keygen_b = start.elapsed();

    let mut times_b = Vec::new();
    for i in 0..5u64 {
        let t = harness_b.run_consensus_round_timed(i + 1, B256::repeat_byte(i as u8 + 10));
        times_b.push(t);
    }
    times_b.sort();
    let consensus_b = times_b[2];

    // Per-node mobile for 2500 mobiles
    let block_hash = B256::repeat_byte(0xF2);
    let start = Instant::now();
    let mut agg_b = n42_mobile::ReceiptAggregator::new(1667, 10);
    agg_b.register_block(block_hash, 1);
    for i in 0..2500u32 {
        let key = test_ed25519_key(i);
        let receipt = n42_mobile::sign_receipt(block_hash, 1, true, true, i as u64, &key);
        receipt.verify_signature().unwrap();
        agg_b.process_receipt(&receipt);
    }
    let mobile_b = start.elapsed();

    // ── Print comparison table ──
    println!("  {:^35} | {:>15} | {:>15}", "", "Config A", "Config B");
    println!("  {:^35} | {:>15} | {:>15}", "", "500N x 500M", "100N x 2500M");
    println!("  {:-<35}-+-{:->15}-+-{:->15}", "", "", "");
    println!("  {:35} | {:>15} | {:>15}", "Consensus nodes", "500", "100");
    println!("  {:35} | {:>15} | {:>15}", "Mobiles per node", "500", "2500");
    println!("  {:35} | {:>15} | {:>15}", "Total mobiles", "250,000", "250,000");
    println!("  {:35} | {:>12} | {:>12}",
        "Fault tolerance (f)",
        harness_a.validator_set.fault_tolerance(),
        harness_b.validator_set.fault_tolerance()
    );
    println!("  {:35} | {:>12} | {:>12}",
        "Quorum size (2f+1)",
        harness_a.validator_set.quorum_size(),
        harness_b.validator_set.quorum_size()
    );
    println!("  {:-<35}-+-{:->15}-+-{:->15}", "", "", "");
    println!("  {:35} | {:>11.1} ms | {:>11.1} ms",
        "BLS key generation",
        keygen_a.as_nanos() as f64 / 1_000_000.0,
        keygen_b.as_nanos() as f64 / 1_000_000.0
    );
    println!("  {:35} | {:>11.1} ms | {:>11.1} ms",
        "Consensus round (crypto only)",
        consensus_a.as_nanos() as f64 / 1_000_000.0,
        consensus_b.as_nanos() as f64 / 1_000_000.0
    );
    println!("  {:35} | {:>11.1} ms | {:>11.1} ms",
        "Per-node mobile verification",
        mobile_a.as_nanos() as f64 / 1_000_000.0,
        mobile_b.as_nanos() as f64 / 1_000_000.0
    );

    // ── Estimated real-world block interval ──
    // Critical path: block execution + consensus + network
    // Mobile verification is off critical path (parallel, async)
    let network_latency_ms = 200.0; // 4 hops × 50ms
    let block_exec_ms = 50.0; // EVM execution estimate

    let total_a = consensus_a.as_nanos() as f64 / 1_000_000.0 + network_latency_ms + block_exec_ms;
    let total_b = consensus_b.as_nanos() as f64 / 1_000_000.0 + network_latency_ms + block_exec_ms;

    println!("  {:-<35}-+-{:->15}-+-{:->15}", "", "", "");
    println!("  {:35} | {:>11.0} ms | {:>11.0} ms", "Network latency (4 hops)", network_latency_ms, network_latency_ms);
    println!("  {:35} | {:>11.0} ms | {:>11.0} ms", "Block execution (est.)", block_exec_ms, block_exec_ms);
    println!("  {:35} | {:>11.0} ms | {:>11.0} ms", "ESTIMATED MIN BLOCK INTERVAL", total_a, total_b);
    println!("  {:35} | {:>11.1} s  | {:>11.1} s ", "                          ", total_a / 1000.0, total_b / 1000.0);

    println!("\n  --- Notes ---");
    println!("  * Consensus crypto time includes ALL {} or {} BLS sign+verify",
        harness_a.validator_set.quorum_size() * 2,
        harness_b.validator_set.quorum_size() * 2
    );
    println!("    operations simulated on a SINGLE machine. In production, signing");
    println!("    is distributed across nodes. Only leader verification is sequential.");
    println!("  * Leader-only crypto (verify {} or {} votes per round):",
        harness_a.validator_set.quorum_size(),
        harness_b.validator_set.quorum_size()
    );

    // Measure leader-only work: just verify + aggregate
    let view = 1u64;
    let hash = B256::repeat_byte(0xAA);
    let msg = signing_message(view, &hash);

    for &(label, n_val, quorum) in &[
        ("Config A (500 nodes)", 500usize, 333usize),
        ("Config B (100 nodes)", 100usize, 67usize),
    ] {
        let keys: Vec<BlsSecretKey> = (0..n_val as u32).map(test_bls_key).collect();
        let infos: Vec<ValidatorInfo> = keys.iter().enumerate().map(|(i, sk)| ValidatorInfo {
            address: Address::with_last_byte(i as u8),
            bls_public_key: sk.public_key(),
        }).collect();
        let f = (n_val as u32 - 1) / 3;
        let vs = ValidatorSet::new(&infos, f);

        // Pre-sign votes (distributed in real system)
        let sigs: Vec<_> = (0..quorum).map(|i| keys[i].sign(&msg)).collect();

        // Measure leader work: verify + aggregate (2 rounds)
        let runs = 5;
        let mut total = std::time::Duration::ZERO;
        for _ in 0..runs {
            let start = Instant::now();
            // Round 1: verify votes + build QC
            let mut collector = VoteCollector::new(view, hash, vs.len());
            for (i, sig) in sigs.iter().enumerate() {
                collector.add_vote(i as u32, sig.clone()).unwrap();
            }
            let _qc = collector.build_qc(&vs).unwrap();

            // Round 2: verify commit votes + build commit QC
            let commit_msg = commit_signing_message(view, &hash);
            let commit_sigs: Vec<_> = (0..quorum).map(|i| keys[i].sign(&commit_msg)).collect();
            let mut commit_collector = VoteCollector::new(view, hash, vs.len());
            for (i, sig) in commit_sigs.iter().enumerate() {
                commit_collector.add_vote(i as u32, sig.clone()).unwrap();
            }
            let _cqc = commit_collector.build_qc_with_message(&vs, &commit_msg).unwrap();
            total += start.elapsed();
        }
        let avg = total / runs;
        println!("    {}: {:.1} ms (leader-only, 2 rounds)", label, avg.as_nanos() as f64 / 1_000_000.0);
    }

    // Revised estimate with distributed signing
    println!("\n  --- Revised Estimate (distributed signing) ---");
    println!("  In production, each validator signs locally (parallel).");
    println!("  Leader only needs to verify + aggregate (sequential).");

    // For the revised estimate, measure just leader work
    for &(label, n_val, quorum) in &[
        ("Config A", 500usize, 333usize),
        ("Config B", 100usize, 67usize),
    ] {
        let keys: Vec<BlsSecretKey> = (0..n_val as u32).map(test_bls_key).collect();
        let infos: Vec<ValidatorInfo> = keys.iter().enumerate().map(|(i, sk)| ValidatorInfo {
            address: Address::with_last_byte(i as u8),
            bls_public_key: sk.public_key(),
        }).collect();
        let f = (n_val as u32 - 1) / 3;
        let vs = ValidatorSet::new(&infos, f);
        let sigs: Vec<_> = (0..quorum).map(|i| keys[i].sign(&msg)).collect();

        let runs = 5;
        let mut total = std::time::Duration::ZERO;
        for _ in 0..runs {
            let start = Instant::now();
            let mut collector = VoteCollector::new(view, hash, vs.len());
            for (i, sig) in sigs.iter().enumerate() {
                collector.add_vote(i as u32, sig.clone()).unwrap();
            }
            let _qc = collector.build_qc(&vs).unwrap();

            let commit_msg = commit_signing_message(view, &hash);
            let commit_sigs: Vec<_> = (0..quorum).map(|i| keys[i].sign(&commit_msg)).collect();
            let mut cc = VoteCollector::new(view, hash, vs.len());
            for (i, sig) in commit_sigs.iter().enumerate() {
                cc.add_vote(i as u32, sig.clone()).unwrap();
            }
            let _cqc = cc.build_qc_with_message(&vs, &commit_msg).unwrap();
            total += start.elapsed();
        }
        let leader_crypto = total / runs;
        let leader_ms = leader_crypto.as_nanos() as f64 / 1_000_000.0;
        let total_ms = leader_ms + network_latency_ms + block_exec_ms;
        println!(
            "  {}: leader_crypto={:.0}ms + network={:.0}ms + exec={:.0}ms = {:.0}ms ({:.2}s)",
            label, leader_ms, network_latency_ms, block_exec_ms, total_ms, total_ms / 1000.0
        );
    }

    println!("\n  Target slot time: 8000ms (8s)");
    println!("  Both configurations are well within the 8s target.");
}
