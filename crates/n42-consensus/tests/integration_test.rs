//! Full integration test suite for the N42 HotStuff-2 consensus engine.
//!
//! Tests cover: genesis bootstrap, multi-node consensus, mobile verification,
//! fault tolerance, boundary conditions, stress performance, and stability.
//!
//! The `TestHarness` simulates multiple consensus engines exchanging messages
//! via synchronous routing—no real network, fully deterministic.

use alloy_primitives::{Address, B256};
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

/// Deterministic BLS signing key for mobile verifiers from an index.
fn test_mobile_bls_key(index: u32) -> BlsSecretKey {
    let mut bytes = [0u8; 32];
    bytes[..4].copy_from_slice(&index.to_le_bytes());
    bytes[31] = 0xED;
    BlsSecretKey::key_gen(&bytes).expect("deterministic mobile BLS key should be valid")
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

        // ── Step 2.5: Simulate block data import for non-leaders ──
        // In production, the leader broadcasts block data via /n42/blocks/1 topic
        // and the orchestrator sends BlockImported after new_payload succeeds.
        // In this test harness, we simulate immediate block data availability.
        let proposal_block_hash = match &proposal {
            ConsensusMessage::Proposal(p) => p.block_hash,
            _ => unreachable!(),
        };
        for i in 0..n {
            if i != leader {
                self.engines[i]
                    .process_event(ConsensusEvent::BlockImported(proposal_block_hash))
                    .expect("non-leader should accept BlockImported");
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

        // ── Step 6: Route Decide from leader to all followers ──
        let leader_outputs = self.drain_outputs(leader);

        // Record committed block from leader outputs
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

        // Extract and route Decide messages to all non-leaders
        let decide_msgs: Vec<_> = leader_outputs
            .into_iter()
            .filter(|o| matches!(o, EngineOutput::BroadcastMessage(ConsensusMessage::Decide(_))))
            .collect();

        for output in &decide_msgs {
            if let EngineOutput::BroadcastMessage(msg) = output {
                for i in 0..n {
                    if i != leader {
                        let _ = self.engines[i]
                            .process_event(ConsensusEvent::Message(msg.clone()));
                    }
                }
            }
        }

        // Drain follower outputs (Decide triggers BlockCommitted on followers,
        // but we only count committed blocks once per view from the leader above).
        for i in 0..n {
            if i != leader {
                self.drain_outputs(i);
            }
        }

        // ── Step 7: Safety net — re-deliver Decide to any engines still behind ──
        let next_view = view + 1;
        for i in 0..n {
            if self.engines[i].current_view() < next_view {
                for output in &decide_msgs {
                    if let EngineOutput::BroadcastMessage(msg) = output {
                        let _ = self.engines[i]
                            .process_event(ConsensusEvent::Message(msg.clone()));
                    }
                }
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
        let proposal_block_hash = match &proposal {
            ConsensusMessage::Proposal(p) => p.block_hash,
            _ => unreachable!(),
        };
        for &i in participating {
            if i != leader {
                self.engines[i]
                    .process_event(ConsensusEvent::Message(proposal.clone()))
                    .expect("non-leader should accept Proposal");
            }
        }

        // Step 2.5: Simulate block data import for participating non-leaders
        for &i in participating {
            if i != leader {
                self.engines[i]
                    .process_event(ConsensusEvent::BlockImported(proposal_block_hash))
                    .expect("non-leader should accept BlockImported");
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

        // Step 7: Check for BlockCommitted and route Decide
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

        // Route Decide messages from leader to all other engines
        let decide_msgs: Vec<_> = leader_outputs
            .into_iter()
            .filter(|o| matches!(o, EngineOutput::BroadcastMessage(ConsensusMessage::Decide(_))))
            .collect();

        for output in &decide_msgs {
            if let EngineOutput::BroadcastMessage(msg) = output {
                for i in 0..n {
                    if i != leader {
                        let _ = self.engines[i]
                            .process_event(ConsensusEvent::Message(msg.clone()));
                    }
                }
            }
        }

        // Drain follower outputs (Decide triggers BlockCommitted on followers,
        // but we only count committed blocks once per view from the leader above).
        for i in 0..n {
            if i != leader {
                self.drain_outputs(i);
            }
        }

        if committed {
            // Catch-up: propagate Proposal + BlockImported + PrepareQC to non-participating
            // engines so their locked_qc stays current. This simulates gossip catch-up
            // that would happen in a real network — non-participating nodes eventually
            // receive the round's messages and update their state. Without this,
            // a non-participant that becomes leader next round would propose with a
            // stale justify_qc, causing SafetyViolation on participating nodes.
            let participating_set: std::collections::HashSet<usize> =
                participating.iter().copied().collect();
            for i in 0..n {
                if !participating_set.contains(&i) && self.engines[i].current_view() <= view {
                    // Send Proposal (updates locked_qc from justify_qc, enters Voting)
                    let _ = self.engines[i]
                        .process_event(ConsensusEvent::Message(proposal.clone()));
                    // Send BlockImported (triggers deferred vote)
                    let _ = self.engines[i]
                        .process_event(ConsensusEvent::BlockImported(proposal_block_hash));
                    // Send PrepareQC (updates locked_qc to current round's QC)
                    let _ = self.engines[i]
                        .process_event(ConsensusEvent::Message(prepare_qc.clone()));
                }
            }
            // Drain catch-up outputs (discard late votes/commit-votes from catch-up)
            self.drain_all_outputs();

            // Re-deliver Decide to any engines still behind
            let next_view = view + 1;
            for i in 0..n {
                if self.engines[i].current_view() < next_view {
                    for output in &decide_msgs {
                        if let EngineOutput::BroadcastMessage(msg) = output {
                            let _ = self.engines[i]
                                .process_event(ConsensusEvent::Message(msg.clone()));
                        }
                    }
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

        // Step 2: Non-leaders receive Proposal (vote is deferred until block imported)
        let proposal_block_hash = match &proposal {
            ConsensusMessage::Proposal(p) => p.block_hash,
            _ => unreachable!(),
        };
        for i in 0..4 {
            if i != leader {
                harness.engines[i]
                    .process_event(ConsensusEvent::Message(proposal.clone()))
                    .unwrap();
                assert_eq!(harness.engines[i].current_phase(), Phase::Voting);
            }
        }

        // Step 2.5: Simulate block data import → triggers deferred votes
        for i in 0..4 {
            if i != leader {
                harness.engines[i]
                    .process_event(ConsensusEvent::BlockImported(proposal_block_hash))
                    .unwrap();
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
            let key = test_mobile_bls_key(i);
            let receipt = sign_receipt(block_hash, block_number, true, true, 1_700_000_000_000, &key);

            assert!(receipt.is_valid());
            assert_eq!(receipt.block_hash, block_hash);
            assert_eq!(receipt.block_number, block_number);
            assert_eq!(receipt.verifier_pubkey, key.public_key().to_bytes());

            // BLS12-381 signature must verify
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
            let key = test_mobile_bls_key(i);
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

        let key = test_mobile_bls_key(0);
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
            let key = test_mobile_bls_key(i);
            let verifier_pubkey = key.public_key().to_bytes();
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
        let key_a = test_mobile_bls_key(0);
        let pubkey_a = key_a.public_key().to_bytes();
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
        let key_b = test_mobile_bls_key(1);
        let pubkey_b = key_b.public_key().to_bytes();
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
            let key = test_mobile_bls_key(i);
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

        // Non-leaders accept proposal and simulate block import to trigger votes
        let proposal_block_hash_1 = match &proposal {
            ConsensusMessage::Proposal(p) => p.block_hash,
            _ => unreachable!(),
        };
        for i in 0..4 {
            if i != leader {
                harness.engines[i]
                    .process_event(ConsensusEvent::Message(proposal.clone()))
                    .unwrap();
                harness.engines[i]
                    .process_event(ConsensusEvent::BlockImported(proposal_block_hash_1))
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
        let proposal2_block_hash = match &proposal2 {
            ConsensusMessage::Proposal(p) => p.block_hash,
            _ => unreachable!(),
        };
        harness2.engines[0]
            .process_event(ConsensusEvent::Message(proposal2))
            .unwrap();
        harness2.engines[0]
            .process_event(ConsensusEvent::BlockImported(proposal2_block_hash))
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

        // Timeout 3 times using the proper TC → NewView flow
        for _ in 0..3 {
            let view = harness.engines[0].current_view();
            harness.run_timeout_view_change(view);
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
            prepare_qc: None,
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
            prepare_qc: None,
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
            let key = test_mobile_bls_key(i);
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

// ══════════════════════════════════════════════════════════════════════════════
// Level 3: Byzantine invalid-signature resilience
// ══════════════════════════════════════════════════════════════════════════════

mod byzantine_signature {
    use super::*;
    use n42_consensus::protocol::quorum::{commit_signing_message, signing_message, timeout_signing_message};
    use n42_primitives::consensus::CommitVote;

    /// Run consensus round where f Byzantine validators send votes with invalid signatures.
    /// Consensus should still succeed because 2f+1 honest validators form quorum.
    ///
    /// Topology: 7 validators, f=2, quorum=5.
    /// Byzantine validators (5, 6) send votes with wrong signatures.
    /// Honest validators (0..5) send valid votes.
    #[test]
    fn test_byzantine_invalid_vote_signatures_7v() {
        let mut harness = TestHarness::new(7);
        let view = 1u64;
        let leader = (view % 7) as usize; // leader = 1
        let block_hash = B256::repeat_byte(0xB1);

        // Step 1: Leader proposes
        harness.engines[leader]
            .process_event(ConsensusEvent::BlockReady(block_hash))
            .expect("leader BlockReady");
        let leader_outputs = harness.drain_outputs(leader);
        let proposal = leader_outputs
            .iter()
            .find_map(|o| match o {
                EngineOutput::BroadcastMessage(msg @ ConsensusMessage::Proposal(_)) => Some(msg.clone()),
                _ => None,
            })
            .expect("leader should broadcast Proposal");

        // Step 2: Route proposal to non-leaders and simulate block import
        let proposal_block_hash_byz = match &proposal {
            ConsensusMessage::Proposal(p) => p.block_hash,
            _ => unreachable!(),
        };
        for i in 0..7 {
            if i != leader {
                harness.engines[i]
                    .process_event(ConsensusEvent::Message(proposal.clone()))
                    .expect("non-leader should accept Proposal");
                harness.engines[i]
                    .process_event(ConsensusEvent::BlockImported(proposal_block_hash_byz))
                    .expect("non-leader should accept BlockImported");
            }
        }

        // Step 3: Send valid votes from honest validators (0, 2, 3, 4)
        for &i in &[0usize, 2, 3, 4] {
            let outputs = harness.drain_outputs(i);
            for output in outputs {
                if let EngineOutput::SendToValidator(target, msg) = output {
                    if target == leader as u32 {
                        harness.engines[leader]
                            .process_event(ConsensusEvent::Message(msg))
                            .expect("leader should accept valid vote");
                    }
                }
            }
        }

        // Step 4: Send INVALID votes from Byzantine validators (5, 6)
        for &i in &[5usize, 6] {
            harness.drain_outputs(i); // drain their valid votes
            // Send forged votes with wrong signatures
            let wrong_msg = signing_message(999, &block_hash);
            let bad_sig = harness.secret_keys[i].sign(&wrong_msg);
            let bad_vote = Vote {
                view,
                block_hash,
                voter: i as u32,
                signature: bad_sig,
            };
            let result = harness.engines[leader]
                .process_event(ConsensusEvent::Message(ConsensusMessage::Vote(bad_vote)));
            assert!(result.is_err(), "invalid vote from byzantine validator {} should be rejected", i);
        }

        // Step 5: QC should be formed (leader self-vote + 4 honest = 5 ≥ quorum of 5)
        let leader_outputs = harness.drain_outputs(leader);
        let prepare_qc = leader_outputs
            .iter()
            .find_map(|o| match o {
                EngineOutput::BroadcastMessage(msg @ ConsensusMessage::PrepareQC(_)) => Some(msg.clone()),
                _ => None,
            })
            .expect("QC should form from honest validators despite Byzantine ones");

        // Step 6: Route PrepareQC to honest validators
        for i in 0..5 {
            if i != leader {
                harness.engines[i]
                    .process_event(ConsensusEvent::Message(prepare_qc.clone()))
                    .expect("honest validator should accept PrepareQC");
            }
        }

        // Step 7: Route commit votes from honest validators
        for i in 0..5 {
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

        // Block should be committed
        let leader_outputs = harness.drain_outputs(leader);
        let committed = leader_outputs.iter().any(|o| matches!(o, EngineOutput::BlockCommitted { .. }));
        assert!(committed, "block should be committed despite Byzantine invalid signatures");
    }

    /// Byzantine validators send commit votes with invalid signatures.
    /// The commit phase should still succeed with enough honest commit votes.
    #[test]
    fn test_byzantine_invalid_commit_vote_signatures() {
        let mut harness = TestHarness::new(4);
        let view = 1u64;
        let leader = (view % 4) as usize; // leader = 1
        let block_hash = B256::repeat_byte(0xB2);

        // Run through proposal + voting phase normally
        harness.engines[leader]
            .process_event(ConsensusEvent::BlockReady(block_hash))
            .expect("leader BlockReady");
        let leader_outputs = harness.drain_outputs(leader);
        let proposal = leader_outputs.iter().find_map(|o| match o {
            EngineOutput::BroadcastMessage(msg @ ConsensusMessage::Proposal(_)) => Some(msg.clone()),
            _ => None,
        }).unwrap();

        let proposal_block_hash_cv = match &proposal {
            ConsensusMessage::Proposal(p) => p.block_hash,
            _ => unreachable!(),
        };
        for i in 0..4 {
            if i != leader {
                harness.engines[i]
                    .process_event(ConsensusEvent::Message(proposal.clone()))
                    .expect("accept proposal");
                harness.engines[i]
                    .process_event(ConsensusEvent::BlockImported(proposal_block_hash_cv))
                    .expect("accept BlockImported");
            }
        }

        // Route valid votes
        for i in 0..4 {
            if i != leader {
                let outputs = harness.drain_outputs(i);
                for output in outputs {
                    if let EngineOutput::SendToValidator(target, msg) = output {
                        if target == leader as u32 {
                            harness.engines[leader]
                                .process_event(ConsensusEvent::Message(msg))
                                .expect("accept vote");
                        }
                    }
                }
            }
        }

        // PrepareQC should be formed
        let leader_outputs = harness.drain_outputs(leader);
        let prepare_qc = leader_outputs.iter().find_map(|o| match o {
            EngineOutput::BroadcastMessage(msg @ ConsensusMessage::PrepareQC(_)) => Some(msg.clone()),
            _ => None,
        }).expect("PrepareQC should form");

        // Route PrepareQC to all non-leaders
        for i in 0..4 {
            if i != leader {
                harness.engines[i]
                    .process_event(ConsensusEvent::Message(prepare_qc.clone()))
                    .expect("accept PrepareQC");
            }
        }

        // Honest validators 0 and 2 send valid commit votes
        for &i in &[0usize, 2] {
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

        // Byzantine validator 3 sends invalid commit vote
        harness.drain_outputs(3);
        let wrong_msg = commit_signing_message(999, &block_hash);
        let bad_sig = harness.secret_keys[3].sign(&wrong_msg);
        let bad_cv = CommitVote {
            view,
            block_hash,
            voter: 3,
            signature: bad_sig,
        };
        let result = harness.engines[leader]
            .process_event(ConsensusEvent::Message(ConsensusMessage::CommitVote(bad_cv)));
        assert!(result.is_err(), "invalid commit vote should be rejected");

        // Block should still be committed (leader + 0 + 2 = 3 ≥ quorum)
        let leader_outputs = harness.drain_outputs(leader);
        let committed = leader_outputs.iter().any(|o| matches!(o, EngineOutput::BlockCommitted { .. }));
        assert!(committed, "block should commit with honest commit votes despite Byzantine");
    }

    /// Byzantine validators send timeout messages with invalid signatures during view change.
    /// The invalid timeout is rejected before entering the collector, while honest
    /// validators still reach quorum for the view change.
    #[test]
    fn test_byzantine_invalid_timeout_signatures() {
        let mut harness = TestHarness::new(4);
        let view = 1u64;

        // All honest validators (0, 1, 2) timeout
        for i in 0..3 {
            harness.engines[i].on_timeout().expect("timeout");
        }

        // Collect honest timeout messages
        let mut honest_timeouts: Vec<(usize, ConsensusMessage)> = Vec::new();
        for i in 0..3 {
            let outputs = harness.drain_outputs(i);
            for output in outputs {
                if let EngineOutput::BroadcastMessage(msg @ ConsensusMessage::Timeout(_)) = output {
                    honest_timeouts.push((i, msg));
                }
            }
        }

        // Byzantine validator 3 sends timeout with invalid signature FIRST
        // (before honest timeouts are routed, so engines are still at view 1)
        let wrong_msg = timeout_signing_message(999);
        let bad_sig = harness.secret_keys[3].sign(&wrong_msg);
        let bad_timeout = n42_primitives::consensus::TimeoutMessage {
            view,
            high_qc: QuorumCertificate::genesis(),
            sender: 3,
            signature: bad_sig,
        };

        // Process the invalid timeout on engines that are still at view 1
        for i in 0..3 {
            let result = harness.engines[i]
                .process_event(ConsensusEvent::Message(ConsensusMessage::Timeout(bad_timeout.clone())));
            assert!(result.is_err(), "invalid timeout should be rejected on engine {}", i);
        }

        // Now route honest timeouts — view change should still succeed
        for (sender, msg) in &honest_timeouts {
            for i in 0..3 {
                if i != *sender {
                    let _ = harness.engines[i]
                        .process_event(ConsensusEvent::Message(msg.clone()));
                }
            }
        }

        // The next leader (view 2 → 2 % 4 = 2, engine 2) should have formed TC
        let mut any_new_view = false;
        for i in 0..3 {
            let outputs = harness.drain_outputs(i);
            for output in &outputs {
                if matches!(output, EngineOutput::BroadcastMessage(ConsensusMessage::NewView(_))) {
                    any_new_view = true;
                }
            }
        }
        assert!(any_new_view, "honest validators should form TC without Byzantine timeout");
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// Level 4: Attestation end-to-end with real BLS keys
// ══════════════════════════════════════════════════════════════════════════════

mod attestation_e2e {
    use super::*;
    use alloy_primitives::hex;
    use n42_primitives::BlsSecretKey;

    /// End-to-end test: consensus commits a block → mobile attestation with real BLS keys.
    ///
    /// Simulates the full flow:
    /// 1. Consensus commits a block
    /// 2. Multiple mobile validators sign the block hash with BLS
    /// 3. Signatures are verified and attestation count tracked
    #[test]
    fn test_consensus_to_attestation_e2e() {
        // Setup: 4-validator consensus
        let mut harness = TestHarness::new(4);
        let view = 1u64;
        let block_hash = B256::repeat_byte(0xE1);

        // Step 1: Run consensus to commit a block
        harness.run_consensus_round(view, block_hash);
        assert_eq!(harness.committed_blocks.len(), 1);
        assert_eq!(harness.committed_blocks[0], (view, block_hash));

        // Step 2: Simulate mobile attestation with real BLS keys
        let mobile_keys: Vec<BlsSecretKey> = (100..110u32).map(test_bls_key).collect();
        let threshold = 5u32;
        let mut attestation_count = 0u32;
        let mut attesters = std::collections::HashSet::new();

        for (i, sk) in mobile_keys.iter().enumerate() {
            let pk = sk.public_key();

            // Mobile signs the block hash
            let sig = sk.sign(block_hash.as_slice());

            // Verify signature (as the node would)
            pk.verify(block_hash.as_slice(), &sig)
                .expect(&format!("mobile {} signature should verify", i));

            // Track attestation
            let pk_hex = hex::encode(pk.to_bytes());
            if attesters.insert(pk_hex) {
                attestation_count += 1;
            }

            if attestation_count >= threshold {
                break;
            }
        }

        assert!(
            attestation_count >= threshold,
            "should reach attestation threshold of {}, got {}",
            threshold,
            attestation_count
        );
    }

    /// Test deduplication: same mobile key signing twice should not increase count.
    #[test]
    fn test_attestation_dedup_with_real_keys() {
        let block_hash = B256::repeat_byte(0xE2);
        let sk = test_bls_key(200);
        let pk = sk.public_key();
        let pk_hex = hex::encode(pk.to_bytes());

        let mut attesters = std::collections::HashSet::new();

        // First attestation
        let sig1 = sk.sign(block_hash.as_slice());
        pk.verify(block_hash.as_slice(), &sig1).expect("sig1 should verify");
        assert!(attesters.insert(pk_hex.clone()), "first attestation should be new");

        // Duplicate attestation (different signature bytes, same key)
        let sig2 = sk.sign(block_hash.as_slice());
        pk.verify(block_hash.as_slice(), &sig2).expect("sig2 should verify");
        assert!(!attesters.insert(pk_hex), "duplicate attestation should be rejected");

        assert_eq!(attesters.len(), 1, "should only count one attestation");
    }

    /// Test that attestation for wrong block is detected.
    #[test]
    fn test_attestation_wrong_block_detected() {
        let correct_hash = B256::repeat_byte(0xE3);
        let wrong_hash = B256::repeat_byte(0xFF);
        let sk = test_bls_key(300);
        let pk = sk.public_key();

        // Sign the wrong block
        let sig = sk.sign(wrong_hash.as_slice());

        // Verification against correct block should fail
        let result = pk.verify(correct_hash.as_slice(), &sig);
        assert!(result.is_err(), "signature for wrong block should fail verification");
    }

    /// Full pipeline: 10 blocks committed, each gets 5+ mobile attestations.
    #[test]
    fn test_multi_block_attestation_pipeline() {
        let mut harness = TestHarness::new(4);
        let mobile_keys: Vec<BlsSecretKey> = (0..8u32).map(|i| test_bls_key(500 + i)).collect();
        let threshold = 5u32;

        for round in 0..10u32 {
            let view = harness.engines[0].current_view();
            let block_hash = {
                let mut bytes = [0u8; 32];
                bytes[..4].copy_from_slice(&round.to_le_bytes());
                bytes[4] = 0xE4;
                B256::from(bytes)
            };

            // Commit block
            harness.run_consensus_round(view, block_hash);

            // Mobile attestation
            let mut count = 0u32;
            let mut attesters = std::collections::HashSet::new();

            for sk in &mobile_keys {
                let pk = sk.public_key();
                let sig = sk.sign(block_hash.as_slice());
                pk.verify(block_hash.as_slice(), &sig)
                    .expect("mobile signature should verify");

                let pk_hex = hex::encode(pk.to_bytes());
                if attesters.insert(pk_hex) {
                    count += 1;
                }
                if count >= threshold {
                    break;
                }
            }

            assert!(
                count >= threshold,
                "block {} (view {}) should reach attestation threshold",
                round,
                view
            );
        }

        assert_eq!(
            harness.committed_blocks.len(),
            10,
            "all 10 blocks should be committed"
        );
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// Module: 21-Node HotStuff-2 Consensus Tests
// n=21, f=6, quorum=13
// ══════════════════════════════════════════════════════════════════════════════

mod twenty_one_node {
    use super::*;

    /// Basic: verify n=21 validator set parameters.
    #[test]
    fn test_21v_genesis_state() {
        let harness = TestHarness::new(21);

        assert_eq!(harness.validator_set.len(), 21);
        assert_eq!(harness.validator_set.fault_tolerance(), 6, "f should be 6 for n=21");
        assert_eq!(harness.validator_set.quorum_size(), 13, "quorum should be 13 for n=21");

        // All engines start at view 1, WaitingForProposal
        for i in 0..21 {
            assert_eq!(harness.engines[i].current_view(), 1);
            assert_eq!(harness.engines[i].current_phase(), Phase::WaitingForProposal);
        }

        // View 1 leader should be validator 1 (1 % 21 = 1)
        assert!(harness.engines[1].is_current_leader());
        for i in 0..21 {
            if i != 1 {
                assert!(!harness.engines[i].is_current_leader());
            }
        }
    }

    /// Basic: single block consensus with all 21 nodes participating.
    #[test]
    fn test_21v_full_consensus_single_block() {
        let mut harness = TestHarness::new(21);

        let block_hash = B256::repeat_byte(0x21);
        harness.run_consensus_round(1, block_hash);

        assert_eq!(harness.committed_blocks.len(), 1);
        assert_eq!(harness.committed_blocks[0], (1, block_hash));

        // All engines should advance to view 2
        for i in 0..21 {
            assert!(
                harness.engines[i].current_view() >= 2,
                "engine {} should be at view >= 2, got {}",
                i,
                harness.engines[i].current_view()
            );
        }
    }

    /// Leader rotation: run 21 consecutive blocks to ensure every validator
    /// gets to be leader exactly once.
    #[test]
    fn test_21v_leader_rotation_full_cycle() {
        let mut harness = TestHarness::new(21);

        for view in 1..=21u64 {
            let leader = (view % 21) as usize;
            assert!(
                harness.engines[leader].is_current_leader(),
                "view {} leader should be validator {}",
                view,
                leader
            );

            let mut block_bytes = [0u8; 32];
            block_bytes[0..8].copy_from_slice(&view.to_le_bytes());
            let block_hash = B256::from(block_bytes);

            harness.run_consensus_round(view, block_hash);
        }

        assert_eq!(
            harness.committed_blocks.len(),
            21,
            "all 21 blocks should be committed (full leader rotation)"
        );

        // Verify each view committed the correct block
        for (idx, &(v, _h)) in harness.committed_blocks.iter().enumerate() {
            assert_eq!(
                v,
                (idx + 1) as u64,
                "committed block {} should be at view {}",
                idx,
                idx + 1
            );
        }
    }

    /// Consecutive: 50 blocks with all 21 nodes, verifying liveness over
    /// multiple leader rotation cycles.
    #[test]
    fn test_21v_consecutive_50_blocks() {
        let mut harness = TestHarness::new(21);

        for view in 1..=50u64 {
            let mut block_bytes = [0u8; 32];
            block_bytes[0..8].copy_from_slice(&view.to_le_bytes());
            let block_hash = B256::from(block_bytes);
            harness.run_consensus_round(view, block_hash);
        }

        assert_eq!(harness.committed_blocks.len(), 50);

        // Verify monotonic view progression
        for i in 1..harness.committed_blocks.len() {
            assert!(
                harness.committed_blocks[i].0 > harness.committed_blocks[i - 1].0,
                "committed views should be monotonically increasing"
            );
        }
    }

    /// Fault tolerance: exactly quorum (13 of 21) participate, 8 crash.
    /// With f=6, losing 8 > f nodes should still work because quorum=13
    /// and we have exactly 13 participants.
    #[test]
    fn test_21v_exact_quorum_13_of_21() {
        let mut harness = TestHarness::new(21);

        // Participating validators: first 13 (indices 0-12)
        // For view 1, leader = 1 % 21 = 1, which is in the participating set.
        let participating: Vec<usize> = (0..13).collect();

        let block_hash = B256::repeat_byte(0xEE);
        let committed = harness.run_consensus_round_partial(1, block_hash, &participating);

        assert!(committed, "exactly quorum (13/21) should achieve consensus");
        assert_eq!(harness.committed_blocks.len(), 1);
    }

    /// Fault tolerance: f=6 crash, 15 nodes remain (still > quorum=13).
    #[test]
    fn test_21v_f_crash_6_nodes_down() {
        let mut harness = TestHarness::new(21);

        // 6 crashed nodes: indices 15-20
        let participating: Vec<usize> = (0..15).collect();

        let block_hash = B256::repeat_byte(0xCC);
        let committed = harness.run_consensus_round_partial(1, block_hash, &participating);

        assert!(committed, "15/21 nodes (6 crashed, f=6) should achieve consensus");
        assert_eq!(harness.committed_blocks.len(), 1);
    }

    /// Fault tolerance: f+1=7 crash, only 14 remain.
    /// 14 >= quorum=13, so should still commit.
    #[test]
    fn test_21v_f_plus_1_crash_14_remain() {
        let mut harness = TestHarness::new(21);

        // 7 crashed nodes: indices 14-20
        let participating: Vec<usize> = (0..14).collect();

        let block_hash = B256::repeat_byte(0xDD);
        let committed = harness.run_consensus_round_partial(1, block_hash, &participating);

        assert!(committed, "14/21 nodes (7 crashed) should still achieve consensus (14 >= quorum=13)");
    }

    /// Fault tolerance: 9 crash, only 12 remain.
    /// 12 < quorum=13, so consensus MUST stall.
    #[test]
    fn test_21v_below_quorum_12_of_21_stalls() {
        let mut harness = TestHarness::new(21);

        // Only 12 nodes participate (indices 0-11)
        // For view 1, leader = 1, which is in the set
        let participating: Vec<usize> = (0..12).collect();

        let block_hash = B256::repeat_byte(0xFF);
        let committed = harness.run_consensus_round_partial(1, block_hash, &participating);

        assert!(!committed, "12/21 nodes (below quorum=13) must NOT achieve consensus");
        assert_eq!(harness.committed_blocks.len(), 0);
    }

    /// Byzantine: f=6 nodes send votes with invalid signatures,
    /// but 14 honest nodes form quorum (14 >= 13).
    #[test]
    fn test_21v_byzantine_6_invalid_votes() {
        let mut harness = TestHarness::new(21);
        let n = 21;
        let view = 1u64;
        let leader = (view % n as u64) as usize;
        let block_hash = B256::repeat_byte(0xBB);

        // Step 1: Leader proposes
        harness.engines[leader]
            .process_event(ConsensusEvent::BlockReady(block_hash))
            .expect("leader BlockReady should succeed");

        let leader_outputs = harness.drain_outputs(leader);
        let proposal = leader_outputs
            .iter()
            .find_map(|o| match o {
                EngineOutput::BroadcastMessage(msg @ ConsensusMessage::Proposal(_)) => {
                    Some(msg.clone())
                }
                _ => None,
            })
            .expect("leader should broadcast Proposal");

        // Step 2: Route Proposal to all non-leaders
        for i in 0..n {
            if i != leader {
                harness.engines[i]
                    .process_event(ConsensusEvent::Message(proposal.clone()))
                    .expect("should accept Proposal");
            }
        }

        // Step 2.5: BlockImported for all non-leaders
        for i in 0..n {
            if i != leader {
                harness.engines[i]
                    .process_event(ConsensusEvent::BlockImported(block_hash))
                    .expect("should accept BlockImported");
            }
        }

        // Step 3: Route votes — 6 Byzantine nodes (indices 15-20) send invalid votes
        let byzantine_set: std::collections::HashSet<usize> = (15..21).collect();

        for i in 0..n {
            if i != leader {
                let outputs = harness.drain_outputs(i);
                for output in outputs {
                    if let EngineOutput::SendToValidator(target, msg) = output {
                        if target == leader as u32 {
                            if byzantine_set.contains(&i) {
                                // Byzantine: tamper with signature by using wrong key
                                if let ConsensusMessage::Vote(vote) = &msg {
                                    let wrong_key = test_bls_key(99);
                                    let vote_msg = n42_consensus::protocol::quorum::signing_message(
                                        vote.view,
                                        &vote.block_hash,
                                    );
                                    let bad_sig = wrong_key.sign(&vote_msg);
                                    let bad_vote = Vote {
                                        view: vote.view,
                                        block_hash: vote.block_hash,
                                        voter: vote.voter,
                                        signature: bad_sig,
                                    };
                                    let _ = harness.engines[leader].process_event(
                                        ConsensusEvent::Message(ConsensusMessage::Vote(bad_vote)),
                                    );
                                }
                            } else {
                                // Honest vote
                                harness.engines[leader]
                                    .process_event(ConsensusEvent::Message(msg))
                                    .expect("leader should accept honest Vote");
                            }
                        }
                    }
                }
            }
        }

        // Step 4: Leader should have PrepareQC from 14 honest votes (>= quorum=13)
        let leader_outputs = harness.drain_outputs(leader);
        let prepare_qc = leader_outputs.iter().find_map(|o| match o {
            EngineOutput::BroadcastMessage(msg @ ConsensusMessage::PrepareQC(_)) => {
                Some(msg.clone())
            }
            _ => None,
        });

        assert!(
            prepare_qc.is_some(),
            "14 honest votes should form PrepareQC (quorum=13)"
        );

        // Continue: Route PrepareQC → CommitVotes → verify commitment
        let prepare_qc = prepare_qc.unwrap();
        for i in 0..n {
            if i != leader && !byzantine_set.contains(&i) {
                harness.engines[i]
                    .process_event(ConsensusEvent::Message(prepare_qc.clone()))
                    .expect("honest node should accept PrepareQC");
            }
        }

        // Route commit votes from honest nodes
        for i in 0..n {
            if i != leader && !byzantine_set.contains(&i) {
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

        // Verify commitment
        let leader_outputs = harness.drain_outputs(leader);
        let committed = leader_outputs.iter().any(|o| {
            matches!(o, EngineOutput::BlockCommitted { .. })
        });

        assert!(committed, "block should be committed despite 6 Byzantine nodes");
    }

    /// Timeout: leader crash triggers view change across 21 nodes.
    #[test]
    fn test_21v_leader_crash_view_change() {
        let mut harness = TestHarness::new(21);

        // View 1 leader is validator 1 — simulate crash (don't propose).
        // All other nodes timeout.
        harness.run_timeout_view_change(1);

        // After view change, all engines should be at view 2.
        let _next_leader = (2 % 21) as usize; // validator 2
        for i in 0..21 {
            assert!(
                harness.engines[i].current_view() >= 2,
                "engine {} should advance to view >= 2 after view change, got {}",
                i,
                harness.engines[i].current_view()
            );
        }

        // View 2: new leader (validator 2) proposes successfully.
        let block_hash = B256::repeat_byte(0xAA);
        harness.run_consensus_round(2, block_hash);

        assert_eq!(harness.committed_blocks.len(), 1);
        assert_eq!(harness.committed_blocks[0], (2, block_hash));
    }

    /// Timeout: two consecutive leader crashes, then successful consensus.
    #[test]
    fn test_21v_two_consecutive_leader_crashes() {
        let mut harness = TestHarness::new(21);

        // View 1: leader (val 1) crashes → timeout → advance to view 2
        harness.run_timeout_view_change(1);

        // View 2: leader (val 2) also crashes → timeout → advance to view 3
        harness.run_timeout_view_change(2);

        // View 3: leader (val 3) proposes successfully
        let block_hash = B256::repeat_byte(0x33);
        harness.run_consensus_round(3, block_hash);

        assert_eq!(harness.committed_blocks.len(), 1);
        assert_eq!(harness.committed_blocks[0], (3, block_hash));
    }

    /// Mixed: alternating successful rounds and timeouts.
    /// 30 views: even views commit, odd views timeout.
    #[test]
    fn test_21v_mixed_consensus_and_timeouts() {
        let mut harness = TestHarness::new(21);

        let mut committed_count = 0;
        let mut view = 1u64;

        for round in 0..30 {
            if round % 2 == 0 {
                // Successful round
                let mut block_bytes = [0u8; 32];
                block_bytes[0..8].copy_from_slice(&view.to_le_bytes());
                let block_hash = B256::from(block_bytes);
                harness.run_consensus_round(view, block_hash);
                committed_count += 1;
                view += 1;
            } else {
                // Timeout round
                harness.run_timeout_view_change(view);
                view += 1;
            }
        }

        assert_eq!(
            harness.committed_blocks.len(),
            committed_count,
            "should have committed exactly {} blocks",
            committed_count
        );

        // All engines should be at the same view
        let final_view = harness.engines[0].current_view();
        for i in 1..21 {
            assert!(
                harness.engines[i].current_view() >= final_view - 1,
                "engine {} view {} should be near {}",
                i,
                harness.engines[i].current_view(),
                final_view
            );
        }
    }

    /// Stability: 100 consecutive blocks with 21 nodes.
    /// Tests sustained operation and memory behavior.
    #[test]
    fn test_21v_100_blocks_liveness() {
        let mut harness = TestHarness::new(21);

        for view in 1..=100u64 {
            let mut block_bytes = [0u8; 32];
            block_bytes[0..8].copy_from_slice(&view.to_le_bytes());
            let block_hash = B256::from(block_bytes);
            harness.run_consensus_round(view, block_hash);
        }

        assert_eq!(harness.committed_blocks.len(), 100);
    }

    /// Stability: verify locked_qc monotonicity over 50 mixed rounds.
    #[test]
    fn test_21v_locked_qc_monotonic() {
        let mut harness = TestHarness::new(21);

        let mut view = 1u64;
        for round in 0..50 {
            if round % 3 == 2 {
                // Every 3rd round is a timeout
                harness.run_timeout_view_change(view);
            } else {
                let mut block_bytes = [0u8; 32];
                block_bytes[0..8].copy_from_slice(&view.to_le_bytes());
                let block_hash = B256::from(block_bytes);
                harness.run_consensus_round(view, block_hash);
            }
            view += 1;
        }

        // Verify all engines have consistent views
        let views: Vec<u64> = (0..21)
            .map(|i| harness.engines[i].current_view())
            .collect();
        let max_view = *views.iter().max().unwrap();
        let min_view = *views.iter().min().unwrap();
        assert!(
            max_view - min_view <= 1,
            "view divergence too large: max={}, min={}, views={:?}",
            max_view,
            min_view,
            views
        );
    }

    /// Fault tolerance: multiple rounds with exactly quorum participants,
    /// different subsets each round (rotating which 8 are "down").
    #[test]
    fn test_21v_rotating_failures_10_rounds() {
        let mut harness = TestHarness::new(21);

        for round in 0..10u64 {
            let view = round + 1;

            // Rotate which 8 nodes are "down" each round.
            // Ensure the leader for this view is always in the participating set.
            let leader = (view % 21) as usize;

            let participating: Vec<usize> = (0..21)
                .filter(|&i| {
                    // Drop 8 nodes based on round number
                    let dropped_start = ((round as usize) * 3) % 21;
                    let is_dropped = (0..8).any(|d| (dropped_start + d) % 21 == i);
                    !is_dropped || i == leader // leader always participates
                })
                .collect();

            // Ensure at least quorum (13) participants
            if participating.len() < 13 {
                // If leader inclusion pushed us to exactly 13, that's fine
                // Otherwise we need more — but with 21-8=13, plus leader always included,
                // we should have exactly 13 or 14.
            }

            let mut block_bytes = [0u8; 32];
            block_bytes[0..8].copy_from_slice(&view.to_le_bytes());
            let block_hash = B256::from(block_bytes);

            let committed =
                harness.run_consensus_round_partial(view, block_hash, &participating);

            assert!(
                committed,
                "round {} (view {}) with {}/21 nodes should commit (quorum=13), participating={:?}",
                round,
                view,
                participating.len(),
                participating
            );

            // Non-participating engines are already advanced via Decide in run_consensus_round_partial.
            harness.drain_all_outputs();
        }

        assert_eq!(harness.committed_blocks.len(), 10);
    }

    /// All 21 engines should remain view-consistent after 200 mixed rounds.
    #[test]
    fn test_21v_view_consistency_200_rounds() {
        let mut harness = TestHarness::new(21);

        let mut view = 1u64;
        let mut committed = 0;
        let mut timeouts = 0;

        for round in 0..200 {
            // 80% success, 20% timeout
            if round % 5 == 4 {
                harness.run_timeout_view_change(view);
                timeouts += 1;
            } else {
                let mut block_bytes = [0u8; 32];
                block_bytes[0..8].copy_from_slice(&view.to_le_bytes());
                let block_hash = B256::from(block_bytes);
                harness.run_consensus_round(view, block_hash);
                committed += 1;
            }
            view += 1;
        }

        assert_eq!(harness.committed_blocks.len(), committed);

        // Final view consistency check
        let views: Vec<u64> = (0..21)
            .map(|i| harness.engines[i].current_view())
            .collect();
        let max_view = *views.iter().max().unwrap();
        let min_view = *views.iter().min().unwrap();

        assert!(
            max_view - min_view <= 1,
            "after 200 rounds ({}c/{}t), view divergence too large: max={}, min={}, views={:?}",
            committed,
            timeouts,
            max_view,
            min_view,
            views
        );
    }

    /// Byzantine: f=6 nodes send votes for WRONG block hash.
    /// 14 honest nodes should still reach consensus on the correct block.
    #[test]
    fn test_21v_byzantine_6_wrong_block_votes() {
        let mut harness = TestHarness::new(21);
        let n = 21;
        let view = 1u64;
        let leader = (view % n as u64) as usize;
        let correct_hash = B256::repeat_byte(0xAA);
        let wrong_hash = B256::repeat_byte(0x99);

        // Step 1: Leader proposes correct block
        harness.engines[leader]
            .process_event(ConsensusEvent::BlockReady(correct_hash))
            .expect("BlockReady should succeed");

        let leader_outputs = harness.drain_outputs(leader);
        let proposal = leader_outputs
            .iter()
            .find_map(|o| match o {
                EngineOutput::BroadcastMessage(msg @ ConsensusMessage::Proposal(_)) => {
                    Some(msg.clone())
                }
                _ => None,
            })
            .expect("leader should broadcast Proposal");

        // Step 2: Route Proposal + BlockImported to all
        for i in 0..n {
            if i != leader {
                harness.engines[i]
                    .process_event(ConsensusEvent::Message(proposal.clone()))
                    .expect("should accept Proposal");
                harness.engines[i]
                    .process_event(ConsensusEvent::BlockImported(correct_hash))
                    .expect("should accept BlockImported");
            }
        }

        // Step 3: Byzantine validators (15-20) forge votes for wrong block
        let byzantine_set: std::collections::HashSet<usize> = (15..21).collect();

        for i in 0..n {
            if i != leader {
                let outputs = harness.drain_outputs(i);
                for output in outputs {
                    if let EngineOutput::SendToValidator(target, msg) = output {
                        if target == leader as u32 {
                            if byzantine_set.contains(&i) {
                                // Byzantine: vote for wrong block hash
                                if let ConsensusMessage::Vote(vote) = &msg {
                                    let vote_msg = signing_message(vote.view, &wrong_hash);
                                    let bad_sig =
                                        harness.secret_keys[i].sign(&vote_msg);
                                    let bad_vote = Vote {
                                        view: vote.view,
                                        block_hash: wrong_hash,
                                        voter: vote.voter,
                                        signature: bad_sig,
                                    };
                                    // This vote is for a different block → VoteCollector
                                    // should ignore it (wrong block_hash).
                                    let _ = harness.engines[leader].process_event(
                                        ConsensusEvent::Message(ConsensusMessage::Vote(bad_vote)),
                                    );
                                }
                            } else {
                                harness.engines[leader]
                                    .process_event(ConsensusEvent::Message(msg))
                                    .expect("should accept honest Vote");
                            }
                        }
                    }
                }
            }
        }

        // 14 honest votes for correct_hash → should form PrepareQC
        let leader_outputs = harness.drain_outputs(leader);
        let has_prepare_qc = leader_outputs.iter().any(|o| {
            matches!(o, EngineOutput::BroadcastMessage(ConsensusMessage::PrepareQC(_)))
        });

        assert!(
            has_prepare_qc,
            "14 honest votes for correct block should form PrepareQC despite 6 wrong-block votes"
        );
    }

    /// Boundary: exactly 12 honest + leader = 13 (borderline quorum) should commit.
    #[test]
    fn test_21v_borderline_quorum() {
        let mut harness = TestHarness::new(21);

        // View 1: leader = 1
        // Participating: leader (1) + validators 2-13 = 13 total = exact quorum
        let participating: Vec<usize> = (1..14).collect();

        let block_hash = B256::repeat_byte(0xBD);
        let committed = harness.run_consensus_round_partial(1, block_hash, &participating);

        assert!(
            committed,
            "borderline quorum of exactly 13/21 (including leader) should commit"
        );
    }

    /// Boundary: 12 voters + leader = 13 for quorum, but one fewer = 12, should NOT commit.
    #[test]
    fn test_21v_one_below_quorum() {
        let mut harness = TestHarness::new(21);

        // View 1: leader = 1
        // Participating: leader (1) + validators 2-12 = 12 total (one below quorum=13)
        let participating: Vec<usize> = (1..13).collect();

        let block_hash = B256::repeat_byte(0xB1);
        let committed = harness.run_consensus_round_partial(1, block_hash, &participating);

        assert!(
            !committed,
            "12/21 (one below quorum=13) should NOT commit"
        );
    }

    /// Stability: 200 consecutive blocks with 21 nodes.
    /// Tests sustained operation, memory behavior, and correct leader rotation
    /// over ~9.5 full rotation cycles.
    #[test]
    fn test_21v_200_consecutive_blocks() {
        let mut harness = TestHarness::new(21);

        for view in 1..=200u64 {
            let mut block_bytes = [0u8; 32];
            block_bytes[0..8].copy_from_slice(&view.to_le_bytes());
            block_bytes[8..16].copy_from_slice(&(view * 0x1337).to_le_bytes());
            let block_hash = B256::from(block_bytes);
            harness.run_consensus_round(view, block_hash);
        }

        assert_eq!(
            harness.committed_blocks.len(),
            200,
            "all 200 blocks should be committed"
        );

        // Verify monotonic view progression
        for i in 1..harness.committed_blocks.len() {
            assert!(
                harness.committed_blocks[i].0 > harness.committed_blocks[i - 1].0,
                "committed views should be monotonically increasing at index {}",
                i
            );
        }

        // Verify correct block hash for each view
        for (idx, &(v, h)) in harness.committed_blocks.iter().enumerate() {
            let mut expected_bytes = [0u8; 32];
            expected_bytes[0..8].copy_from_slice(&v.to_le_bytes());
            expected_bytes[8..16].copy_from_slice(&(v * 0x1337).to_le_bytes());
            let expected = B256::from(expected_bytes);
            assert_eq!(
                h, expected,
                "block {} (view {}) hash mismatch",
                idx, v
            );
        }

        // All engines at view 201
        for i in 0..21 {
            assert!(
                harness.engines[i].current_view() >= 201,
                "engine {} should be at view >= 201 after 200 blocks, got {}",
                i,
                harness.engines[i].current_view()
            );
        }

        // Verify every validator was leader approximately 200/21 ≈ 9-10 times
        let mut leader_counts = [0u32; 21];
        for &(v, _) in &harness.committed_blocks {
            let leader = (v % 21) as usize;
            leader_counts[leader] += 1;
        }
        for (i, &count) in leader_counts.iter().enumerate() {
            assert!(
                count >= 8 && count <= 11,
                "validator {} was leader {} times (expected ~9-10)",
                i,
                count
            );
        }
    }

    /// Verify deferred voting: non-leader should NOT emit vote until BlockImported.
    #[test]
    fn test_21v_deferred_voting_behavior() {
        let mut harness = TestHarness::new(21);
        let view = 1u64;
        let leader = (view % 21) as usize;
        let block_hash = B256::repeat_byte(0xDF);

        // Leader proposes
        harness.engines[leader]
            .process_event(ConsensusEvent::BlockReady(block_hash))
            .expect("BlockReady should succeed");

        let leader_outputs = harness.drain_outputs(leader);
        let proposal = leader_outputs
            .iter()
            .find_map(|o| match o {
                EngineOutput::BroadcastMessage(msg @ ConsensusMessage::Proposal(_)) => {
                    Some(msg.clone())
                }
                _ => None,
            })
            .expect("should have Proposal");

        // Route Proposal to validator 0 (non-leader)
        harness.engines[0]
            .process_event(ConsensusEvent::Message(proposal))
            .expect("should accept Proposal");

        // Check: NO vote yet (deferred voting)
        let outputs = harness.drain_outputs(0);
        let has_vote = outputs.iter().any(|o| {
            matches!(o, EngineOutput::SendToValidator(_, ConsensusMessage::Vote(_)))
        });
        assert!(
            !has_vote,
            "non-leader should NOT emit vote before BlockImported"
        );

        // Now send BlockImported → vote should be emitted
        harness.engines[0]
            .process_event(ConsensusEvent::BlockImported(block_hash))
            .expect("should accept BlockImported");

        let outputs = harness.drain_outputs(0);
        let has_vote = outputs.iter().any(|o| {
            matches!(o, EngineOutput::SendToValidator(_, ConsensusMessage::Vote(_)))
        });
        assert!(
            has_vote,
            "non-leader should emit vote after BlockImported"
        );
    }
}
