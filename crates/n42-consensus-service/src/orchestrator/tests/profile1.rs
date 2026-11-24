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
