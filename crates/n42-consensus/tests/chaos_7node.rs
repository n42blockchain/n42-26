//! 7-Node HotStuff-2 Chaos Integration Tests (Cases 5-8)
//! Tests fault scenarios with precise message routing control.

use alloy_primitives::{Address, B256};
use n42_chainspec::ValidatorInfo;
use n42_consensus::error::ConsensusError;
use n42_consensus::protocol::quorum::signing_message;
use n42_consensus::{ConsensusEngine, ConsensusEvent, EngineOutput, ValidatorSet};
use n42_primitives::consensus::{ConsensusMessage, ViewNumber};
use n42_primitives::BlsSecretKey;
use tokio::sync::mpsc;

fn test_bls_key(index: u32) -> BlsSecretKey {
    let mut bytes = [0u8; 32];
    bytes[28..32].copy_from_slice(&((index + 1) as u32).to_be_bytes());
    BlsSecretKey::from_bytes(&bytes).expect("deterministic BLS key should be valid")
}

struct ChaosHarness {
    engines: Vec<ConsensusEngine>,
    #[allow(dead_code)]
    secret_keys: Vec<BlsSecretKey>,
    output_rxs: Vec<mpsc::Receiver<EngineOutput>>,
    #[allow(dead_code)]
    validator_set: ValidatorSet,
    committed_blocks: Vec<(ViewNumber, B256)>,
}

impl ChaosHarness {
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
            let (tx, rx) = mpsc::channel(1024);
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

    fn n(&self) -> usize {
        self.engines.len()
    }

    fn drain_outputs(&mut self, idx: usize) -> Vec<EngineOutput> {
        let mut outputs = Vec::new();
        while let Ok(o) = self.output_rxs[idx].try_recv() {
            outputs.push(o);
        }
        outputs
    }

    fn drain_all_outputs(&mut self) -> Vec<Vec<EngineOutput>> {
        (0..self.n()).map(|i| self.drain_outputs(i)).collect()
    }

    fn run_consensus_round(&mut self, view: ViewNumber, block_hash: B256) {
        let n = self.n();
        let leader = (view % n as u64) as usize;

        self.engines[leader]
            .process_event(ConsensusEvent::BlockReady(block_hash))
            .expect("leader BlockReady should succeed");

        let proposal = self.drain_outputs(leader)
            .into_iter()
            .find_map(|o| match o {
                EngineOutput::BroadcastMessage(msg @ ConsensusMessage::Proposal(_)) => Some(msg),
                _ => None,
            })
            .expect("leader should broadcast Proposal");

        for i in 0..n {
            if i != leader {
                self.engines[i]
                    .process_event(ConsensusEvent::Message(proposal.clone()))
                    .expect("non-leader should accept Proposal");
            }
        }

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

        for i in 0..n {
            if i != leader {
                for output in self.drain_outputs(i) {
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

        let prepare_qc = self.drain_outputs(leader)
            .into_iter()
            .find_map(|o| match o {
                EngineOutput::BroadcastMessage(msg @ ConsensusMessage::PrepareQC(_)) => Some(msg),
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

        for i in 0..n {
            if i != leader {
                for output in self.drain_outputs(i) {
                    if let EngineOutput::SendToValidator(target, msg) = output {
                        if target == leader as u32 {
                            let _ = self.engines[leader]
                                .process_event(ConsensusEvent::Message(msg));
                        }
                    }
                }
            }
        }

        let leader_outputs = self.drain_outputs(leader);
        for output in &leader_outputs {
            if let EngineOutput::BlockCommitted { view: v, block_hash: h, .. } = output {
                self.committed_blocks.push((*v, *h));
            }
        }

        let decide_msgs: Vec<_> = leader_outputs
            .into_iter()
            .filter_map(|o| match o {
                EngineOutput::BroadcastMessage(msg @ ConsensusMessage::Decide(_)) => Some(msg),
                _ => None,
            })
            .collect();

        for msg in &decide_msgs {
            for i in 0..n {
                if i != leader {
                    let _ = self.engines[i]
                        .process_event(ConsensusEvent::Message(msg.clone()));
                }
            }
        }

        let next_view = view + 1;
        for i in 0..n {
            if self.engines[i].current_view() < next_view {
                for msg in &decide_msgs {
                    let _ = self.engines[i]
                        .process_event(ConsensusEvent::Message(msg.clone()));
                }
            }
        }

        self.drain_all_outputs();
    }

    fn run_timeout_view_change(&mut self, view: ViewNumber) {
        let n = self.n();

        for i in 0..n {
            self.engines[i].on_timeout().expect("timeout should succeed");
        }

        let timeout_msgs: Vec<_> = self.drain_all_outputs()
            .into_iter()
            .enumerate()
            .flat_map(|(i, outputs)| {
                outputs.into_iter().filter_map(move |o| match o {
                    EngineOutput::BroadcastMessage(msg @ ConsensusMessage::Timeout(_)) => {
                        Some((i, msg))
                    }
                    _ => None,
                })
            })
            .collect();

        for (from, msg) in &timeout_msgs {
            for i in 0..n {
                if i != *from {
                    let _ = self.engines[i]
                        .process_event(ConsensusEvent::Message(msg.clone()));
                }
            }
        }

        let next_view = view + 1;
        let next_leader = (next_view % n as u64) as usize;

        if let Some(nv) = self.drain_all_outputs()
            .into_iter()
            .flatten()
            .find_map(|o| match o {
                EngineOutput::BroadcastMessage(msg @ ConsensusMessage::NewView(_)) => Some(msg),
                _ => None,
            })
        {
            for i in 0..n {
                if i != next_leader && self.engines[i].current_view() < next_view {
                    let _ = self.engines[i]
                        .process_event(ConsensusEvent::Message(nv.clone()));
                }
            }
        }

        self.drain_all_outputs();
    }

    /// Consensus round with network partition: group_a={0,1,2}, bridge={3}, group_b={4,5,6}.
    /// Same-group and bridge messages delivered in phase 1; cross-group in phase 2.
    fn run_round_with_partition(&mut self, view: ViewNumber, block_hash: B256) {
        let n = self.n();
        let leader = (view % n as u64) as usize;

        let in_group_a = |i: usize| -> bool { i <= 2 };
        let in_group_b = |i: usize| -> bool { i >= 4 && i <= 6 };
        let is_bridge = |i: usize| -> bool { i == 3 };
        let same_group = |i: usize, j: usize| -> bool {
            (in_group_a(i) && in_group_a(j)) || (in_group_b(i) && in_group_b(j))
        };
        let immediate_delivery = |from: usize, to: usize| -> bool {
            same_group(from, to) || is_bridge(from) || is_bridge(to)
        };

        // Helper: route a broadcast message from `from` with partition-aware delivery
        let route_broadcast =
            |harness: &mut ChaosHarness, from: usize, msg: &ConsensusMessage| {
                // Phase 1: immediate delivery (same group or bridge involved)
                for i in 0..n {
                    if i != from && immediate_delivery(from, i) {
                        let _ = harness.engines[i]
                            .process_event(ConsensusEvent::Message(msg.clone()));
                    }
                }
                // Phase 2: delayed delivery (cross-group, no bridge)
                for i in 0..n {
                    if i != from && !immediate_delivery(from, i) {
                        let _ = harness.engines[i]
                            .process_event(ConsensusEvent::Message(msg.clone()));
                    }
                }
            };

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

        // Step 2: Route Proposal with partition
        route_broadcast(self, leader, &proposal);

        // Step 2.5: BlockImported for non-leaders
        let proposal_block_hash = match &proposal {
            ConsensusMessage::Proposal(p) => p.block_hash,
            _ => unreachable!(),
        };
        for i in 0..n {
            if i != leader {
                let _ = self.engines[i]
                    .process_event(ConsensusEvent::BlockImported(proposal_block_hash));
            }
        }

        // Step 3: Route Votes to leader (with partition delay for cross-group)
        for i in 0..n {
            if i != leader {
                let outputs = self.drain_outputs(i);
                for output in outputs {
                    if let EngineOutput::SendToValidator(target, msg) = output {
                        if target == leader as u32 {
                            // Votes are point-to-point; apply partition rule
                            // Phase 1: immediate if same group or bridge
                            if immediate_delivery(i, leader) {
                                self.engines[leader]
                                    .process_event(ConsensusEvent::Message(msg))
                                    .expect("leader should accept Vote");
                            } else {
                                // Phase 2: delayed cross-group
                                let _ = self.engines[leader]
                                    .process_event(ConsensusEvent::Message(msg));
                            }
                        }
                    }
                }
            }
        }

        // Step 4: Route PrepareQC with partition
        let leader_outputs = self.drain_outputs(leader);
        let prepare_qc = leader_outputs.iter().find_map(|o| match o {
            EngineOutput::BroadcastMessage(msg @ ConsensusMessage::PrepareQC(_)) => {
                Some(msg.clone())
            }
            _ => None,
        });

        if let Some(ref pqc) = prepare_qc {
            route_broadcast(self, leader, pqc);
        }

        // Step 5: Route CommitVotes to leader
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

        // Step 6: Record commits, route Decide with partition
        let leader_outputs = self.drain_outputs(leader);
        for output in &leader_outputs {
            if let EngineOutput::BlockCommitted { view: v, block_hash: h, .. } = output {
                self.committed_blocks.push((*v, *h));
            }
        }

        let decide_msgs: Vec<ConsensusMessage> = leader_outputs
            .into_iter()
            .filter_map(|o| match o {
                EngineOutput::BroadcastMessage(msg @ ConsensusMessage::Decide(_)) => Some(msg),
                _ => None,
            })
            .collect();

        for msg in &decide_msgs {
            route_broadcast(self, leader, msg);
        }

        // Step 7: Safety net — re-deliver Decide
        let next_view = view + 1;
        for i in 0..n {
            if self.engines[i].current_view() < next_view {
                for msg in &decide_msgs {
                    let _ = self.engines[i]
                        .process_event(ConsensusEvent::Message(msg.clone()));
                }
            }
        }

        self.drain_all_outputs();
    }

    /// Consensus round with random packet drop.
    /// Returns (committed: bool, duplicate_vote_errors: usize, equivocation_events: usize).
    fn run_round_with_drop(
        &mut self,
        view: ViewNumber,
        block_hash: B256,
        rng: &mut SimpleRng,
        drop_rate: f64,
    ) -> (bool, usize, usize) {
        let n = self.n();
        let leader = (view % n as u64) as usize;
        let mut dup_vote_errors = 0usize;
        let mut equivocation_events = 0usize;
        let mut committed = false;

        // Helper: check process_event result for DuplicateVote
        let check_result = |result: Result<(), ConsensusError>, dup_count: &mut usize| {
            match result {
                Err(ConsensusError::DuplicateVote { .. }) => *dup_count += 1,
                _ => {}
            }
        };

        // Helper: count EquivocationDetected in outputs
        let count_equivocations = |outputs: &[EngineOutput]| -> usize {
            outputs
                .iter()
                .filter(|o| matches!(o, EngineOutput::EquivocationDetected { .. }))
                .count()
        };

        // Step 1: Leader proposes
        let result = self.engines[leader]
            .process_event(ConsensusEvent::BlockReady(block_hash));
        check_result(result, &mut dup_vote_errors);

        let leader_outputs = self.drain_outputs(leader);
        equivocation_events += count_equivocations(&leader_outputs);

        let proposal = leader_outputs.iter().find_map(|o| match o {
            EngineOutput::BroadcastMessage(msg @ ConsensusMessage::Proposal(_)) => {
                Some(msg.clone())
            }
            _ => None,
        });

        let proposal = match proposal {
            Some(p) => p,
            None => return (false, dup_vote_errors, equivocation_events),
        };

        let proposal_block_hash = match &proposal {
            ConsensusMessage::Proposal(p) => p.block_hash,
            _ => unreachable!(),
        };

        // Step 2: Route Proposal with drop
        for i in 0..n {
            if i != leader {
                if rng.next_f64() >= drop_rate {
                    let result = self.engines[i]
                        .process_event(ConsensusEvent::Message(proposal.clone()));
                    check_result(result, &mut dup_vote_errors);

                    // BlockImported immediately after Proposal
                    let _ = self.engines[i]
                        .process_event(ConsensusEvent::BlockImported(proposal_block_hash));
                }
            }
        }

        // Step 3: Route Votes with drop
        for i in 0..n {
            if i != leader {
                let outputs = self.drain_outputs(i);
                equivocation_events += count_equivocations(&outputs);
                for output in outputs {
                    if let EngineOutput::SendToValidator(target, msg) = output {
                        if target == leader as u32 {
                            if rng.next_f64() >= drop_rate {
                                let result = self.engines[leader]
                                    .process_event(ConsensusEvent::Message(msg));
                                check_result(result, &mut dup_vote_errors);
                            }
                        }
                    }
                }
            }
        }

        // Step 4: Check if PrepareQC formed
        let leader_outputs = self.drain_outputs(leader);
        equivocation_events += count_equivocations(&leader_outputs);
        let prepare_qc = leader_outputs.iter().find_map(|o| match o {
            EngineOutput::BroadcastMessage(msg @ ConsensusMessage::PrepareQC(_)) => {
                Some(msg.clone())
            }
            _ => None,
        });

        let prepare_qc = match prepare_qc {
            Some(pqc) => pqc,
            None => return (false, dup_vote_errors, equivocation_events),
        };

        // Step 5: Route PrepareQC with drop
        for i in 0..n {
            if i != leader {
                if rng.next_f64() >= drop_rate {
                    let result = self.engines[i]
                        .process_event(ConsensusEvent::Message(prepare_qc.clone()));
                    check_result(result, &mut dup_vote_errors);
                }
            }
        }

        // Step 6: Route CommitVotes with drop
        for i in 0..n {
            if i != leader {
                let outputs = self.drain_outputs(i);
                equivocation_events += count_equivocations(&outputs);
                for output in outputs {
                    if let EngineOutput::SendToValidator(target, msg) = output {
                        if target == leader as u32 {
                            if rng.next_f64() >= drop_rate {
                                let result = self.engines[leader]
                                    .process_event(ConsensusEvent::Message(msg));
                                check_result(result, &mut dup_vote_errors);
                            }
                        }
                    }
                }
            }
        }

        // Step 7: Check for Decide
        let leader_outputs = self.drain_outputs(leader);
        equivocation_events += count_equivocations(&leader_outputs);
        for output in &leader_outputs {
            if let EngineOutput::BlockCommitted { view: v, block_hash: h, .. } = output {
                self.committed_blocks.push((*v, *h));
                committed = true;
            }
        }

        let decide_msgs: Vec<ConsensusMessage> = leader_outputs
            .into_iter()
            .filter_map(|o| match o {
                EngineOutput::BroadcastMessage(msg @ ConsensusMessage::Decide(_)) => Some(msg),
                _ => None,
            })
            .collect();

        // Route Decide with drop
        for msg in &decide_msgs {
            for i in 0..n {
                if i != leader {
                    if rng.next_f64() >= drop_rate {
                        let _ = self.engines[i]
                            .process_event(ConsensusEvent::Message(msg.clone()));
                    }
                }
            }
        }

        // Safety net: ensure all engines advance
        let next_view = view + 1;
        for i in 0..n {
            if self.engines[i].current_view() < next_view {
                for msg in &decide_msgs {
                    let _ = self.engines[i]
                        .process_event(ConsensusEvent::Message(msg.clone()));
                }
            }
        }

        let all_outputs = self.drain_all_outputs();
        for outputs in &all_outputs {
            equivocation_events += count_equivocations(outputs);
        }

        (committed, dup_vote_errors, equivocation_events)
    }
}

// ---------------------------------------------------------------------------
// Simple deterministic PRNG (xorshift64)
// ---------------------------------------------------------------------------

struct SimpleRng {
    state: u64,
}

impl SimpleRng {
    fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 { 1 } else { seed },
        }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / ((1u64 << 53) as f64)
    }
}

// ===========================================================================
// Case 5: Proposer Suppression → Timeout → View Change → Recovery
// ===========================================================================

#[tokio::test]
async fn test_timeout_view_change_proposer_suppression() {
    let mut harness = ChaosHarness::new(7);

    // Phase 1: Normal rounds 1-3
    for view in 1..=3 {
        let block_hash = B256::from([view as u8; 32]);
        harness.run_consensus_round(view, block_hash);
    }

    // All engines should be at view 4
    for i in 0..7 {
        assert_eq!(
            harness.engines[i].current_view(),
            4,
            "engine {i} should be at view 4 after 3 normal rounds"
        );
    }

    // Phase 2: View 4 — proposer is silent (no BlockReady)
    // Simulate timeout: all engines timeout on view 4
    harness.run_timeout_view_change(4);

    // After timeout + view change, all engines should be at view 5
    for i in 0..7 {
        assert_eq!(
            harness.engines[i].current_view(),
            5,
            "engine {i} should be at view 5 after view 4 timeout"
        );
    }

    // Phase 3: View 5 — normal consensus resumes with the new leader
    let block_hash_5 = B256::from([5u8; 32]);
    harness.run_consensus_round(5, block_hash_5);

    for i in 0..7 {
        assert_eq!(
            harness.engines[i].current_view(),
            6,
            "engine {i} should be at view 6 after view 5 completes"
        );
    }

    // Phase 4: Normal rounds 6-7
    for view in 6..=7 {
        let block_hash = B256::from([view as u8; 32]);
        harness.run_consensus_round(view, block_hash);
    }

    // Verification
    // V1: View 4 should have no committed block
    let committed_views: Vec<ViewNumber> = harness.committed_blocks.iter().map(|(v, _)| *v).collect();
    assert!(
        !committed_views.contains(&4),
        "view 4 should have no committed block (proposer was silent)"
    );

    // V2: View 5 should have a committed block
    assert!(
        committed_views.contains(&5),
        "view 5 should have a committed block after recovery"
    );

    // V3: Views 1,2,3,5,6,7 should all be committed (6 blocks total)
    let expected_committed: Vec<u64> = vec![1, 2, 3, 5, 6, 7];
    for v in &expected_committed {
        assert!(
            committed_views.contains(v),
            "view {v} should have been committed"
        );
    }
    assert_eq!(
        harness.committed_blocks.len(),
        6,
        "should have exactly 6 committed blocks (skip view 4)"
    );

    // V4: All engines at view 8
    for i in 0..7 {
        assert_eq!(
            harness.engines[i].current_view(),
            8,
            "engine {i} should be at view 8"
        );
    }
}

// ===========================================================================
// Case 6: Malicious Forged Votes
// ===========================================================================

#[tokio::test]
async fn test_malicious_forged_votes() {
    let mut harness = ChaosHarness::new(7);

    // Phase 1: Normal rounds 1-2
    for view in 1..=2 {
        let block_hash = B256::from([view as u8; 32]);
        harness.run_consensus_round(view, block_hash);
    }

    // All engines at view 3
    for i in 0..7 {
        assert_eq!(harness.engines[i].current_view(), 3);
    }

    // Phase 2: View 3 — manual round with malicious votes
    let view = 3u64;
    let block_hash = B256::from([3u8; 32]);
    let leader = (view % 7) as usize; // leader = 3
    assert_eq!(leader, 3, "leader for view 3 should be node 3");

    // Step a: Leader proposes
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

    // Step b: Route Proposal + BlockImported to ALL 7 nodes (non-leader)
    for i in 0..7 {
        if i != leader {
            harness.engines[i]
                .process_event(ConsensusEvent::Message(proposal.clone()))
                .expect("should accept Proposal");
            harness.engines[i]
                .process_event(ConsensusEvent::BlockImported(block_hash))
                .expect("should accept BlockImported");
        }
    }

    // Step c: Collect honest votes from nodes 0, 2, 4, 5 and send to leader
    // Leader already self-voted (1 vote), we need 4 more for quorum of 5
    let honest_voters = [0usize, 2, 4, 5];
    for &i in &honest_voters {
        let outputs = harness.drain_outputs(i);
        for output in outputs {
            if let EngineOutput::SendToValidator(target, msg) = output {
                if target == leader as u32 {
                    harness.engines[leader]
                        .process_event(ConsensusEvent::Message(msg))
                        .expect("leader should accept honest Vote");
                }
            }
        }
    }

    // Step d: Malicious node 1 — forged vote with wrong key
    {
        let wrong_sk = test_bls_key(100); // completely wrong key
        let msg_bytes = signing_message(view, &block_hash);
        let wrong_sig = wrong_sk.sign(&msg_bytes);
        let forged_vote = ConsensusMessage::Vote(n42_primitives::consensus::Vote {
            view,
            block_hash,
            voter: 1,
            signature: wrong_sig,
        });

        let result = harness.engines[leader]
            .process_event(ConsensusEvent::Message(forged_vote));

        match result {
            Err(ConsensusError::InvalidSignature {
                view: v,
                validator_index: vi,
            }) => {
                assert_eq!(v, 3, "should be view 3");
                assert_eq!(vi, 1, "should be validator 1");
            }
            other => panic!(
                "expected InvalidSignature for forged vote from node 1, got: {:?}",
                other
            ),
        }
    }

    // Step e: Malicious node 6 — correct key but wrong block hash
    {
        let sk6 = test_bls_key(6);
        let wrong_hash = B256::repeat_byte(0xFF);
        let msg_bytes = signing_message(view, &wrong_hash);
        let wrong_sig = sk6.sign(&msg_bytes);
        let forged_vote = ConsensusMessage::Vote(n42_primitives::consensus::Vote {
            view,
            block_hash: wrong_hash,
            voter: 6,
            signature: wrong_sig,
        });

        // This vote has a mismatched block_hash — it should be rejected
        // Either Ok (silently ignored) or error due to hash mismatch
        let result = harness.engines[leader]
            .process_event(ConsensusEvent::Message(forged_vote));

        // The vote's block_hash doesn't match the collector's expected hash,
        // so it could be ignored or return an error — either is acceptable
        match &result {
            Ok(()) => { /* silently ignored, acceptable */ }
            Err(ConsensusError::BlockHashMismatch { .. }) => { /* explicitly rejected, acceptable */ }
            Err(ConsensusError::InvalidSignature { .. }) => {
                // The engine verifies signature against the *proposal's* block_hash,
                // not the vote's block_hash, so this mismatch causes sig check failure
            }
            Err(e) => panic!(
                "unexpected error for wrong-hash vote from node 6: {:?}",
                e
            ),
        }
    }

    // Drain node 1 and 6 outputs (they voted honestly to themselves but we don't route)
    harness.drain_outputs(1);
    harness.drain_outputs(6);

    // Step f: Leader should have formed PrepareQC from 5 honest votes
    let leader_outputs = harness.drain_outputs(leader);
    let has_prepare_qc = leader_outputs.iter().any(|o| {
        matches!(
            o,
            EngineOutput::BroadcastMessage(ConsensusMessage::PrepareQC(_))
        )
    });
    assert!(
        has_prepare_qc,
        "leader should have formed PrepareQC from honest votes"
    );

    // Step g: Complete the rest of the round — route PrepareQC → CommitVote → Decide
    let prepare_qc = leader_outputs
        .iter()
        .find_map(|o| match o {
            EngineOutput::BroadcastMessage(msg @ ConsensusMessage::PrepareQC(_)) => {
                Some(msg.clone())
            }
            _ => None,
        })
        .unwrap();

    for i in 0..7 {
        if i != leader {
            let _ = harness.engines[i]
                .process_event(ConsensusEvent::Message(prepare_qc.clone()));
        }
    }

    // Route CommitVotes
    for i in 0..7 {
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

    // Route Decide
    let leader_outputs = harness.drain_outputs(leader);
    for output in &leader_outputs {
        if let EngineOutput::BlockCommitted { view: v, block_hash: h, .. } = output {
            harness.committed_blocks.push((*v, *h));
        }
    }

    let decide_msgs: Vec<_> = leader_outputs
        .iter()
        .filter_map(|o| match o {
            EngineOutput::BroadcastMessage(msg @ ConsensusMessage::Decide(_)) => Some(msg.clone()),
            _ => None,
        })
        .collect();

    for msg in &decide_msgs {
        for i in 0..7 {
            if i != leader {
                let _ = harness.engines[i]
                    .process_event(ConsensusEvent::Message(msg.clone()));
            }
        }
    }

    harness.drain_all_outputs();

    // Phase 3: Normal rounds 4-5
    for view in 4..=5 {
        let block_hash = B256::from([view as u8; 32]);
        harness.run_consensus_round(view, block_hash);
    }

    // Verification
    let committed_views: Vec<ViewNumber> =
        harness.committed_blocks.iter().map(|(v, _)| *v).collect();

    // V1: View 3 was successfully committed despite malicious votes
    assert!(
        committed_views.contains(&3),
        "view 3 should be committed despite malicious votes"
    );

    // V2: All 5 views committed (1,2,3,4,5)
    assert_eq!(
        harness.committed_blocks.len(),
        5,
        "should have committed 5 blocks total"
    );
    for v in 1..=5 {
        assert!(
            committed_views.contains(&v),
            "view {v} should be committed"
        );
    }

    // V3: All engines at view 6
    for i in 0..7 {
        assert_eq!(
            harness.engines[i].current_view(),
            6,
            "engine {i} should be at view 6"
        );
    }
}

// ===========================================================================
// Case 7: Network Partition with Bridge Node
// ===========================================================================

#[tokio::test]
async fn test_network_partition_with_bridge() {
    let mut harness = ChaosHarness::new(7);

    // Phase 1: Normal rounds 1-3
    for view in 1..=3 {
        let block_hash = B256::from([view as u8; 32]);
        harness.run_consensus_round(view, block_hash);
    }

    // Phase 2: Partition mode rounds 4-13
    for view in 4..=13 {
        let block_hash = B256::from([view as u8; 32]);
        harness.run_round_with_partition(view, block_hash);
    }

    // Phase 3: Normal rounds 14-15
    for view in 14..=15 {
        let block_hash = B256::from([view as u8; 32]);
        harness.run_consensus_round(view, block_hash);
    }

    // Verification
    let committed_views: Vec<ViewNumber> =
        harness.committed_blocks.iter().map(|(v, _)| *v).collect();

    // V1: All 15 rounds committed
    assert_eq!(
        harness.committed_blocks.len(),
        15,
        "all 15 rounds should be committed, got {}",
        harness.committed_blocks.len()
    );
    for v in 1..=15 {
        assert!(
            committed_views.contains(&v),
            "view {v} should be committed"
        );
    }

    // V2: All engines at view 16
    for i in 0..7 {
        assert_eq!(
            harness.engines[i].current_view(),
            16,
            "engine {i} should be at view 16"
        );
    }

    // V3: Block hash consistency — all engines committed the same hashes
    for (view, hash) in &harness.committed_blocks {
        // Verify the hash matches what we proposed
        let expected = B256::from([*view as u8; 32]);
        assert_eq!(
            *hash, expected,
            "committed block hash for view {view} should match proposed hash"
        );
    }
}

// ===========================================================================
// Case 8: Packet Drop + No Duplicate Votes
// ===========================================================================

#[tokio::test]
async fn test_packet_drop_consensus_resilience() {
    let mut harness = ChaosHarness::new(7);
    let mut rng = SimpleRng::new(42);
    let drop_rate = 0.20; // 20% drop rate

    let mut total_committed = 0usize;
    let mut total_dup_vote_errors = 0usize;
    let mut total_equivocation_events = 0usize;

    // Phase 1: Normal rounds 1-3 (warm up)
    for view in 1..=3 {
        let block_hash = B256::from([view as u8; 32]);
        harness.run_consensus_round(view, block_hash);
        total_committed += 1;
    }

    // Phase 2: 30 rounds with packet drop (views 4-33)
    let mut timeout_views = Vec::new();
    for view in 4..=33 {
        let block_hash = B256::from([(view % 256) as u8; 32]);
        let (committed, dup_errors, equiv_events) =
            harness.run_round_with_drop(view, block_hash, &mut rng, drop_rate);

        total_dup_vote_errors += dup_errors;
        total_equivocation_events += equiv_events;

        if committed {
            total_committed += 1;
        } else {
            timeout_views.push(view);
            // Run timeout → view change so all engines advance
            harness.run_timeout_view_change(view);
        }
    }

    // Verification
    eprintln!(
        "Packet drop results: committed={total_committed}, timeouts={}, dup_vote_errors={total_dup_vote_errors}, equivocations={total_equivocation_events}",
        timeout_views.len()
    );

    // V1: At least 10 rounds committed (3 normal + >=7 under drop)
    // With 20% drop and 7 nodes (quorum=5), P(>=5 votes arrive) is high
    assert!(
        total_committed >= 10,
        "should have at least 10 committed rounds, got {total_committed}"
    );

    // V2: No duplicate vote errors (honest nodes never double-vote)
    assert_eq!(
        total_dup_vote_errors, 0,
        "honest nodes should never produce duplicate votes"
    );

    // V3: No equivocation events
    assert_eq!(
        total_equivocation_events, 0,
        "honest nodes should never equivocate"
    );

    // V4: All engines should be at the same view
    let final_views: Vec<ViewNumber> = (0..7)
        .map(|i| harness.engines[i].current_view())
        .collect();
    let max_view = *final_views.iter().max().unwrap();
    let min_view = *final_views.iter().min().unwrap();
    assert!(
        max_view - min_view <= 1,
        "all engines should be at approximately the same view, got range {min_view}..{max_view}"
    );
}

// ===========================================================================
// Case 9: Timeout Convergence from Different Views
// ===========================================================================
//
// 7 nodes are scattered across views 10-22 (simulating independent timeout
// progression after a stall). By exchanging timeout messages, they must
// converge to a common view and resume normal consensus.

#[tokio::test]
async fn test_timeout_convergence_from_different_views() {
    let n = 7usize;
    let secret_keys: Vec<BlsSecretKey> = (0..n as u32).map(test_bls_key).collect();

    let infos: Vec<ValidatorInfo> = secret_keys
        .iter()
        .enumerate()
        .map(|(i, sk)| ValidatorInfo {
            address: Address::with_last_byte(i as u8),
            bls_public_key: sk.public_key(),
        })
        .collect();

    let f = (n as u32).saturating_sub(1) / 3; // f=2
    let validator_set = ValidatorSet::new(&infos, f);

    // Create engines with different starting views (simulating desync).
    let starting_views: Vec<u64> = vec![10, 14, 18, 22, 12, 16, 20];

    let mut engines = Vec::with_capacity(n);
    let mut output_rxs = Vec::with_capacity(n);

    for i in 0..n {
        let (tx, rx) = mpsc::channel(4096);
        let engine = ConsensusEngine::with_recovered_state(
            i as u32,
            secret_keys[i].clone(),
            n42_consensus::EpochManager::new(validator_set.clone()),
            1000,
            10000,
            tx,
            starting_views[i],
            n42_primitives::consensus::QuorumCertificate::genesis(),
            n42_primitives::consensus::QuorumCertificate::genesis(),
            0,
        );
        engines.push(engine);
        output_rxs.push(rx);
    }

    // Verify starting views
    for i in 0..n {
        assert_eq!(engines[i].current_view(), starting_views[i]);
    }

    /// Drain all broadcast messages from all engines.
    fn drain_broadcasts(
        n: usize,
        output_rxs: &mut [mpsc::Receiver<EngineOutput>],
    ) -> Vec<(usize, ConsensusMessage)> {
        let mut msgs = Vec::new();
        for i in 0..n {
            while let Ok(o) = output_rxs[i].try_recv() {
                if let EngineOutput::BroadcastMessage(msg) = o {
                    msgs.push((i, msg));
                }
            }
        }
        msgs
    }

    /// Route broadcast messages to all other engines, then drain any new
    /// broadcasts generated by the routing. Repeats until no new messages.
    fn route_all_broadcasts(
        n: usize,
        engines: &mut [ConsensusEngine],
        output_rxs: &mut [mpsc::Receiver<EngineOutput>],
    ) {
        let mut pending = drain_broadcasts(n, output_rxs);
        let mut safety = 0;
        while !pending.is_empty() && safety < 50 {
            safety += 1;
            for (from, msg) in pending.drain(..) {
                for i in 0..n {
                    if i != from {
                        let _ = engines[i]
                            .process_event(ConsensusEvent::Message(msg.clone()));
                    }
                }
            }
            pending = drain_broadcasts(n, output_rxs);
        }
    }

    // Phase 1: Convergence via timeout exchange.
    // Each round: trigger timeouts → route all messages until stable.
    let max_rounds = 10;
    for _round in 0..max_rounds {
        for i in 0..n {
            let _ = engines[i].on_timeout();
        }

        route_all_broadcasts(n, &mut engines, &mut output_rxs);

        // Check convergence
        let views: Vec<u64> = (0..n).map(|i| engines[i].current_view()).collect();
        let max_v = *views.iter().max().unwrap();
        let min_v = *views.iter().min().unwrap();
        if max_v == min_v {
            break;
        }
    }

    // Verification: all engines should now be at the same view
    let final_views: Vec<u64> = (0..n).map(|i| engines[i].current_view()).collect();
    let max_view = *final_views.iter().max().unwrap();
    let min_view = *final_views.iter().min().unwrap();

    eprintln!("Final views after convergence: {:?}", final_views);

    assert!(
        max_view - min_view <= 1,
        "all engines should converge, got range {min_view}..{max_view}, views: {:?}",
        final_views
    );
    assert!(max_view >= 22, "converged view should be >= 22, got {max_view}");

    // Phase 2: Transfer engines into a ChaosHarness for normal consensus.
    // Build a harness from the converged engines.
    let mut harness = ChaosHarness {
        engines,
        secret_keys,
        output_rxs,
        validator_set,
        committed_blocks: Vec::new(),
    };

    // Run timeout→view change to advance past TimedOut phase.
    let converged_view = max_view;
    harness.run_timeout_view_change(converged_view);

    // All engines should be at converged_view + 1
    let next_view = converged_view + 1;
    for i in 0..n {
        assert_eq!(
            harness.engines[i].current_view(),
            next_view,
            "engine {i} should be at view {next_view} after view change"
        );
    }

    // Phase 3: 3 normal consensus rounds to verify recovery.
    for offset in 0u64..3 {
        let view = next_view + offset;
        let block_hash = B256::repeat_byte((view % 256) as u8);
        harness.run_consensus_round(view, block_hash);
    }

    // Verification
    assert_eq!(
        harness.committed_blocks.len(), 3,
        "should commit 3 blocks post-convergence"
    );

    let final_view = next_view + 3;
    for i in 0..n {
        assert_eq!(
            harness.engines[i].current_view(),
            final_view,
            "engine {i} should be at view {final_view}"
        );
    }

    eprintln!(
        "Convergence test passed: converged at {}, resumed at {}, final view {}",
        converged_view, next_view, final_view
    );
}
