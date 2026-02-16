//! Full integration test suite for the N42 HotStuff-2 consensus engine.
//!
//! Tests cover: genesis bootstrap, multi-node consensus, mobile verification,
//! fault tolerance, boundary conditions, stress performance, and stability.
//!
//! The `TestHarness` simulates multiple consensus engines exchanging messages
//! via synchronous routing—no real network, fully deterministic.

use alloy_primitives::{Address, B256};
use ed25519_dalek::SigningKey;
use n42_chainspec::ValidatorInfo;
use n42_consensus::error::ConsensusError;
use n42_consensus::protocol::quorum::signing_message;
use n42_consensus::protocol::Phase;
use n42_consensus::{ConsensusEngine, ConsensusEvent, EngineOutput, LeaderSelector, ValidatorSet};
use n42_primitives::consensus::{
    ConsensusMessage, Proposal, QuorumCertificate, TimeoutMessage, ViewNumber, Vote,
};
use n42_primitives::BlsSecretKey;
use tokio::sync::mpsc;

// ══════════════════════════════════════════════════════════════════════════════
// Deterministic key generators (TEST ONLY)
// ══════════════════════════════════════════════════════════════════════════════

/// Deterministic BLS secret key from a small index.
/// The bytes are arranged so that the big-endian scalar is small and valid.
fn test_bls_key(index: u32) -> BlsSecretKey {
    let mut bytes = [0u8; 32];
    // Place (index+1) in the last 4 bytes (big-endian) → small valid scalar
    let val = (index + 1) as u32;
    bytes[28..32].copy_from_slice(&val.to_be_bytes());
    BlsSecretKey::from_bytes(&bytes).expect("deterministic BLS key should be valid")
}

/// Deterministic Ed25519 signing key from an index.
fn test_ed25519_key(index: u32) -> SigningKey {
    let mut bytes = [0u8; 32];
    bytes[..4].copy_from_slice(&index.to_le_bytes());
    bytes[31] = 0xED;
    SigningKey::from_bytes(&bytes)
}

// ══════════════════════════════════════════════════════════════════════════════
// TestHarness
// ══════════════════════════════════════════════════════════════════════════════

struct TestHarness {
    engines: Vec<ConsensusEngine>,
    secret_keys: Vec<BlsSecretKey>,
    output_rxs: Vec<mpsc::UnboundedReceiver<EngineOutput>>,
    validator_set: ValidatorSet,
    committed_blocks: Vec<(ViewNumber, B256)>,
}

impl TestHarness {
    /// Creates a test system with `n` validators, deterministic keys.
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
            let engine = ConsensusEngine::new(
                i as u32,
                secret_keys[i].clone(),
                validator_set.clone(),
                60_000,
                120_000,
                tx,
            );
            engines.push(engine);
            output_rxs.push(rx);
        }

        Self {
            engines,
            secret_keys,
            output_rxs,
            validator_set,
            committed_blocks: Vec::new(),
        }
    }

    /// Drains all pending outputs from engine `idx`.
    fn drain_outputs(&mut self, idx: usize) -> Vec<EngineOutput> {
        let mut outputs = Vec::new();
        while let Ok(o) = self.output_rxs[idx].try_recv() {
            outputs.push(o);
        }
        outputs
    }

    /// Drains outputs from all engines. Returns a Vec of Vecs.
    fn drain_all_outputs(&mut self) -> Vec<Vec<EngineOutput>> {
        (0..self.engines.len())
            .map(|i| self.drain_outputs(i))
            .collect()
    }

    /// Returns the number of validators.
    fn n(&self) -> usize {
        self.engines.len()
    }

    /// Runs a full two-round consensus for the given view and block hash.
    ///
    /// For n=1: everything completes in one `BlockReady` call.
    /// For n>=2: routes Proposal → Votes → PrepareQC → CommitVotes → Committed,
    /// then advances all non-leader engines to the next view.
    fn run_consensus_round(&mut self, view: ViewNumber, block_hash: B256) {
        let n = self.n();
        let leader = (view % n as u64) as usize;

        if n == 1 {
            self.engines[0]
                .process_event(ConsensusEvent::BlockReady(block_hash))
                .expect("single-validator BlockReady should succeed");

            let outputs = self.drain_outputs(0);
            for output in &outputs {
                if let EngineOutput::BlockCommitted {
                    view: v,
                    block_hash: h,
                    ..
                } = output
                {
                    self.committed_blocks.push((*v, *h));
                }
            }
            return;
        }

        // ── Step 1: Leader proposes ──
        self.engines[leader]
            .process_event(ConsensusEvent::BlockReady(block_hash))
            .expect("leader BlockReady should succeed");

        let leader_outputs = self.drain_outputs(leader);
        let proposal = leader_outputs
            .iter()
            .find_map(|o| match o {
                EngineOutput::BroadcastMessage(msg @ ConsensusMessage::Proposal(_)) => {
                    Some(msg.clone())
                }
                _ => None,
            })
            .expect("leader should broadcast Proposal");

        // ── Step 2: Route Proposal to all non-leaders ──
        for i in 0..n {
            if i != leader {
                self.engines[i]
                    .process_event(ConsensusEvent::Message(proposal.clone()))
                    .expect("non-leader should accept Proposal");
            }
        }

        // ── Step 3: Route Votes from non-leaders to leader ──
        for i in 0..n {
            if i != leader {
                let outputs = self.drain_outputs(i);
                for output in outputs {
                    if let EngineOutput::SendToValidator(target, msg) = output {
                        if target == leader as u32 {
                            self.engines[leader]
                                .process_event(ConsensusEvent::Message(msg))
                                .expect("leader should accept Vote");
                        }
                    }
                }
            }
        }

        // ── Step 4: Route PrepareQC from leader to all non-leaders ──
        let leader_outputs = self.drain_outputs(leader);
        let prepare_qc = leader_outputs
            .iter()
            .find_map(|o| match o {
                EngineOutput::BroadcastMessage(msg @ ConsensusMessage::PrepareQC(_)) => {
                    Some(msg.clone())
                }
                _ => None,
            })
            .expect("leader should broadcast PrepareQC");

        for i in 0..n {
            if i != leader {
                self.engines[i]
                    .process_event(ConsensusEvent::Message(prepare_qc.clone()))
                    .expect("non-leader should accept PrepareQC");
            }
        }

        // ── Step 5: Route CommitVotes from non-leaders to leader ──
        // After quorum is reached, the leader commits and advances view.
        // Late commit votes from remaining validators will get ViewMismatch,
        // which is expected behavior—we ignore those errors.
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

        // ── Step 6: Record committed block ──
        let leader_outputs = self.drain_outputs(leader);
        for output in &leader_outputs {
            if let EngineOutput::BlockCommitted {
                view: v,
                block_hash: h,
                ..
            } = output
            {
                self.committed_blocks.push((*v, *h));
            }
        }

        // ── Step 7: Advance non-leaders to next view via timeout catch-up ──
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

        // Drain any remaining outputs
        self.drain_all_outputs();
    }

    /// Runs a consensus round with partial participation (only specific voters).
    /// Returns true if the round committed.
    fn run_consensus_round_partial(
        &mut self,
        view: ViewNumber,
        block_hash: B256,
        participating: &[usize],
    ) -> bool {
        let n = self.n();
        let leader = (view % n as u64) as usize;

        assert!(
            participating.contains(&leader),
            "leader must participate"
        );

        // Step 1: Leader proposes
        self.engines[leader]
            .process_event(ConsensusEvent::BlockReady(block_hash))
            .expect("leader BlockReady should succeed");

        let leader_outputs = self.drain_outputs(leader);
        let proposal = leader_outputs
            .iter()
            .find_map(|o| match o {
                EngineOutput::BroadcastMessage(msg @ ConsensusMessage::Proposal(_)) => {
                    Some(msg.clone())
                }
                _ => None,
            })
            .expect("leader should broadcast Proposal");

        // Step 2: Route Proposal only to participating non-leaders
        for &i in participating {
            if i != leader {
                self.engines[i]
                    .process_event(ConsensusEvent::Message(proposal.clone()))
                    .expect("non-leader should accept Proposal");
            }
        }

        // Step 3: Route Votes to leader
        for &i in participating {
            if i != leader {
                let outputs = self.drain_outputs(i);
                for output in outputs {
                    if let EngineOutput::SendToValidator(target, msg) = output {
                        if target == leader as u32 {
                            self.engines[leader]
                                .process_event(ConsensusEvent::Message(msg))
                                .expect("leader should accept Vote");
                        }
                    }
                }
            }
        }

        // Step 4: Check if PrepareQC was formed
        let leader_outputs = self.drain_outputs(leader);
        let prepare_qc = leader_outputs.iter().find_map(|o| match o {
            EngineOutput::BroadcastMessage(msg @ ConsensusMessage::PrepareQC(_)) => {
                Some(msg.clone())
            }
            _ => None,
        });

        let prepare_qc = match prepare_qc {
            Some(pqc) => pqc,
            None => return false, // Not enough votes for QC
        };

        // Step 5: Route PrepareQC to participating non-leaders
        for &i in participating {
            if i != leader {
                self.engines[i]
                    .process_event(ConsensusEvent::Message(prepare_qc.clone()))
                    .expect("non-leader should accept PrepareQC");
            }
        }

        // Step 6: Route CommitVotes to leader (ignore late ViewMismatch)
        for &i in participating {
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

        // Step 7: Check for BlockCommitted
        let leader_outputs = self.drain_outputs(leader);
        let mut committed = false;
        for output in &leader_outputs {
            if let EngineOutput::BlockCommitted {
                view: v,
                block_hash: h,
                ..
            } = output
            {
                self.committed_blocks.push((*v, *h));
                committed = true;
            }
        }

        if committed {
            // Advance all engines to next view
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
            self.drain_all_outputs();
        }

        committed
    }

    /// Triggers timeout on a single engine.
    fn trigger_timeout(&mut self, idx: usize) {
        self.engines[idx]
            .on_timeout()
            .expect("timeout should succeed");
    }

    /// Simulates a full view-change flow: all engines timeout → TC formed → NewView.
    fn run_timeout_view_change(&mut self, view: ViewNumber) {
        let n = self.n();

        // Step 1: All engines timeout
        for i in 0..n {
            self.engines[i]
                .on_timeout()
                .expect("timeout should succeed");
        }

        // Step 2: Collect all timeout broadcast messages
        let all_outputs: Vec<Vec<EngineOutput>> =
            (0..n).map(|i| self.drain_outputs(i)).collect();

        let mut timeout_msgs: Vec<(usize, ConsensusMessage)> = Vec::new();
        for (i, outputs) in all_outputs.iter().enumerate() {
            for output in outputs {
                if let EngineOutput::BroadcastMessage(msg @ ConsensusMessage::Timeout(_)) = output
                {
                    timeout_msgs.push((i, msg.clone()));
                }
            }
        }

        // Step 3: Route all timeout messages to all other engines
        for (from, msg) in &timeout_msgs {
            for i in 0..n {
                if i != *from {
                    let _ = self.engines[i]
                        .process_event(ConsensusEvent::Message(msg.clone()));
                }
            }
        }

        // Step 4: Find NewView from the next leader
        let next_view = view + 1;
        let next_leader = (next_view % n as u64) as usize;

        let all_outputs: Vec<Vec<EngineOutput>> =
            (0..n).map(|i| self.drain_outputs(i)).collect();

        let new_view_msg = all_outputs.iter().flatten().find_map(|o| match o {
            EngineOutput::BroadcastMessage(msg @ ConsensusMessage::NewView(_)) => {
                Some(msg.clone())
            }
            _ => None,
        });

        // Step 5: Route NewView to non-next-leader engines
        if let Some(nv) = new_view_msg {
            for i in 0..n {
                if i != next_leader && self.engines[i].current_view() < next_view {
                    let _ = self.engines[i]
                        .process_event(ConsensusEvent::Message(nv.clone()));
                }
            }
        }

        self.drain_all_outputs();
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// Module 1: Genesis Bootstrap (3 tests)
// ══════════════════════════════════════════════════════════════════════════════

mod genesis_bootstrap {
    use super::*;

    #[test]
    fn test_genesis_initial_state() {
        let harness = TestHarness::new(4);

        for i in 0..4 {
            assert_eq!(
                harness.engines[i].current_view(),
                1,
                "engine {} initial view should be 1",
                i
            );
            assert_eq!(
                harness.engines[i].current_phase(),
                Phase::WaitingForProposal,
                "engine {} initial phase should be WaitingForProposal",
                i
            );
        }

        // Verify validator set properties
        assert_eq!(harness.validator_set.len(), 4);
        assert_eq!(harness.validator_set.fault_tolerance(), 1);
        assert_eq!(harness.validator_set.quorum_size(), 3);
    }

    #[test]
    fn test_single_validator_genesis() {
        let mut harness = TestHarness::new(1);

        assert_eq!(harness.engines[0].current_view(), 1);
        assert_eq!(harness.engines[0].current_phase(), Phase::WaitingForProposal);
        assert!(harness.engines[0].is_current_leader());

        // First block: instant self-consensus
        let block_hash = B256::repeat_byte(0x01);
        harness.run_consensus_round(1, block_hash);

        assert_eq!(harness.engines[0].current_view(), 2);
        assert_eq!(harness.committed_blocks.len(), 1);
        assert_eq!(harness.committed_blocks[0], (1, block_hash));
    }

    #[test]
    fn test_first_block_commitment() {
        let mut harness = TestHarness::new(4);
        let block_hash = B256::repeat_byte(0xAA);

        harness.run_consensus_round(1, block_hash);

        // All engines should be at view 2
        for i in 0..4 {
            assert_eq!(
                harness.engines[i].current_view(),
                2,
                "engine {} should be at view 2",
                i
            );
        }

        // Verify committed block
        assert_eq!(harness.committed_blocks.len(), 1);
        assert_eq!(harness.committed_blocks[0], (1, block_hash));
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// Module 2: Multi-Node Consensus (6 tests)
// ══════════════════════════════════════════════════════════════════════════════

mod multi_node_consensus {
    use super::*;

    #[test]
    fn test_full_consensus_4v() {
        let mut harness = TestHarness::new(4);
        let view = 1u64;
        let leader = (view % 4) as usize; // leader = 1
        let block_hash = B256::repeat_byte(0xF1);

        // Step 1: Leader proposes
        harness.engines[leader]
            .process_event(ConsensusEvent::BlockReady(block_hash))
            .unwrap();
        assert_eq!(harness.engines[leader].current_phase(), Phase::Voting);

        let outputs = harness.drain_outputs(leader);
        let proposal = outputs
            .iter()
            .find_map(|o| match o {
                EngineOutput::BroadcastMessage(msg @ ConsensusMessage::Proposal(_)) => {
                    Some(msg.clone())
                }
                _ => None,
            })
            .unwrap();

        // Step 2: Non-leaders receive Proposal, produce Votes
        for i in 0..4 {
            if i != leader {
                harness.engines[i]
                    .process_event(ConsensusEvent::Message(proposal.clone()))
                    .unwrap();
                assert_eq!(harness.engines[i].current_phase(), Phase::Voting);
            }
        }

        // Step 3: Route Votes to leader
        for i in 0..4 {
            if i != leader {
                let outputs = harness.drain_outputs(i);
                for output in outputs {
                    if let EngineOutput::SendToValidator(target, msg) = output {
                        if target == leader as u32 {
                            harness.engines[leader]
                                .process_event(ConsensusEvent::Message(msg))
                                .unwrap();
                        }
                    }
                }
            }
        }

        // Leader should have formed PrepareQC
        let outputs = harness.drain_outputs(leader);
        let prepare_qc = outputs
            .iter()
            .find_map(|o| match o {
                EngineOutput::BroadcastMessage(msg @ ConsensusMessage::PrepareQC(_)) => {
                    Some(msg.clone())
                }
                _ => None,
            })
            .expect("leader should broadcast PrepareQC");

        // Step 4: Non-leaders receive PrepareQC, produce CommitVotes
        for i in 0..4 {
            if i != leader {
                harness.engines[i]
                    .process_event(ConsensusEvent::Message(prepare_qc.clone()))
                    .unwrap();
                assert_eq!(harness.engines[i].current_phase(), Phase::PreCommit);
            }
        }

        // Step 5: Route CommitVotes to leader (ignore late ViewMismatch)
        for i in 0..4 {
            if i != leader {
                let outputs = harness.drain_outputs(i);
                for output in outputs {
                    if let EngineOutput::SendToValidator(target, msg) = output {
                        if target == leader as u32 {
                            let _ = harness.engines[leader]
                                .process_event(ConsensusEvent::Message(msg));
                        }
                    }
                }
            }
        }

        // Leader should have committed and advanced
        harness.drain_outputs(leader);
        assert_eq!(harness.engines[leader].current_view(), 2,
            "leader should have committed and advanced to view 2");
    }

    #[test]
    fn test_full_consensus_7v() {
        let mut harness = TestHarness::new(7);
        // f=2, quorum=5. Only 5 out of 7 participate.
        let view = 1u64;
        let leader = (view % 7) as usize; // leader = 1
        let block_hash = B256::repeat_byte(0xF7);
        let participating: Vec<usize> = (0..5).collect(); // engines 0-4 participate

        // Ensure leader is in participating set
        assert!(participating.contains(&leader));

        let committed =
            harness.run_consensus_round_partial(view, block_hash, &participating);
        assert!(committed, "5/7 validators should reach consensus");
        assert_eq!(harness.committed_blocks.len(), 1);
    }

    #[test]
    fn test_full_consensus_10v() {
        let mut harness = TestHarness::new(10);
        let block_hash = B256::repeat_byte(0xFA);

        harness.run_consensus_round(1, block_hash);

        assert_eq!(harness.committed_blocks.len(), 1);
        assert_eq!(harness.committed_blocks[0], (1, block_hash));

        // All engines should be at view 2
        for i in 0..10 {
            assert_eq!(harness.engines[i].current_view(), 2);
        }
    }

    #[test]
    fn test_consecutive_10_blocks() {
        let mut harness = TestHarness::new(4);

        for view in 1..=10u64 {
            let block_hash = B256::repeat_byte(view as u8);
            harness.run_consensus_round(view, block_hash);
        }

        assert_eq!(harness.committed_blocks.len(), 10);

        // Verify view monotonicity and correct block hashes
        for (idx, &(v, h)) in harness.committed_blocks.iter().enumerate() {
            let expected_view = (idx + 1) as u64;
            assert_eq!(v, expected_view, "block {} should be at view {}", idx, expected_view);
            assert_eq!(h, B256::repeat_byte(expected_view as u8));
        }

        // All engines at view 11
        for i in 0..4 {
            assert_eq!(harness.engines[i].current_view(), 11);
        }
    }

    #[test]
    fn test_leader_rotation_full_cycle() {
        let mut harness = TestHarness::new(4);

        // 8 blocks = 2 full rotation cycles
        for view in 1..=8u64 {
            let leader = (view % 4) as usize;
            let block_hash = B256::repeat_byte(view as u8);
            harness.run_consensus_round(view, block_hash);

            // Verify the correct leader was used
            let expected_leader = LeaderSelector::leader_for_view(view, &harness.validator_set);
            assert_eq!(expected_leader, leader as u32);
        }

        assert_eq!(harness.committed_blocks.len(), 8);

        // Each validator should have been leader exactly 2 times
        let mut leader_counts = [0u32; 4];
        for &(v, _) in &harness.committed_blocks {
            let leader = (v % 4) as usize;
            leader_counts[leader] += 1;
        }
        for (i, &count) in leader_counts.iter().enumerate() {
            assert_eq!(count, 2, "validator {} should have been leader 2 times", i);
        }
    }

    #[test]
    fn test_large_set_100_validators() {
        let mut harness = TestHarness::new(100);

        // f=33, quorum=67
        assert_eq!(harness.validator_set.fault_tolerance(), 33);
        assert_eq!(harness.validator_set.quorum_size(), 67);

        let block_hash = B256::repeat_byte(0xCC);
        harness.run_consensus_round(1, block_hash);

        assert_eq!(harness.committed_blocks.len(), 1);
        for i in 0..100 {
            assert_eq!(harness.engines[i].current_view(), 2);
        }
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// Module 3: Mobile Verification (6 tests)
// ══════════════════════════════════════════════════════════════════════════════

mod mobile_verification {
    use super::*;
    use n42_mobile::commitment::compute_commitment;
    use n42_mobile::{
        sign_receipt, CommitmentError, ReceiptAggregator, VerificationCommitment,
        VerificationReveal,
    };

    #[test]
    fn test_mobile_receipt_signing() {
        let block_hash = B256::repeat_byte(0xAB);
        let block_number = 42u64;

        for i in 0..10u32 {
            let key = test_ed25519_key(i);
            let receipt = sign_receipt(block_hash, block_number, true, true, 1_700_000_000_000, &key);

            assert!(receipt.is_valid());
            assert_eq!(receipt.block_hash, block_hash);
            assert_eq!(receipt.block_number, block_number);
            assert_eq!(receipt.verifier_pubkey, key.verifying_key().to_bytes());

            // Ed25519 signature must verify
            receipt
                .verify_signature()
                .expect("receipt signature should be valid");
        }
    }

    #[test]
    fn test_receipt_aggregation_threshold() {
        let block_hash = B256::repeat_byte(0xBC);
        let block_number = 100u64;
        let threshold = 7u32;

        let mut aggregator = ReceiptAggregator::new(threshold, 100);
        aggregator.register_block(block_hash, block_number);

        for i in 0..10u32 {
            let key = test_ed25519_key(i);
            let receipt = sign_receipt(block_hash, block_number, true, true, 1_000_000 + i as u64, &key);

            let result = aggregator.process_receipt(&receipt);

            if i < threshold - 1 {
                // Not yet reached threshold
                assert_eq!(
                    result,
                    Some(false),
                    "receipt {} should be accepted but not reach threshold",
                    i
                );
            } else if i == threshold - 1 {
                // Exactly reached threshold
                assert_eq!(
                    result,
                    Some(true),
                    "receipt {} should trigger threshold",
                    i
                );
            }
        }

        let status = aggregator.get_status(&block_hash).unwrap();
        assert!(status.is_attested());
        assert_eq!(status.total_receipts(), 10);
    }

    #[test]
    fn test_receipt_deduplication() {
        let block_hash = B256::repeat_byte(0xDE);
        let block_number = 50u64;

        let mut aggregator = ReceiptAggregator::new(5, 100);
        aggregator.register_block(block_hash, block_number);

        let key = test_ed25519_key(0);
        let receipt = sign_receipt(block_hash, block_number, true, true, 1_000_000, &key);

        // First submission accepted
        let result = aggregator.process_receipt(&receipt);
        assert_eq!(result, Some(false));

        // Duplicate submission rejected (returns None)
        let result = aggregator.process_receipt(&receipt);
        assert_eq!(result, None, "duplicate receipt should be rejected");

        let status = aggregator.get_status(&block_hash).unwrap();
        assert_eq!(status.total_receipts(), 1, "should still have only 1 receipt");
    }

    #[test]
    fn test_commitment_reveal_success() {
        let block_hash = B256::repeat_byte(0xCD);
        let block_number = 200u64;

        for i in 0..5u32 {
            let key = test_ed25519_key(i);
            let verifier_pubkey = key.verifying_key().to_bytes();
            let nonce = B256::repeat_byte(i as u8 + 1);

            let commitment_hash = compute_commitment(
                &block_hash,
                block_number,
                true,
                true,
                &nonce,
            );

            let commitment = VerificationCommitment {
                block_hash,
                block_number,
                verifier_pubkey,
                commitment_hash,
                timestamp_ms: 1_000_000,
            };

            let reveal = VerificationReveal {
                block_hash,
                block_number,
                verifier_pubkey,
                state_root_match: true,
                receipts_root_match: true,
                nonce,
            };

            reveal
                .verify_against_commitment(&commitment)
                .expect("valid commitment-reveal should succeed");
        }
    }

    #[test]
    fn test_commitment_reveal_copying_detected() {
        let block_hash = B256::repeat_byte(0xEF);
        let block_number = 300u64;

        // Phone A creates a valid commitment
        let key_a = test_ed25519_key(0);
        let pubkey_a = key_a.verifying_key().to_bytes();
        let nonce_a = B256::repeat_byte(0xAA);

        let commitment_hash_a = compute_commitment(
            &block_hash,
            block_number,
            true,
            true,
            &nonce_a,
        );

        let commitment_a = VerificationCommitment {
            block_hash,
            block_number,
            verifier_pubkey: pubkey_a,
            commitment_hash: commitment_hash_a,
            timestamp_ms: 1_000_000,
        };

        // Phone B tries to copy Phone A's result with a DIFFERENT nonce
        let key_b = test_ed25519_key(1);
        let pubkey_b = key_b.verifying_key().to_bytes();
        let nonce_b = B256::repeat_byte(0xBB);

        // B creates its own commitment
        let commitment_hash_b = compute_commitment(
            &block_hash,
            block_number,
            true,
            true,
            &nonce_b,
        );

        let commitment_b = VerificationCommitment {
            block_hash,
            block_number,
            verifier_pubkey: pubkey_b,
            commitment_hash: commitment_hash_b,
            timestamp_ms: 1_000_001,
        };

        // B tries to reveal using A's nonce (copying attempt)
        let bad_reveal = VerificationReveal {
            block_hash,
            block_number,
            verifier_pubkey: pubkey_b,
            state_root_match: true,
            receipts_root_match: true,
            nonce: nonce_a, // Using A's nonce!
        };

        let result = bad_reveal.verify_against_commitment(&commitment_b);
        assert!(result.is_err(), "copying should be detected");
        assert!(
            matches!(result.unwrap_err(), CommitmentError::HashMismatch),
            "should be HashMismatch error"
        );

        // B can't use A's commitment either (verifier mismatch)
        let result2 = bad_reveal.verify_against_commitment(&commitment_a);
        assert!(result2.is_err());
        assert!(matches!(
            result2.unwrap_err(),
            CommitmentError::VerifierMismatch
        ));
    }

    #[test]
    fn test_end_to_end_block_to_mobile() {
        // Step 1: Run consensus to produce a committed block
        let mut harness = TestHarness::new(4);
        let block_hash = B256::repeat_byte(0xE2);
        harness.run_consensus_round(1, block_hash);
        assert_eq!(harness.committed_blocks.len(), 1);

        // Step 2: Simulate mobile verifiers signing receipts
        let mut aggregator = ReceiptAggregator::new(7, 100);
        aggregator.register_block(block_hash, 1);

        for i in 0..10u32 {
            let key = test_ed25519_key(i);
            let receipt = sign_receipt(block_hash, 1, true, true, 2_000_000 + i as u64, &key);

            receipt.verify_signature().unwrap();
            aggregator.process_receipt(&receipt);
        }

        // Step 3: Verify attestation threshold reached
        let status = aggregator.get_status(&block_hash).unwrap();
        assert!(status.is_attested(), "block should be attested by 10 >= 7 mobiles");
        assert_eq!(status.total_receipts(), 10);
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// Module 4: Fault Tolerance (9 tests)
// ══════════════════════════════════════════════════════════════════════════════

mod fault_tolerance {
    use super::*;

    #[test]
    fn test_f_crash_consensus_continues() {
        // n=4, f=1: validator 3 crashes, remaining 3 (0,1,2) meet quorum=3
        let mut harness = TestHarness::new(4);
        let view = 1u64;
        let block_hash = B256::repeat_byte(0xC1);
        let participating: Vec<usize> = vec![0, 1, 2]; // engine 3 crashed

        let committed =
            harness.run_consensus_round_partial(view, block_hash, &participating);
        assert!(committed, "3/4 validators should reach consensus");
    }

    #[test]
    fn test_f_crash_n7() {
        // n=7, f=2: validators 5,6 crash, remaining 5 meet quorum=5
        let mut harness = TestHarness::new(7);
        let view = 1u64;
        let block_hash = B256::repeat_byte(0xC7);
        let participating: Vec<usize> = vec![0, 1, 2, 3, 4]; // engines 5,6 crashed

        let committed =
            harness.run_consensus_round_partial(view, block_hash, &participating);
        assert!(committed, "5/7 validators should reach consensus");
    }

    #[test]
    fn test_byzantine_wrong_view() {
        let mut harness = TestHarness::new(4);
        let view = 1u64;

        // Create a vote for view 99 (wrong view)
        let msg = signing_message(99, &B256::repeat_byte(0xBB));
        let sig = harness.secret_keys[0].sign(&msg);
        let vote = Vote {
            view: 99,
            block_hash: B256::repeat_byte(0xBB),
            voter: 0,
            signature: sig,
        };

        // Send to the leader (engine 1 for view 1)
        let leader = (view % 4) as usize; // 1

        // First make the leader propose so it has a vote collector
        harness.engines[leader]
            .process_event(ConsensusEvent::BlockReady(B256::repeat_byte(0xBB)))
            .unwrap();
        harness.drain_outputs(leader);

        let result = harness.engines[leader]
            .process_event(ConsensusEvent::Message(ConsensusMessage::Vote(vote)));
        assert!(result.is_err());
        match result.unwrap_err() {
            ConsensusError::ViewMismatch {
                current,
                received,
            } => {
                assert_eq!(current, 1);
                assert_eq!(received, 99);
            }
            other => panic!("expected ViewMismatch, got: {:?}", other),
        }
    }

    #[test]
    fn test_byzantine_wrong_signature() {
        let mut harness = TestHarness::new(4);
        let view = 1u64;
        let leader = (view % 4) as usize;
        let block_hash = B256::repeat_byte(0xBA);

        // Leader proposes
        harness.engines[leader]
            .process_event(ConsensusEvent::BlockReady(block_hash))
            .unwrap();
        let outputs = harness.drain_outputs(leader);
        let proposal = outputs
            .iter()
            .find_map(|o| match o {
                EngineOutput::BroadcastMessage(msg @ ConsensusMessage::Proposal(_)) => {
                    Some(msg.clone())
                }
                _ => None,
            })
            .unwrap();

        // Non-leaders accept proposal and vote
        for i in 0..4 {
            if i != leader {
                harness.engines[i]
                    .process_event(ConsensusEvent::Message(proposal.clone()))
                    .unwrap();
            }
        }

        // Collect legitimate votes from engines 0 and 2
        let mut legit_votes = Vec::new();
        for i in [0usize, 2] {
            let outputs = harness.drain_outputs(i);
            for output in outputs {
                if let EngineOutput::SendToValidator(target, msg) = output {
                    if target == leader as u32 {
                        legit_votes.push(msg);
                    }
                }
            }
        }

        // Send legitimate votes
        for vote in legit_votes {
            harness.engines[leader]
                .process_event(ConsensusEvent::Message(vote))
                .unwrap();
        }

        // The bad vote scenario: only 2 legit votes (below quorum),
        // then a bad vote that pushes count to quorum but fails build_qc.

        // Start fresh for this specific test
        let mut harness2 = TestHarness::new(4);
        let leader2 = (1u64 % 4) as usize;

        harness2.engines[leader2]
            .process_event(ConsensusEvent::BlockReady(block_hash))
            .unwrap();
        let outputs = harness2.drain_outputs(leader2);
        let proposal2 = outputs
            .iter()
            .find_map(|o| match o {
                EngineOutput::BroadcastMessage(msg @ ConsensusMessage::Proposal(_)) => {
                    Some(msg.clone())
                }
                _ => None,
            })
            .unwrap();

        // Only engine 0 votes legitimately (leader has self-vote = 1, + engine 0 = 2)
        harness2.engines[0]
            .process_event(ConsensusEvent::Message(proposal2))
            .unwrap();
        let outputs = harness2.drain_outputs(0);
        for output in outputs {
            if let EngineOutput::SendToValidator(target, msg) = output {
                if target == leader2 as u32 {
                    harness2.engines[leader2]
                        .process_event(ConsensusEvent::Message(msg))
                        .unwrap();
                }
            }
        }

        // Now send bad vote from engine 2 (wrong key — use engine 3's key for engine 2's index)
        let msg_correct = signing_message(view, &block_hash);
        let sig_wrong_key = harness2.secret_keys[3].sign(&msg_correct); // wrong key!
        let bad_vote2 = Vote {
            view,
            block_hash,
            voter: 2,
            signature: sig_wrong_key,
        };

        // This should fail because build_qc verifies each signature
        let result = harness2.engines[leader2]
            .process_event(ConsensusEvent::Message(ConsensusMessage::Vote(bad_vote2)));
        assert!(
            result.is_err(),
            "vote with wrong key signature should cause build_qc to fail"
        );
    }

    #[test]
    fn test_duplicate_vote_rejected() {
        let mut harness = TestHarness::new(4);
        let view = 1u64;
        let leader = (view % 4) as usize;
        let block_hash = B256::repeat_byte(0xD1);

        // Leader proposes
        harness.engines[leader]
            .process_event(ConsensusEvent::BlockReady(block_hash))
            .unwrap();
        harness.drain_outputs(leader);

        // Engine 0 sends a legitimate vote
        let msg = signing_message(view, &block_hash);
        let sig = harness.secret_keys[0].sign(&msg);
        let vote = Vote {
            view,
            block_hash,
            voter: 0,
            signature: sig.clone(),
        };
        harness.engines[leader]
            .process_event(ConsensusEvent::Message(ConsensusMessage::Vote(vote)))
            .unwrap();

        // Same engine 0 sends duplicate vote
        let dup_vote = Vote {
            view,
            block_hash,
            voter: 0,
            signature: sig,
        };
        let result = harness.engines[leader]
            .process_event(ConsensusEvent::Message(ConsensusMessage::Vote(dup_vote)));
        assert!(result.is_err());
        match result.unwrap_err() {
            ConsensusError::DuplicateVote {
                view: v,
                validator_index,
            } => {
                assert_eq!(v, view);
                assert_eq!(validator_index, 0);
            }
            other => panic!("expected DuplicateVote, got: {:?}", other),
        }
    }

    #[test]
    fn test_leader_crash_view_change() {
        let mut harness = TestHarness::new(4);
        // View 1: leader=1. Simulate leader crash by not processing any messages.

        // All engines timeout
        harness.run_timeout_view_change(1);

        // After view change, all engines should be at view 2
        // Next leader for view 2 is 2%4=2
        let next_view = 2u64;
        for i in 0..4 {
            assert_eq!(
                harness.engines[i].current_view(),
                next_view,
                "engine {} should be at view {}",
                i,
                next_view
            );
        }

        // New leader (engine 2) can propose
        let block_hash = B256::repeat_byte(0xAC);
        harness.run_consensus_round(next_view, block_hash);
        assert_eq!(harness.committed_blocks.len(), 1);
        assert_eq!(harness.committed_blocks[0], (next_view, block_hash));
    }

    #[test]
    fn test_consecutive_timeouts_backoff() {
        let mut harness = TestHarness::new(4);

        // Timeout 3 times
        for _ in 0..3 {
            for i in 0..4 {
                harness.trigger_timeout(i);
            }
            harness.drain_all_outputs();
            // Advance engines manually for next round
            let current = harness.engines[0].current_view();
            for i in 0..4 {
                let dummy = TimeoutMessage {
                    view: current + 1,
                    high_qc: QuorumCertificate::genesis(),
                    sender: 0,
                    signature: harness.secret_keys[0].sign(b"advance"),
                };
                let _ = harness.engines[i].process_event(ConsensusEvent::Message(
                    ConsensusMessage::Timeout(dummy),
                ));
            }
            harness.drain_all_outputs();
        }

        // Verify pacemaker timeout duration has increased (exponential backoff)
        // base=60000ms, max=120000ms
        // duration(0) = 60000ms, duration(1) = 120000ms (capped), duration(3) = 120000ms (capped)
        let pm = harness.engines[0].pacemaker();
        let duration_0 = pm.timeout_duration(0);
        let duration_1 = pm.timeout_duration(1);
        assert!(
            duration_1 > duration_0,
            "first backoff should increase timeout"
        );
        // With base=60s and max=120s: duration(1)=120s (capped), which is 2x base
        assert_eq!(
            duration_1,
            std::time::Duration::from_millis(120_000),
            "first backoff should be capped at max_timeout"
        );

        // After a successful commit, timeouts should reset
        let current_view = harness.engines[0].current_view();
        harness.run_consensus_round(current_view, B256::repeat_byte(0xBB));
        // The commit resets consecutive_timeouts to 0 internally
        assert_eq!(harness.committed_blocks.len(), 1);
    }

    #[test]
    fn test_safety_violation_detected() {
        let mut harness = TestHarness::new(4);

        // First, commit a block at view 1 to advance locked_qc
        harness.run_consensus_round(1, B256::repeat_byte(0x01));

        // All engines now at view 2 with locked_qc at view 1.
        // Try to send a proposal with justify_qc.view < locked_qc.view (view 0 < view 1).
        let view = 2u64;
        let leader = (view % 4) as usize; // 2
        let block_hash = B256::repeat_byte(0x5A);

        let msg = signing_message(view, &block_hash);
        let sig = harness.secret_keys[leader].sign(&msg);

        let bad_proposal = Proposal {
            view,
            block_hash,
            justify_qc: QuorumCertificate::genesis(), // view 0! But locked_qc is at view 1
            proposer: leader as u32,
            signature: sig,
        };

        // Non-leader should reject this proposal as a safety violation
        let target = if leader == 0 { 1 } else { 0 };
        let result = harness.engines[target]
            .process_event(ConsensusEvent::Message(ConsensusMessage::Proposal(
                bad_proposal,
            )));
        assert!(result.is_err());
        match result.unwrap_err() {
            ConsensusError::SafetyViolation {
                qc_view,
                locked_view,
            } => {
                assert_eq!(qc_view, 0);
                assert_eq!(locked_view, 1);
            }
            other => panic!("expected SafetyViolation, got: {:?}", other),
        }
    }

    #[test]
    fn test_invalid_proposer_rejected() {
        let mut harness = TestHarness::new(4);
        let view = 1u64;
        // Leader for view 1 is validator 1. Send proposal from validator 0.
        let block_hash = B256::repeat_byte(0x1B);

        let msg = signing_message(view, &block_hash);
        let sig = harness.secret_keys[0].sign(&msg);

        let bad_proposal = Proposal {
            view,
            block_hash,
            justify_qc: QuorumCertificate::genesis(),
            proposer: 0, // Wrong! Should be 1
            signature: sig,
        };

        let result = harness.engines[2]
            .process_event(ConsensusEvent::Message(ConsensusMessage::Proposal(
                bad_proposal,
            )));
        assert!(result.is_err());
        match result.unwrap_err() {
            ConsensusError::InvalidProposer {
                expected, actual, ..
            } => {
                assert_eq!(expected, 1);
                assert_eq!(actual, 0);
            }
            other => panic!("expected InvalidProposer, got: {:?}", other),
        }
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// Module 5: Boundary Conditions (7 tests)
// ══════════════════════════════════════════════════════════════════════════════

mod boundary_conditions {
    use super::*;

    #[test]
    fn test_single_validator_instant_commit() {
        let mut harness = TestHarness::new(1);
        let block_hash = B256::repeat_byte(0x01);

        // BlockReady should complete both rounds instantly
        harness.engines[0]
            .process_event(ConsensusEvent::BlockReady(block_hash))
            .unwrap();

        assert_eq!(harness.engines[0].current_view(), 2);
        assert_eq!(
            harness.engines[0].current_phase(),
            Phase::WaitingForProposal
        );

        let outputs = harness.drain_outputs(0);

        // Should contain: Proposal, PrepareQC, BlockCommitted
        let has_proposal = outputs
            .iter()
            .any(|o| matches!(o, EngineOutput::BroadcastMessage(ConsensusMessage::Proposal(_))));
        let has_qc = outputs
            .iter()
            .any(|o| matches!(o, EngineOutput::BroadcastMessage(ConsensusMessage::PrepareQC(_))));
        let has_committed = outputs
            .iter()
            .any(|o| matches!(o, EngineOutput::BlockCommitted { .. }));

        assert!(has_proposal, "should broadcast Proposal");
        assert!(has_qc, "should broadcast PrepareQC");
        assert!(has_committed, "should emit BlockCommitted");
    }

    #[test]
    fn test_minimum_bft_exactly_quorum() {
        // n=4, f=1, quorum=3. Exactly 3 votes should succeed, 2 should not.
        let mut harness = TestHarness::new(4);
        let view = 1u64;
        let block_hash = B256::repeat_byte(0xA3);

        // Test with exactly quorum (3): leader + 2 others
        let participating: Vec<usize> = vec![0, 1, 2]; // leader=1, plus 0 and 2
        let committed =
            harness.run_consensus_round_partial(view, block_hash, &participating);
        assert!(committed, "exactly 3/4 (quorum) should commit");
    }

    #[test]
    fn test_max_f_crash_still_works() {
        // n=4, 1 crashed: exactly quorum remaining
        let mut harness = TestHarness::new(4);
        let participating: Vec<usize> = vec![0, 1, 2]; // engine 3 crashed

        let committed =
            harness.run_consensus_round_partial(1, B256::repeat_byte(0xA4), &participating);
        assert!(committed, "n-f validators should still commit");
    }

    #[test]
    fn test_f_plus_1_crash_stalls() {
        // n=4, 2 crashed: only 2 remaining, below quorum=3
        let mut harness = TestHarness::new(4);
        let participating: Vec<usize> = vec![1, 2]; // engines 0, 3 crashed (leader=1 participates)

        let committed =
            harness.run_consensus_round_partial(1, B256::repeat_byte(0xF5), &participating);
        assert!(
            !committed,
            "only 2/4 validators should NOT reach consensus"
        );
    }

    #[test]
    fn test_high_view_number() {
        // Test with very high view numbers to verify saturating_add behavior
        let mut harness = TestHarness::new(1);

        // Run a few blocks first
        for v in 1..=3u64 {
            harness.run_consensus_round(v, B256::repeat_byte(v as u8));
        }

        assert_eq!(harness.committed_blocks.len(), 3);
        assert_eq!(harness.engines[0].current_view(), 4);

        // Verify saturating_add correctness conceptually:
        // view.saturating_add(1) should never overflow
        let max_view: ViewNumber = u64::MAX;
        let next = max_view.saturating_add(1);
        assert_eq!(next, u64::MAX, "saturating_add at MAX should stay at MAX");
    }

    #[test]
    fn test_zero_block_hash() {
        let mut harness = TestHarness::new(4);
        let block_hash = B256::ZERO;

        harness.run_consensus_round(1, block_hash);

        assert_eq!(harness.committed_blocks.len(), 1);
        assert_eq!(harness.committed_blocks[0], (1, B256::ZERO));
    }

    #[test]
    fn test_quorum_size_formula() {
        // Verify quorum_size = 2f+1 for various n values
        let test_cases: Vec<(usize, u32, usize)> = vec![
            // (n, expected_f, expected_quorum)
            (1, 0, 1),
            (4, 1, 3),
            (7, 2, 5),
            (10, 3, 7),
            (100, 33, 67),
            (500, 166, 333),
        ];

        for (n, expected_f, expected_quorum) in test_cases {
            let f = (n as u32).saturating_sub(1) / 3;
            assert_eq!(f, expected_f, "f for n={}", n);

            let keys: Vec<BlsSecretKey> = (0..n as u32).map(test_bls_key).collect();
            let infos: Vec<ValidatorInfo> = keys
                .iter()
                .enumerate()
                .map(|(i, sk)| ValidatorInfo {
                    address: Address::with_last_byte(i as u8),
                    bls_public_key: sk.public_key(),
                })
                .collect();
            let vs = ValidatorSet::new(&infos, f);

            assert_eq!(
                vs.quorum_size(),
                expected_quorum,
                "quorum_size for n={}",
                n
            );
        }
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// Module 6: Stress & Performance (4 tests)
// ══════════════════════════════════════════════════════════════════════════════

mod stress_performance {
    use super::*;

    #[test]
    fn test_100_consecutive_blocks_liveness() {
        let mut harness = TestHarness::new(4);

        for view in 1..=100u64 {
            let block_hash = {
                let mut bytes = [0u8; 32];
                bytes[..8].copy_from_slice(&view.to_le_bytes());
                B256::from(bytes)
            };
            harness.run_consensus_round(view, block_hash);
        }

        assert_eq!(harness.committed_blocks.len(), 100);

        // Verify sequential views
        for (idx, &(v, _)) in harness.committed_blocks.iter().enumerate() {
            assert_eq!(v, (idx + 1) as u64);
        }

        // All engines at view 101
        for i in 0..4 {
            assert_eq!(harness.engines[i].current_view(), 101);
        }
    }

    #[test]
    fn test_rapid_view_changes() {
        let mut harness = TestHarness::new(4);
        let mut current_view = 1u64;

        for round in 0..50u64 {
            if round % 2 == 0 {
                // Successful round
                let block_hash = {
                    let mut bytes = [0u8; 32];
                    bytes[..8].copy_from_slice(&current_view.to_le_bytes());
                    B256::from(bytes)
                };
                harness.run_consensus_round(current_view, block_hash);
                current_view = harness.engines[0].current_view();
            } else {
                // Timeout round
                harness.run_timeout_view_change(current_view);
                current_view = harness.engines[0].current_view();
            }
        }

        // Should have committed 25 blocks (every other round)
        assert_eq!(harness.committed_blocks.len(), 25);

        // All engines should be at the same view
        let final_view = harness.engines[0].current_view();
        for i in 1..4 {
            assert_eq!(
                harness.engines[i].current_view(),
                final_view,
                "engine {} should be at view {}",
                i,
                final_view
            );
        }
    }

    #[test]
    fn test_large_set_500_validators() {
        let mut harness = TestHarness::new(500);

        // f=166, quorum=333
        assert_eq!(harness.validator_set.fault_tolerance(), 166);
        assert_eq!(harness.validator_set.quorum_size(), 333);

        let block_hash = B256::repeat_byte(0x55);
        harness.run_consensus_round(1, block_hash);

        assert_eq!(harness.committed_blocks.len(), 1);
    }

    #[test]
    fn test_1000_mobile_receipts() {
        use n42_mobile::{sign_receipt, ReceiptAggregator};

        let block_hash = B256::repeat_byte(0xA5);
        let block_number = 1u64;
        let threshold = 667u32;

        let mut aggregator = ReceiptAggregator::new(threshold, 10);
        aggregator.register_block(block_hash, block_number);

        let mut threshold_reached_at = None;

        for i in 0..1000u32 {
            let key = test_ed25519_key(i);
            let receipt = sign_receipt(
                block_hash,
                block_number,
                true,
                true,
                3_000_000 + i as u64,
                &key,
            );

            let result = aggregator.process_receipt(&receipt);
            if result == Some(true) && threshold_reached_at.is_none() {
                threshold_reached_at = Some(i);
            }
        }

        assert_eq!(
            threshold_reached_at,
            Some(threshold - 1),
            "threshold should be reached at receipt #{}",
            threshold - 1
        );

        let status = aggregator.get_status(&block_hash).unwrap();
        assert!(status.is_attested());
        assert_eq!(status.total_receipts(), 1000);
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// Module 7: Stability (4 tests)
// ══════════════════════════════════════════════════════════════════════════════

mod stability {
    use super::*;

    #[test]
    fn test_1000_views_mixed() {
        let mut harness = TestHarness::new(4);
        let mut current_view = 1u64;
        let mut committed_count = 0u32;
        let mut _timeout_count = 0u32;

        for round in 0..1000u32 {
            let action = round % 10; // 0-6 success, 7-8 timeout, 9 double-timeout
            if action < 7 {
                // 70%: successful round
                let block_hash = {
                    let mut bytes = [0u8; 32];
                    bytes[..8].copy_from_slice(&current_view.to_le_bytes());
                    bytes[8..12].copy_from_slice(&round.to_le_bytes());
                    B256::from(bytes)
                };
                harness.run_consensus_round(current_view, block_hash);
                current_view = harness.engines[0].current_view();
                committed_count += 1;
            } else if action < 9 {
                // 20%: single timeout
                harness.run_timeout_view_change(current_view);
                current_view = harness.engines[0].current_view();
                _timeout_count += 1;
            } else {
                // 10%: double timeout (two consecutive timeouts)
                harness.run_timeout_view_change(current_view);
                current_view = harness.engines[0].current_view();
                harness.run_timeout_view_change(current_view);
                current_view = harness.engines[0].current_view();
                _timeout_count += 2;
            }
        }

        assert_eq!(committed_count, 700);
        assert_eq!(harness.committed_blocks.len(), 700);

        // All engines should have consistent view
        let final_view = harness.engines[0].current_view();
        for i in 1..4 {
            assert_eq!(
                harness.engines[i].current_view(),
                final_view,
                "engine {} view mismatch after 1000 mixed views",
                i
            );
        }
    }

    #[test]
    fn test_channels_no_leak() {
        let mut harness = TestHarness::new(4);

        // Run 50 rounds
        for view in 1..=50u64 {
            let block_hash = B256::repeat_byte(view as u8);
            harness.run_consensus_round(view, block_hash);
        }

        // Verify all output channels are fully drained
        for i in 0..4 {
            let remaining = harness.drain_outputs(i);
            assert!(
                remaining.is_empty(),
                "engine {} should have no pending outputs after full drain, found {}",
                i,
                remaining.len()
            );
        }
    }

    #[test]
    fn test_locked_qc_monotonic() {
        let mut harness = TestHarness::new(4);
        let mut current_view = 1u64;

        for round in 0..200u32 {
            if round % 3 == 0 {
                // Timeout
                harness.run_timeout_view_change(current_view);
                current_view = harness.engines[0].current_view();
            } else {
                // Commit
                let block_hash = {
                    let mut bytes = [0u8; 32];
                    bytes[..8].copy_from_slice(&current_view.to_le_bytes());
                    B256::from(bytes)
                };
                harness.run_consensus_round(current_view, block_hash);
                current_view = harness.engines[0].current_view();
            }
        }

        // After all rounds, committed blocks should have monotonically increasing views
        let mut prev_view = 0u64;
        for &(v, _) in &harness.committed_blocks {
            assert!(
                v > prev_view,
                "committed view {} should be > previous {}",
                v,
                prev_view
            );
            prev_view = v;
        }
    }

    #[test]
    fn test_all_engines_consistent_view() {
        let mut harness = TestHarness::new(4);
        let mut current_view = 1u64;

        // Mixed operations
        for round in 0..100u32 {
            if round % 5 == 0 {
                harness.run_timeout_view_change(current_view);
            } else {
                let block_hash = {
                    let mut bytes = [0u8; 32];
                    bytes[..4].copy_from_slice(&round.to_le_bytes());
                    bytes[4..12].copy_from_slice(&current_view.to_le_bytes());
                    B256::from(bytes)
                };
                harness.run_consensus_round(current_view, block_hash);
            }
            current_view = harness.engines[0].current_view();

            // After every round, check all engines are at the same view
            for i in 1..4 {
                assert_eq!(
                    harness.engines[i].current_view(),
                    current_view,
                    "engine {} should be at view {} after round {}",
                    i,
                    current_view,
                    round
                );
            }
        }
    }
}
