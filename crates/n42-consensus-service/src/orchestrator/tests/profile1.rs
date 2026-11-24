//! Profile 1 (mixed production standard): V1's headers and refusals fed
//! through the orchestrator's import entry points — follower import (block
//! data), sync import, the authenticated catch-up import (shared by the
//! reverse walk and the range client) — and the Round-1 vote gate.

use super::*;
use crate::profile1_header::Profile1ImportGate;
use alloy_primitives::hex;
use n42_chainspec::profile1::{Profile1Header, profile1_chain_spec};
use serde_json::Value;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

fn testdata(file: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../n42-chainspec/testdata/profile1/V1")
        .join(file);
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn raw(value: &Value) -> Vec<u8> {
    hex::decode(value.as_str().unwrap()).unwrap()
}

struct V1 {
    vector: Value,
    block1: Vec<u8>,
    block2: Vec<u8>,
}

/// The vote clock of the tests that read it, in unix seconds.
static VOTE_CLOCK: AtomicU64 = AtomicU64::new(0);

fn vote_clock() -> u64 {
    VOTE_CLOCK.load(Ordering::SeqCst)
}

fn v1() -> (V1, Profile1ImportGate) {
    let vector = testdata("header_profile_v1.json");
    let (_, config, genesis) = profile1_chain_spec(&testdata("genesis_8s.json")).unwrap();
    let gate = Profile1ImportGate::new(&config, genesis).unwrap();
    let v1 = V1 {
        block1: raw(&vector["headers"][0]["rlp"]),
        block2: raw(&vector["headers"][1]["rlp"]),
        vector,
    };
    (v1, gate)
}

impl V1 {
    /// The refusals checked on import, with the vector's names.
    /// V1's import refusals the header gate decides. Its two requestsHash
    /// refusals are not among them: under AM25 requestsHash is a deferred
    /// field, refused by the engine's deferred check (`n42-node`'s
    /// `profile1_deferred.rs`), not by the static header rules.
    fn import_refusals(&self) -> Vec<(String, Vec<u8>)> {
        self.refusals("import")
            .into_iter()
            .filter(|(name, _)| !name.starts_with("requestsHash"))
            .collect()
    }

    fn refusals(&self, check: &str) -> Vec<(String, Vec<u8>)> {
        self.vector["refusals"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|refusal| refusal["check"] == check)
            .map(|refusal| (refusal["name"].as_str().unwrap().to_owned(), raw(&refusal["rlp"])))
            .collect()
    }
}

fn profile1_orchestrator(gate: Profile1ImportGate) -> ConsensusService {
    let (engine, output_rx) = make_test_engine();
    let (network, _cmd_rx, _prx) = make_test_network();
    let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
    ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx)
        .with_profile1_gate(gate)
}

/// Native block data for `header`: the header travels in
/// `native_header_rlp`, the payload names the header's hash.
fn block_data(header: &[u8], view: u64) -> (B256, BlockDataBroadcast) {
    let hash = alloy_primitives::keccak256(header);
    let parent = Profile1Header::decode(header)
        .map(|header| header.header.parent_hash)
        .unwrap_or_default();
    let payload_json = serde_json::to_vec(&test_execution_data(parent, hash)).unwrap();
    (
        hash,
        BlockDataBroadcast {
            block_hash: hash,
            view,
            payload_json,
            timestamp: 1,
            execution_output: None,
            leader_ready_unix_ms: 0,
            native_header_rlp: Some(header.to_vec()),
        },
    )
}

/// gov5's wire form `[header, txs, verifiers, rewards]` of an empty block
/// whose rewards are `devBlockReward` to `coinbase` and to the faucet (A8).
fn gov5_wire_block(header: &[u8], coinbase: alloy_primitives::Address) -> Vec<u8> {
    use alloy_rlp::Encodable;
    let faucet: alloy_primitives::Address =
        "0x42e9819036f61bF665D5f727E8C03121f12f586e".parse().unwrap();
    let reward = U256::from(1_000_000_000_000_000_000_u64);
    let mut rewards_payload = Vec::new();
    for address in [coinbase, faucet] {
        let mut item = Vec::new();
        address.encode(&mut item);
        reward.encode(&mut item);
        alloy_rlp::Header { list: true, payload_length: item.len() }.encode(&mut rewards_payload);
        rewards_payload.extend_from_slice(&item);
    }
    let mut rewards = Vec::new();
    alloy_rlp::Header { list: true, payload_length: rewards_payload.len() }.encode(&mut rewards);
    rewards.extend_from_slice(&rewards_payload);
    let mut payload = header.to_vec();
    payload.extend_from_slice(&[0xc0, 0xc0]);
    payload.extend_from_slice(&rewards);
    let mut out = Vec::new();
    alloy_rlp::Header { list: true, payload_length: payload.len() }.encode(&mut out);
    out.extend_from_slice(&payload);
    out
}

#[tokio::test]
async fn follower_import_admits_v1_and_refuses_every_import_refusal() {
    let (f, gate) = v1();
    let mut orch = profile1_orchestrator(gate);

    for (header, view) in [(&f.block1, 1), (&f.block2, 2)] {
        let (hash, broadcast) = block_data(header, view);
        orch.handle_block_data(bincode::serialize(&broadcast).unwrap()).await;
        assert!(orch.pending_block_data.contains_key(&hash), "V1 block {view} is cached");
        assert!(orch.profile1.as_ref().unwrap().header(&hash).is_some());
    }

    let refusals = f.import_refusals();
    assert_eq!(refusals.len(), 21);
    for (name, header) in refusals {
        let (hash, broadcast) = block_data(&header, 2);
        orch.handle_block_data(bincode::serialize(&broadcast).unwrap()).await;
        assert!(
            !orch.pending_block_data.contains_key(&hash),
            "{name}: a refused header must not be cached for import or vote"
        );
        assert!(orch.profile1.as_ref().unwrap().header(&hash).is_none(), "{name}");
    }

    // The vote-time refusal is a valid header on import.
    for (name, header) in f.refusals("vote") {
        let (hash, broadcast) = block_data(&header, 2);
        orch.handle_block_data(bincode::serialize(&broadcast).unwrap()).await;
        assert!(orch.pending_block_data.contains_key(&hash), "{name} imports");
    }
}

#[tokio::test]
async fn follower_import_without_the_parent_is_held_back() {
    let (f, gate) = v1();
    let mut orch = profile1_orchestrator(gate);
    let (hash, broadcast) = block_data(&f.block2, 2);
    orch.handle_block_data(bincode::serialize(&broadcast).unwrap()).await;
    assert!(!orch.pending_block_data.contains_key(&hash), "block 1 was never admitted");
}

#[tokio::test]
async fn sync_import_refuses_before_new_payload() {
    let (f, gate) = v1();
    let mut orch = profile1_orchestrator(gate);
    let mock = Arc::new(MockExecutionLayer::new(
        PayloadStatusEnum::Valid,
        PayloadStatusEnum::Valid,
    ));
    orch.el = Some(mock.clone());
    orch.async_finalize_fcu = false;

    let (hash1, broadcast) = block_data(&f.block1, 1);
    let mut broadcast_json = broadcast.clone();
    broadcast_json.payload_json = compress_payload(&broadcast.payload_json);
    assert!(orch.import_and_notify(broadcast_json).await, "V1 block 1 imports");
    assert_eq!(mock.new_payload_hashes(), vec![hash1]);

    for (name, header) in f.import_refusals() {
        let (_, mut broadcast) = block_data(&header, 2);
        broadcast.payload_json = compress_payload(&broadcast.payload_json);
        assert!(!orch.import_and_notify(broadcast).await, "{name}");
    }
    assert_eq!(mock.new_payload_hashes(), vec![hash1], "no refused header reached execution");

    // A sync block without its header is refused on Profile 1.
    let unknown = B256::repeat_byte(0x5a);
    let mut headerless = block_data(&f.block2, 2).1;
    headerless.block_hash = unknown;
    headerless.native_header_rlp = None;
    headerless.payload_json =
        compress_payload(&serde_json::to_vec(&test_execution_data(hash1, unknown)).unwrap());
    assert!(!orch.import_and_notify(headerless).await);
    assert_eq!(mock.new_payload_hashes(), vec![hash1]);
}

/// The catch-up import (reverse walk and range client) checks the header
/// before the payload is rebuilt. A valid V1 block passes the gate and goes
/// on to `new_payload` under its Profile-1 hash (the payload is a V3 payload
/// with the requests hash in its sidecar; the engine validator binds the
/// 24-element header back from the remembered encoding). No refused header
/// gets past the gate.
#[tokio::test]
async fn catchup_import_refuses_before_new_payload() {
    let (f, gate) = v1();
    let block1 = Profile1Header::decode(&f.block1).unwrap();
    let block2 = Profile1Header::decode(&f.block2).unwrap();
    let mut orch = profile1_orchestrator(gate);
    let mock = Arc::new(MockExecutionLayer::new(
        PayloadStatusEnum::Valid,
        PayloadStatusEnum::Valid,
    ));
    orch.el = Some(mock.clone());
    let source = n42_network::PeerId::random();
    let admitted = |orch: &ConsensusService, hash: B256| {
        orch.profile1.as_ref().unwrap().header(&hash).is_some()
    };

    // gov5 bodies decode only with the Profile-1 layout.
    let wire1 = gov5_wire_block(&f.block1, block1.header.beneficiary);
    assert!(n42_network::decode_gov5_block_rlp(&wire1).is_err());
    let decoded1 = orch.decode_wire_block(&wire1).unwrap();
    assert_eq!(decoded1.block_hash, block1.hash());
    let payload1 = crate::replay_import::build_gov5_gossip_execution_data(&decoded1).unwrap();
    assert_eq!(payload1.block_hash(), block1.hash());
    assert_eq!(
        payload1.sidecar.requests_hash(),
        Some(n42_chainspec::profile1::PROFILE1_REQUESTS_HASH)
    );
    assert!(orch.import_h2_v4_catchup_block(source, decoded1, 1).await);
    assert!(admitted(&orch, block1.hash()), "V1 block 1 passes the header gate");
    assert_eq!(mock.new_payload_hashes(), vec![block1.hash()]);

    let mut refused = 0;
    for (name, header) in f.import_refusals() {
        let Ok(decoded) = Profile1Header::decode(&header) else {
            // A layout refusal never decodes as a Profile-1 body.
            let wire = gov5_wire_block(&header, block2.header.beneficiary);
            assert!(orch.decode_wire_block(&wire).is_err(), "{name}");
            refused += 1;
            continue;
        };
        let block = n42_network::Gov5GossipBlock {
            block_hash: decoded.hash(),
            header: decoded.header.clone(),
            transactions: Vec::new(),
            rewards: Vec::new(),
            mobile_registry_root: Some(decoded.mobile_registry_root),
        };
        assert!(!orch.import_h2_v4_catchup_block(source, block, 2).await, "{name}");
        assert!(!admitted(&orch, decoded.hash()), "{name}");
        refused += 1;
    }
    assert_eq!(refused, 21);

    let wire2 = gov5_wire_block(&f.block2, block2.header.beneficiary);
    let decoded2 = orch.decode_wire_block(&wire2).unwrap();
    assert!(orch.import_h2_v4_catchup_block(source, decoded2, 2).await);
    assert!(admitted(&orch, block2.hash()), "V1 block 2 passes the header gate");
    assert_eq!(
        mock.new_payload_hashes(),
        vec![block1.hash(), block2.hash()],
        "only the two valid blocks reach new_payload"
    );
}

#[tokio::test]
async fn round1_vote_waits_until_the_header_time_is_near() {
    let (f, gate) = v1();
    let vote_refusal = f.vector["refusals"]
        .as_array()
        .unwrap()
        .iter()
        .find(|refusal| refusal["check"] == "vote")
        .unwrap()
        .clone();
    let now = vote_refusal["nowUnix"].as_u64().unwrap();
    let header = raw(&vote_refusal["rlp"]);
    let (engine, output_rx) = make_test_engine();
    let (network, _cmd_rx, mut cmd_rx) = make_test_network();
    let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
    let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx)
        .with_profile1_gate(gate.with_clock(vote_clock));
    for (raw, view) in [(&f.block1, 1), (&header, 2)] {
        let (hash, broadcast) = block_data(raw, view);
        orch.handle_block_data(bincode::serialize(&broadcast).unwrap()).await;
        assert!(orch.pending_block_data.contains_key(&hash));
    }
    while cmd_rx.try_recv().is_ok() {}
    let block_hash = alloy_primitives::keccak256(&header);
    let vote = ConsensusMessage::Vote(Vote {
        view: 2,
        block_hash,
        voter: 0,
        signature: test_key(0x13).sign(b"test"),
    });

    // The vector's clock: the header is more than 2 s ahead, no vote leaves.
    VOTE_CLOCK.store(now, Ordering::SeqCst);
    orch.handle_engine_output(EngineOutput::SendToValidator(0, vote.clone())).await;
    assert!(cmd_rx.try_recv().is_err(), "B4: the Round-1 vote is held back");

    // Two seconds before the header's time the bound holds.
    let time = Profile1Header::decode(&header).unwrap().header.timestamp;
    VOTE_CLOCK.store(time - 2, Ordering::SeqCst);
    orch.handle_engine_output(EngineOutput::SendToValidator(0, vote)).await;
    assert!(matches!(cmd_rx.try_recv(), Ok(NetworkCommand::BroadcastConsensus(_))));

    // A vote for a block whose header was never admitted does not leave.
    let unknown = ConsensusMessage::Vote(Vote {
        view: 3,
        block_hash: B256::repeat_byte(0x77),
        voter: 0,
        signature: test_key(0x13).sign(b"test"),
    });
    orch.handle_engine_output(EngineOutput::SendToValidator(0, unknown)).await;
    assert!(cmd_rx.try_recv().is_err());
}

/// The same engine output on a chain without a profile is unchanged.
#[tokio::test]
async fn non_profile_votes_are_not_gated() {
    let (engine, output_rx) = make_test_engine();
    let (network, _cmd_rx, mut cmd_rx) = make_test_network();
    let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
    let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx);
    let vote = ConsensusMessage::Vote(Vote {
        view: 1,
        block_hash: B256::repeat_byte(0x77),
        voter: 0,
        signature: test_key(0x13).sign(b"test"),
    });
    orch.handle_engine_output(EngineOutput::SendToValidator(0, vote)).await;
    assert!(matches!(cmd_rx.try_recv(), Ok(NetworkCommand::BroadcastConsensus(_))));
}

/// A Profile-1 header remembered in the native header registry: V1's
/// block 2 moved to `number` under `parent`, with `change` applied.
fn remembered_header(number: u64, parent: B256, change: impl FnOnce(&mut Profile1Header)) -> Profile1Header {
    let (f, _) = v1();
    let mut header = Profile1Header::decode(&f.block2).unwrap();
    header.header.number = number;
    header.header.parent_hash = parent;
    change(&mut header);
    n42_consensus::remember_gov5_native_header(&header.encode());
    assert!(n42_consensus::remembered_profile1_header(&header.hash()).is_some());
    header
}

fn halted_orchestrator(
    gate: Profile1ImportGate,
) -> (
    ConsensusService,
    Arc<MockExecutionLayer>,
    Arc<crate::halt::Halt>,
    mpsc::Sender<NetworkEvent>,
) {
    let (engine, output_rx) = make_test_engine();
    let (network, _cmd_rx, _prx) = make_test_network();
    let (net_event_tx, net_event_rx) = mpsc::channel(8192);
    let halt = Arc::new(crate::halt::Halt::in_memory());
    let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx)
        .with_profile1_gate(gate)
        .with_halt(halt.clone());
    let mock = Arc::new(MockExecutionLayer::new(PayloadStatusEnum::Valid, PayloadStatusEnum::Valid));
    orch.el = Some(mock.clone());
    orch.async_finalize_fcu = false;
    (orch, mock, halt, net_event_tx)
}

/// E12 fault injection: a block committed by consensus whose carried state
/// root is not this node's result for its parent halts the node before it
/// imports, makes canonical, votes or builds anything, and the halted loop
/// processes nothing more.
#[tokio::test]
async fn e12_a_committed_block_with_wrong_carried_fields_halts() {
    let (_, gate) = v1();
    let (mut orch, mock, halt, net_event_tx) = halted_orchestrator(gate);
    let parent = B256::repeat_byte(0xe1);
    let header = remembered_header(41, parent, |header| {
        header.header.state_root = B256::repeat_byte(0xbd);
    });
    // This node's own result for the parent differs in the state root only.
    let mut own = n42_consensus::ExecutedFields::carried_by(&header.header);
    own.state_root = B256::repeat_byte(0x0d);
    n42_consensus::remember_executed_fields(parent, own).unwrap();
    let hash = header.hash();
    orch.pending_block_data
        .insert(hash, test_block_data(parent, hash, 7).into());

    orch.handle_engine_output(EngineOutput::BlockCommitted {
        view: 7,
        block_hash: hash,
        commit_qc: QuorumCertificate::genesis(),
        validator_changes: None,
    })
    .await;

    let evidence = halt.evidence().expect("halted");
    assert_eq!(evidence.view, 7);
    assert!(evidence.commit_qc.is_some(), "the commit QC is kept as evidence");
    match &evidence.reason {
        crate::halt::HaltReason::CarriedFields { block, parent: p, field, .. } => {
            assert_eq!((*block, *p, field.as_str()), (hash, parent, "stateRoot"));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(orch.committed_block_count, 0, "the commit is not acted on");
    assert!(mock.new_payload_hashes().is_empty(), "nothing imported");
    assert!(mock.fcu_heads().is_empty(), "nothing made canonical");

    // The halted loop drops a proposal and engine output unprocessed and
    // ends only when its channels close.
    let (_, block) = block_data(&header.encode(), 8);
    net_event_tx
        .send(NetworkEvent::BlockAnnouncement {
            source: libp2p::PeerId::random(),
            data: bincode::serialize(&block).unwrap(),
        })
        .await
        .unwrap();
    drop(net_event_tx);
    tokio::time::timeout(Duration::from_secs(5), orch.run())
        .await
        .expect("the halted loop ends when its channels close");
    assert!(mock.new_payload_hashes().is_empty());
    assert!(mock.fcu_heads().is_empty());
    assert!(mock.build_parents().is_empty(), "nothing built");
}

/// E12: a block the import refused for its carried fields halts the node
/// when it is committed, with the import's evidence.
#[tokio::test]
async fn e12_a_committed_block_the_import_refused_halts() {
    let (_, gate) = v1();
    let (mut orch, mock, halt, _net_event_tx) = halted_orchestrator(gate);
    let hash = B256::repeat_byte(0xe2);
    let carried = n42_consensus::ExecutedFields {
        state_root: B256::repeat_byte(1),
        receipts_root: B256::repeat_byte(2),
        logs_bloom: Default::default(),
        gas_used: 21_000,
        requests_hash: n42_chainspec::profile1::PROFILE1_REQUESTS_HASH,
    };
    let expected = n42_consensus::ExecutedFields { gas_used: 42_000, ..carried };
    n42_consensus::record_carried_mismatch(
        hash,
        n42_consensus::CarriedMismatch {
            parent: B256::repeat_byte(0xe3),
            field: "gasUsed",
            carried,
            expected,
        },
    );
    orch.handle_engine_output(EngineOutput::BlockCommitted {
        view: 3,
        block_hash: hash,
        commit_qc: QuorumCertificate::genesis(),
        validator_changes: None,
    })
    .await;
    match halt.evidence().expect("halted").reason {
        crate::halt::HaltReason::CarriedFields { field, own, .. } => {
            assert_eq!(field, "gasUsed");
            assert_eq!(own["gasUsed"], 42_000);
        }
        other => panic!("{other:?}"),
    }
    assert!(mock.fcu_heads().is_empty());
}

/// E12: two committed blocks at one height halt the node; the first commit
/// alone does not.
#[tokio::test]
async fn e12_a_conflicting_commit_halts() {
    let (_, gate) = v1();
    let (mut orch, _mock, halt, _net_event_tx) = halted_orchestrator(gate);
    let parent = B256::repeat_byte(0xe4);
    let first = remembered_header(52, parent, |_| {});
    let second = remembered_header(52, parent, |header| header.header.timestamp += 1);
    for (view, header) in [(5, &first), (6, &second)] {
        let hash = header.hash();
        orch.pending_block_data
            .insert(hash, test_block_data(parent, hash, view).into());
        orch.handle_engine_output(EngineOutput::BlockCommitted {
            view,
            block_hash: hash,
            commit_qc: QuorumCertificate::genesis(),
            validator_changes: None,
        })
        .await;
        assert_eq!(halt.is_halted(), view == 6, "view {view}");
    }
    assert_eq!(
        halt.evidence().unwrap().reason,
        crate::halt::HaltReason::ConflictingCommit {
            number: 52,
            committed: first.hash(),
            conflicting: second.hash(),
        }
    );
}

/// Off Profile 1 there is no halt latch: the same commit proceeds.
#[tokio::test]
async fn e12_needs_a_halt_latch() {
    let (_, gate) = v1();
    let mut orch = profile1_orchestrator(gate);
    let hash = B256::repeat_byte(0xe5);
    n42_consensus::record_carried_mismatch(
        hash,
        n42_consensus::CarriedMismatch {
            parent: B256::ZERO,
            field: "stateRoot",
            carried: n42_consensus::ExecutedFields::carried_by(&Default::default()),
            expected: n42_consensus::ExecutedFields::carried_by(&Default::default()),
        },
    );
    assert!(!orch.profile1_check_committed(1, hash, None));
}

/// E14: on Profile 1 the commit's forkchoice update makes the committed
/// block the head, its parent safe and the highest persisted ancestor
/// finalized; off Profile 1 all three are the committed block.
#[tokio::test]
async fn e14_commit_forkchoice_tags() {
    let p8 = B256::repeat_byte(0x48);
    let p9 = B256::repeat_byte(0x49);
    let head = remembered_header(10, p9, |header| header.header.gas_limit += 1).hash();
    for profile1 in [true, false] {
        let (_, gate) = v1();
        let (engine, output_rx) = make_test_engine();
        let (network, _cmd_rx, _prx) = make_test_network();
        let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
        let mut orch = ConsensusService::new(engine, Arc::new(network), net_event_rx, output_rx)
            .with_profile1_gate(gate);
        if profile1 {
            let links = std::collections::HashMap::from([
                (head, (10u64, p9)),
                (p9, (9, p8)),
                (p8, (8, B256::repeat_byte(0x47))),
            ]);
            orch = orch.with_fcu_tags(crate::fcu_tags::ForkchoiceTags::new(
                move |hash| links.get(hash).copied(),
                move || Some((8, p8)),
            ));
        }
        let mock = Arc::new(MockExecutionLayer::new(PayloadStatusEnum::Valid, PayloadStatusEnum::Valid));
        orch.el = Some(mock.clone());
        orch.async_finalize_fcu = false;
        orch.head_block_hash = p9;
        orch.pending_block_data
            .insert(head, test_block_data(p9, head, 10).into());
        orch.handle_engine_output(EngineOutput::BlockCommitted {
            view: 10,
            block_hash: head,
            commit_qc: QuorumCertificate::genesis(),
            validator_changes: None,
        })
        .await;
        orch.handle_commit_execution_timeout(10, head);
        if let Ok(Some((hash, view, outcome, ts))) =
            tokio::time::timeout(Duration::from_secs(2), orch.import_done_rx.recv()).await
        {
            orch.handle_import_done(hash, view, outcome, ts).await;
        }
        let states = mock.fcu_states();
        let expected = if profile1 { (head, p9, p8) } else { (head, head, head) };
        assert!(states.contains(&expected), "profile1={profile1}: {states:?}");
        if profile1 {
            assert!(
                states.iter().all(|(_, safe, finalized)| *safe != head && *finalized != head),
                "the committed head is never safe or finalized on Profile 1: {states:?}"
            );
        }
    }
}

/// Phase B pitfall (design 3.6): with Round 1 cast on the check, a quorum
/// can commit a block before this node's import of it completes. The commit
/// is held, not re-imported beside the running import, and is finalised
/// when that import reports Valid.
#[tokio::test]
async fn phase_b_a_decide_before_the_import_is_queued() {
    let (_, gate) = v1();
    let (mut orch, mock, _halt, _net_event_tx) = halted_orchestrator(gate);
    let parent = B256::repeat_byte(0x61);
    let hash = B256::repeat_byte(0x62);
    orch.head_block_hash = parent;
    orch.pending_block_data
        .insert(hash, test_block_data(parent, hash, 4).into());
    let in_flight = EagerImportInFlight::enter(&orch.eager_imports_in_flight, hash);

    orch.handle_engine_output(EngineOutput::BlockCommitted {
        view: 4,
        block_hash: hash,
        commit_qc: QuorumCertificate::genesis(),
        validator_changes: None,
    })
    .await;
    orch.handle_commit_execution_timeout(4, hash);
    assert!(mock.new_payload_hashes().is_empty(), "no second import beside the running one");
    assert_ne!(orch.head_block_hash, hash, "not canonical before its import");

    drop(in_flight);
    orch.handle_eager_import_done(hash, 1, parent, 1, false, None).await;
    assert_eq!(orch.head_block_hash, hash, "finalised on the import's Valid");
    assert!(mock.fcu_heads().contains(&hash));
}

/// Phase B in the orchestrator: a check outcome becomes the engine event;
/// Retry and Unsupported leave the vote to the import.
#[tokio::test]
async fn phase_b_check_outcomes_reach_the_engine() {
    use crate::deferred_check::DeferredCheck;
    let (_, gate) = v1();
    let mut orch = profile1_orchestrator(gate);
    let (block, parent) = (B256::repeat_byte(0x71), B256::repeat_byte(0x70));
    orch.handle_deferred_check_done(block, parent, DeferredCheck::Retry("parent".into()));
    orch.handle_deferred_check_done(block, parent, DeferredCheck::Unsupported);
    orch.handle_deferred_check_done(block, parent, DeferredCheck::Refused("nonce".into()));
    orch.handle_deferred_check_done(block, parent, DeferredCheck::Checked);
    // The engine is not on Profile-1 vote rules in this harness; the calls
    // above must simply be accepted without output.
    assert!(orch.output_rx.try_recv().is_err());
}

/// gov5's V6 messages (every consensus message kind) in the engine's form.
fn v6_messages() -> Vec<ConsensusMessage> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../n42-chainspec/testdata/profile1/V6/consensus_messages_v6.json");
    let json: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let identity = n42_network::h2_v4::H2V4ChainIdentity {
        chain_id: json["chainId"].as_u64().unwrap(),
        genesis_hash: json["genesisHash"].as_str().unwrap().parse().unwrap(),
    };
    json["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| {
            let envelope =
                n42_network::h2_v4::decode_gossip(&raw(&message["gossipSnappy"]), identity).unwrap();
            n42_network::consensus_from_h2_v4(envelope).unwrap()
        })
        .collect()
}

/// A follower (AM6) on Profile 1 with an H2-v4 transport, the halt latch
/// and a recording network under its follower wrapper.
fn follower_orchestrator(
    gate: Profile1ImportGate,
) -> (ConsensusService, MockConsensusNetwork, Arc<MockExecutionLayer>, Arc<crate::halt::Halt>) {
    let (engine, output_rx) = make_test_engine();
    let network = MockConsensusNetwork::default();
    let (_net_event_tx, net_event_rx) = mpsc::channel(8192);
    let halt = Arc::new(crate::halt::Halt::in_memory());
    let mut orch = ConsensusService::new(engine, Arc::new(network.clone()), net_event_rx, output_rx)
        .with_h2_v4_participant(test_h2_identity())
        .with_profile1_gate(gate)
        .with_halt(halt.clone())
        .with_follower();
    let mock = Arc::new(MockExecutionLayer::new(PayloadStatusEnum::Valid, PayloadStatusEnum::Valid));
    orch.el = Some(mock.clone());
    orch.async_finalize_fcu = false;
    (orch, network, mock, halt)
}

fn assert_nothing_published(network: &MockConsensusNetwork) {
    assert!(network.broadcasts().is_empty(), "{:?}", network.broadcasts());
    assert!(network.h2_v4_broadcasts().is_empty(), "{:?}", network.h2_v4_broadcasts());
    assert!(network.direct_messages().is_empty(), "{:?}", network.direct_messages());
}

/// AM6 guard: whatever the engine (or any other caller) asks, a follower
/// publishes no vote, commit vote, timeout, NewView, proposal, QC or Decide,
/// and never builds; requests it needs to follow (range fetch) still leave.
#[tokio::test]
async fn follower_publishes_no_consensus_message() {
    let (_, gate) = v1();
    let (mut orch, network, mock, _halt) = follower_orchestrator(gate);
    assert!(orch.is_follower());
    assert_eq!(
        orch.range_catchup.mode(),
        crate::range_catchup::AnchorMode::Enforce,
        "Profile 1 range catch-up enforces anchors (§3)"
    );
    let messages = v6_messages();
    let kinds: std::collections::BTreeSet<_> =
        messages.iter().map(crate::net_port::consensus_message_kind).collect();
    assert_eq!(kinds.len(), 7, "every message kind: {kinds:?}");
    for message in messages {
        orch.handle_engine_output(EngineOutput::BroadcastMessage(message.clone())).await;
        orch.handle_engine_output(EngineOutput::SendToValidator(0, message.clone())).await;
        // The network wrapper refuses the same messages on its own.
        orch.broadcast_engine_consensus(message.clone()).unwrap();
        orch.broadcast_via_rotor(message.clone());
        orch.network.send_direct(libp2p::PeerId::random(), message.clone()).unwrap();
        orch.network.broadcast_consensus(message).unwrap();
    }
    assert!(orch.pending_vote_resend.is_none(), "no vote kept for a resend");
    orch.network.announce_block_reliable(vec![1]).await.unwrap();
    orch.network.broadcast_gov5_block_reliable(vec![1]).await.unwrap();
    orch.network.broadcast_blob_sidecar(vec![1]).unwrap();
    orch.do_trigger_payload_build(None).await;
    assert!(mock.build_parents().is_empty(), "a follower never builds");
    assert_nothing_published(&network);

    let peer = libp2p::PeerId::random();
    orch.network.request_gov5_range(peer, 1, 8).unwrap();
    assert_eq!(network.range_requests.lock().unwrap().as_slice(), &[(peer, 1, 8)]);
}

/// AM6 follower on a Profile-1 chain: it imports and makes canonical the
/// blocks the validators commit, each carrying its parent's result equal to
/// this node's own (A10), then halts (E12) on a committed block whose carried
/// state root is not its own result for the parent, importing nothing more.
/// It publishes nothing throughout.
#[tokio::test]
async fn follower_imports_a_profile1_chain_and_halts_on_a_tampered_carried_field() {
    let (_, gate) = v1();
    let (mut orch, network, mock, halt) = follower_orchestrator(gate);
    let genesis = B256::repeat_byte(0xf0);
    orch.head_block_hash = genesis;
    let mut parent = genesis;
    let mut chain = Vec::new();
    for number in 71..=74u64 {
        let header = remembered_header(number, parent, |header| {
            header.header.gas_limit += number;
        });
        // This node's own execution of the parent is what the child carries.
        n42_consensus::remember_executed_fields(
            parent,
            n42_consensus::ExecutedFields::carried_by(&header.header),
        )
        .unwrap();
        chain.push(header.hash());
        parent = header.hash();
    }
    for (view, hash) in (20u64..).zip(chain.iter().copied()) {
        let parent = orch.head_block_hash;
        orch.pending_block_data.insert(hash, test_block_data(parent, hash, view).into());
        orch.handle_engine_output(EngineOutput::BlockCommitted {
            view,
            block_hash: hash,
            commit_qc: QuorumCertificate::genesis(),
            validator_changes: None,
        })
        .await;
        orch.handle_commit_execution_timeout(view, hash);
        if let Ok(Some((hash, view, outcome, ts))) =
            tokio::time::timeout(Duration::from_secs(2), orch.import_done_rx.recv()).await
        {
            orch.handle_import_done(hash, view, outcome, ts).await;
        }
        assert_eq!(orch.head_block_hash, hash, "view {view}: committed and canonical");
    }
    assert!(!halt.is_halted());
    assert_eq!(mock.new_payload_hashes(), chain, "every block imported, in order");
    for hash in &chain {
        assert!(mock.fcu_heads().contains(hash));
    }

    // The next committed block carries a state root that is not this node's
    // own result for its parent.
    let tampered = remembered_header(75, parent, |header| {
        header.header.state_root = B256::repeat_byte(0xbd);
    });
    let mut own = n42_consensus::ExecutedFields::carried_by(&tampered.header);
    own.state_root = B256::repeat_byte(0x0d);
    n42_consensus::remember_executed_fields(parent, own).unwrap();
    let hash = tampered.hash();
    orch.pending_block_data.insert(hash, test_block_data(parent, hash, 30).into());
    orch.handle_engine_output(EngineOutput::BlockCommitted {
        view: 30,
        block_hash: hash,
        commit_qc: QuorumCertificate::genesis(),
        validator_changes: None,
    })
    .await;
    match halt.evidence().expect("E12 halt").reason {
        crate::halt::HaltReason::CarriedFields { block, field, .. } => {
            assert_eq!((block, field.as_str()), (hash, "stateRoot"));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(mock.new_payload_hashes(), chain, "the tampered block is not imported");
    assert_eq!(orch.head_block_hash, parent, "nor made canonical");
    assert_nothing_published(&network);
}
