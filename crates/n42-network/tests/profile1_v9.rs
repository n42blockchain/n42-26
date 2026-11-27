//! gov5's profile-1 vector V9 (GossipSub parameters, v1.2;
//! `testdata/profile1/gov5/V9`, a byte-exact copy of gov5 08ecf671
//! `internal/testdata/profile1/vectors/V9`) against this node's profile-1
//! router (`gossipsub::profile1`).
//!
//! The wire contract -- topic strings (G2), the message id (G3) and the
//! message-size cap after profile 1's `maxBlockBytes` (B15) -- must match
//! exactly. The mesh, gossip and history values (G4) are asserted wherever
//! rust-libp2p's gossipsub has the same knob; the go-libp2p-pubsub fields it
//! has no counterpart for are listed in [`NO_COUNTERPART`], so a new field in
//! the vector fails the test until it is placed.
//!
//! Peer scoring is not compared: AM34 makes it a local choice outside the
//! wire contract. Which topics the service joins (G2 only, no cohort topics,
//! AM34 items 4-5) is tested in `service::tests` (`profile1_*`), where the
//! swarm is reachable.

use std::time::Duration;

use alloy_primitives::B256;
use n42_network::gossipsub::profile1::{
    self, g2_topics, gossipsub_config, gov5_block_fits, max_gossip_wire_size,
};
use n42_network::gossipsub::topics::{gov5_block_topic, gov5_h2_topic, gov5_tx_topic, h2_v4_topic};
use n42_network::gossipsub::{MESSAGE_ID_LEN, message_id_parts};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// SHA-256 of `gossipsub_parameters.json` as gov5 published it (08ecf671).
const PARAMETERS_SHA256: &str = "b315f30b8b9682486ab750f7aa7ab3bbaa84bd62f8941c74492208479fad8716";

/// Profile 1's `maxBlockBytes` (B15) in the standard genesis and V1's.
const PROFILE1_MAX_BLOCK_BYTES: u64 = 64 << 20;

/// go-libp2p-pubsub `GossipSubParams` fields rust-libp2p's gossipsub has no
/// setting for (dialing belongs to the swarm; IDONTWANT bookkeeping and the
/// slow-heartbeat warning are internal there).
const NO_COUNTERPART: &[&str] = &[
    "ConnectionTimeout",
    "Connectors",
    "MaxPendingConnections",
    "DirectConnectInitialDelay",
    "SlowHeartbeatWarning",
    "IDontWantMessageTTL",
    "MaxIDontWantLength",
    "MaxIDontWantMessages",
];

fn vector() -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("testdata/profile1/gov5/V9/gossipsub_parameters.json");
    let raw = std::fs::read(path).expect("V9 vector");
    assert_eq!(
        hex::encode(Sha256::digest(&raw)),
        PARAMETERS_SHA256,
        "V9 is not gov5's byte-exact copy"
    );
    serde_json::from_slice(&raw).expect("json")
}

fn genesis(v: &Value) -> B256 {
    v["genesisHash"]
        .as_str()
        .expect("hash")
        .parse()
        .expect("B256")
}

fn number(value: &Value) -> u64 {
    value.as_u64().expect("an integer")
}

fn ms(value: &Value) -> Duration {
    Duration::from_millis(number(value))
}

fn config(v: &Value) -> libp2p::gossipsub::Config {
    gossipsub_config(genesis(v), PROFILE1_MAX_BLOCK_BYTES).expect("the profile-1 config")
}

#[test]
fn v9_topic_strings_match_gov5() {
    let v = vector();
    let genesis = genesis(&v);
    let digest = hex::encode(&genesis[..4]);
    assert_eq!(digest, v["forkDigest"].as_str().expect("digest"));
    let topics = v["topics"].as_array().expect("topics");
    assert_eq!(topics.len(), 11);
    for t in topics {
        let name = t["name"].as_str().expect("name");
        let expected = t["topic"].as_str().expect("topic");
        let ours = match name {
            "block" => gov5_block_topic(genesis).to_string(),
            "transaction_v2" => gov5_tx_topic(genesis).to_string(),
            "hotstuff_consensus" => gov5_h2_topic(genesis).to_string(),
            "h2/4" => h2_v4_topic().to_string(),
            // Topics this node never joins on profile 1 (not in G2, or no
            // mobileverify service yet): the same string rule.
            other => format!("/n42/{digest}/{other}/ssz_snappy"),
        };
        assert_eq!(ours, expected, "topic {name}");
    }
    let in_g2: Vec<&str> = topics
        .iter()
        .filter(|t| t["inG2"].as_bool() == Some(true))
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        in_g2,
        [
            "block",
            "transaction_v2",
            "hotstuff_consensus",
            "h2/4",
            "mobileverify_packet",
            "mobileverify_registration"
        ]
    );
    // AM34 item 4: what this node joins is inside G2. G2's mobileverify
    // topics are left out until this node runs the mobileverify service
    // (I1); every topic outside G2 (blob_sidecar, zk_proof, the cohort
    // topics) is never joined.
    let g2: Vec<&str> = topics
        .iter()
        .filter(|t| t["inG2"].as_bool() == Some(true))
        .map(|t| t["topic"].as_str().unwrap())
        .collect();
    for takes_transactions in [true, false] {
        let ours: Vec<String> = g2_topics(genesis, takes_transactions)
            .iter()
            .map(ToString::to_string)
            .collect();
        for topic in &ours {
            assert!(g2.contains(&topic.as_str()), "{topic} is not a G2 topic");
            assert!(!topic.contains("cohort"), "{topic}: AM34 item 5");
        }
        assert_eq!(ours.len(), 3 + usize::from(takes_transactions));
        assert_eq!(
            ours.contains(&gov5_tx_topic(genesis).to_string()),
            takes_transactions
        );
    }
}

#[test]
fn v9_message_ids_match_gov5() {
    let v = vector();
    let genesis = genesis(&v);
    let rows = v["messageIds"].as_array().expect("messageIds");
    assert_eq!(rows.len(), 4);
    assert_eq!(MESSAGE_ID_LEN, 20);
    for row in rows {
        let topic = row["topic"].as_str().expect("topic");
        let data =
            hex::decode(row["data"].as_str().expect("data").trim_start_matches("0x")).expect("hex");
        assert_eq!(
            format!("0x{}", hex::encode(message_id_parts(genesis, topic, &data))),
            row["messageId"].as_str().expect("id"),
            "{topic} / {}",
            row["data"]
        );
    }
}

#[test]
fn v9_router_options_match_gov5() {
    let v = vector();
    let router = &v["router"];
    let cap = number(&router["maxDecodedMessageBytes"]);
    assert_eq!(cap, PROFILE1_MAX_BLOCK_BYTES);
    // B15: the router cap is the snappy bound of maxBlockBytes.
    assert_eq!(
        max_gossip_wire_size(cap as usize) as u64,
        number(&router["maxMessageSizeBytes"])
    );
    let cfg = config(&v);
    assert_eq!(
        cfg.max_transmit_size() as u64,
        number(&router["maxMessageSizeBytes"])
    );
    // The decoded cap of one gov5 block on gossip.
    assert!(gov5_block_fits(cap as usize, cap as usize));
    assert!(!gov5_block_fits(cap as usize, cap as usize + 1));
    // StrictNoSign + no author = rust-libp2p's anonymous validation.
    assert_eq!(router["messageSignaturePolicy"], "StrictNoSign");
    assert!(router["noAuthor"].as_bool().expect("bool"));
    assert!(matches!(
        cfg.validation_mode(),
        libp2p::gossipsub::ValidationMode::Anonymous
    ));
    assert_eq!(cfg.duplicate_cache_time(), ms(&router["seenMessagesTTLMs"]));
    assert_eq!(
        profile1::SEEN_MESSAGES_TTL,
        ms(&router["seenMessagesTTLMs"])
    );
    assert_eq!(router["protocolSuffix"], "/ssz_snappy");
    assert_eq!(router["subscriptionFilterPrefix"], "/n42/");
}

#[test]
fn v9_gossipsub_params_match_gov5() {
    let v = vector();
    let p = &v["gossipSubParams"];
    let cfg = config(&v);
    let checks: &[(&str, u64, u64)] = &[
        ("D", cfg.mesh_n() as u64, number(&p["D"])),
        ("Dlo", cfg.mesh_n_low() as u64, number(&p["Dlo"])),
        ("Dhi", cfg.mesh_n_high() as u64, number(&p["Dhi"])),
        ("Dlazy", cfg.gossip_lazy() as u64, number(&p["Dlazy"])),
        ("Dout", cfg.mesh_outbound_min() as u64, number(&p["Dout"])),
        ("Dscore", cfg.retain_scores() as u64, number(&p["Dscore"])),
        (
            "HistoryLength",
            cfg.history_length() as u64,
            number(&p["HistoryLength"]),
        ),
        (
            "HistoryGossip",
            cfg.history_gossip() as u64,
            number(&p["HistoryGossip"]),
        ),
        (
            "GossipRetransmission",
            u64::from(cfg.gossip_retransimission()),
            number(&p["GossipRetransmission"]),
        ),
        (
            "OpportunisticGraftPeers",
            cfg.opportunistic_graft_peers() as u64,
            number(&p["OpportunisticGraftPeers"]),
        ),
        (
            "OpportunisticGraftTicks",
            cfg.opportunistic_graft_ticks(),
            number(&p["OpportunisticGraftTicks"]),
        ),
        (
            "DirectConnectTicks",
            cfg.check_explicit_peers_ticks(),
            number(&p["DirectConnectTicks"]),
        ),
        (
            "MaxIHaveLength",
            cfg.max_control_messages_sent() as u64,
            number(&p["MaxIHaveLength"]),
        ),
        (
            "MaxIHaveMessages",
            cfg.max_ihave_messages_heartbeat() as u64,
            number(&p["MaxIHaveMessages"]),
        ),
        (
            "IDontWantMessageThreshold",
            cfg.idontwant_message_size_threshold() as u64,
            number(&p["IDontWantMessageThreshold"]),
        ),
        (
            "HeartbeatInterval",
            cfg.heartbeat_interval().as_millis() as u64,
            number(&p["HeartbeatInterval"]),
        ),
        (
            "HeartbeatInitialDelay",
            cfg.heartbeat_initial_delay().as_millis() as u64,
            number(&p["HeartbeatInitialDelay"]),
        ),
        (
            "FanoutTTL",
            cfg.fanout_ttl().as_millis() as u64,
            number(&p["FanoutTTL"]),
        ),
        (
            "PruneBackoff",
            cfg.prune_backoff().as_millis() as u64,
            number(&p["PruneBackoff"]),
        ),
        (
            "UnsubscribeBackoff",
            cfg.unsubscribe_backoff().as_millis() as u64,
            number(&p["UnsubscribeBackoff"]),
        ),
        (
            "GraftFloodThreshold",
            cfg.graft_flood_threshold().as_millis() as u64,
            number(&p["GraftFloodThreshold"]),
        ),
        (
            "IWantFollowupTime",
            cfg.iwant_followup_time().as_millis() as u64,
            number(&p["IWantFollowupTime"]),
        ),
    ];
    let diffs: Vec<String> = checks
        .iter()
        .filter(|(_, ours, gov5)| ours != gov5)
        .map(|(name, ours, gov5)| format!("{name}: ours {ours}, gov5 {gov5}"))
        .collect();
    assert!(
        diffs.is_empty(),
        "GossipSub parameters differ from gov5: {diffs:?}"
    );
    assert_eq!(
        cfg.gossip_factor(),
        p["GossipFactor"].as_f64().expect("float"),
        "GossipFactor"
    );
    // PrunePeers is the peer-exchange list a PRUNE carries; neither gov5 nor
    // this node runs peer exchange, so the count is never used.
    assert_eq!(number(&p["PrunePeers"]), 16);
    assert!(!cfg.do_px());

    // Every field of the vector is asserted above or listed as having no
    // counterpart.
    let asserted: Vec<&str> = checks
        .iter()
        .map(|(name, _, _)| *name)
        .chain(["GossipFactor", "PrunePeers"])
        .collect();
    for field in p.as_object().expect("params").keys() {
        assert!(
            asserted.contains(&field.as_str()) || NO_COUNTERPART.contains(&field.as_str()),
            "gossipSubParams.{field} is neither asserted nor listed"
        );
    }
}
