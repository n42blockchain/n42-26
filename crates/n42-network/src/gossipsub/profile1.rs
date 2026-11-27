//! GossipSub on a standard-profile-1 chain: gov5's router values (G4, vector
//! V9), the G2 topic list (AM34 items 4-5) and the message-size cap after
//! `maxBlockBytes` (B15).
//!
//! The wire contract is the topic strings, the message id (G3,
//! [`super::message_id_fn`]) and the size cap; those must equal gov5's
//! exactly. The mesh, gossip and history values are local router behaviour
//! (a client with others still interoperates) but are set to gov5's so a
//! Rust member grafts and prunes like a Go one. Every value below is V9's
//! `gossipSubParams` / `router` (gov5 `pubsubGossipParam`, go-libp2p-pubsub
//! v0.18.0 defaults where gov5 sets nothing).
//!
//! Peer scoring is not part of the contract (AM34: "a local choice"); this
//! node keeps its own.

use std::time::Duration;

use alloy_primitives::B256;
use libp2p::gossipsub::{self, IdentTopic};

use super::message_id_fn;
use super::topics::{gov5_block_topic, gov5_h2_topic, gov5_tx_topic, h2_v4_topic};

/// gov5 `gossipSubD`: target mesh degree.
pub const D: usize = 8;
/// gov5 `gossipSubDlo`: low watermark.
pub const D_LO: usize = 6;
/// go-libp2p-pubsub `GossipSubDhi` (gov5 keeps the default).
pub const D_HI: usize = 12;
/// go-libp2p-pubsub `GossipSubDlazy`.
pub const D_LAZY: usize = 6;
/// go-libp2p-pubsub `GossipSubDout`.
pub const D_OUT: usize = 2;
/// go-libp2p-pubsub `GossipSubDscore`.
pub const D_SCORE: usize = 4;
/// gov5 `gossipSubHeartbeatInterval`.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(700);
/// go-libp2p-pubsub `GossipSubHeartbeatInitialDelay` (rust-libp2p: 5 s).
pub const HEARTBEAT_INITIAL_DELAY: Duration = Duration::from_millis(100);
/// gov5 `gossipSubMcacheLen` (rust-libp2p: 5).
pub const HISTORY_LENGTH: usize = 6;
/// gov5 `gossipSubMcacheGossip`.
pub const HISTORY_GOSSIP: usize = 3;
/// gov5 `seenMessagesTTL` (rust-libp2p: 60 s).
pub const SEEN_MESSAGES_TTL: Duration = Duration::from_secs(30);
/// go-libp2p-pubsub `GossipSubIDontWantMessageThreshold` (rust-libp2p: 1000).
pub const IDONTWANT_MESSAGE_THRESHOLD: usize = 1024;
/// go-libp2p-pubsub `GossipSubMaxIHaveLength`.
pub const MAX_IHAVE_LENGTH: usize = 5000;
/// go-libp2p-pubsub `GossipSubMaxIHaveMessages`.
pub const MAX_IHAVE_MESSAGES: usize = 10;
/// go-libp2p-pubsub `GossipSubOpportunisticGraftTicks`.
pub const OPPORTUNISTIC_GRAFT_TICKS: u64 = 60;
/// go-libp2p-pubsub `GossipSubOpportunisticGraftPeers`.
pub const OPPORTUNISTIC_GRAFT_PEERS: usize = 2;
/// go-libp2p-pubsub `GossipSubGossipRetransmission`.
pub const GOSSIP_RETRANSMISSION: u32 = 3;
/// go-libp2p-pubsub `GossipSubGossipFactor`.
pub const GOSSIP_FACTOR: f64 = 0.25;
/// go-libp2p-pubsub `GossipSubDirectConnectTicks`.
pub const DIRECT_CONNECT_TICKS: u64 = 300;
/// go-libp2p-pubsub `GossipSubFanoutTTL`.
pub const FANOUT_TTL: Duration = Duration::from_secs(60);
/// go-libp2p-pubsub `GossipSubPruneBackoff`.
pub const PRUNE_BACKOFF: Duration = Duration::from_secs(60);
/// go-libp2p-pubsub `GossipSubUnsubscribeBackoff`.
pub const UNSUBSCRIBE_BACKOFF: Duration = Duration::from_secs(10);
/// go-libp2p-pubsub `GossipSubGraftFloodThreshold`.
pub const GRAFT_FLOOD_THRESHOLD: Duration = Duration::from_secs(10);
/// go-libp2p-pubsub `GossipSubIWantFollowupTime`.
pub const IWANT_FOLLOWUP_TIME: Duration = Duration::from_secs(3);

/// The router's message-size cap for a chain whose blocks are at most
/// `max_block_bytes` decoded (B15): gov5's `GossipMaxSize`, Go's
/// `snappy.MaxEncodedLen(n) = 32 + n + n/6`. 78 293 706 B for 64 MiB.
pub const fn max_gossip_wire_size(max_block_bytes: usize) -> usize {
    32 + max_block_bytes + max_block_bytes / 6
}

/// GossipSub as a profile-1 node runs it.
///
/// [`gossipsub::ValidationMode::Anonymous`] is gov5's `StrictNoSign` plus
/// `WithNoAuthor`: messages carry no author, sequence number or signature,
/// and one that does is refused. It pairs with
/// [`gossipsub::MessageAuthenticity::Anonymous`] on the behaviour.
pub fn gossipsub_config(
    genesis_hash: B256,
    max_block_bytes: u64,
) -> Result<gossipsub::Config, gossipsub::ConfigBuilderError> {
    let max_block_bytes = usize::try_from(max_block_bytes).unwrap_or(usize::MAX / 2);
    gossipsub::ConfigBuilder::default()
        .mesh_n(D)
        .mesh_n_low(D_LO)
        .mesh_n_high(D_HI)
        .gossip_lazy(D_LAZY)
        .mesh_outbound_min(D_OUT)
        .retain_scores(D_SCORE)
        .heartbeat_interval(HEARTBEAT_INTERVAL)
        .heartbeat_initial_delay(HEARTBEAT_INITIAL_DELAY)
        .history_length(HISTORY_LENGTH)
        .history_gossip(HISTORY_GOSSIP)
        .duplicate_cache_time(SEEN_MESSAGES_TTL)
        .idontwant_message_size_threshold(IDONTWANT_MESSAGE_THRESHOLD)
        .max_control_messages_sent(MAX_IHAVE_LENGTH)
        .max_ihave_messages_heartbeat(MAX_IHAVE_MESSAGES)
        .opportunistic_graft_ticks(OPPORTUNISTIC_GRAFT_TICKS)
        .opportunistic_graft_peers(OPPORTUNISTIC_GRAFT_PEERS)
        .gossip_retransimission(GOSSIP_RETRANSMISSION)
        .gossip_factor(GOSSIP_FACTOR)
        .check_explicit_peers_ticks(DIRECT_CONNECT_TICKS)
        .fanout_ttl(FANOUT_TTL)
        .prune_backoff(PRUNE_BACKOFF)
        .unsubscribe_backoff(UNSUBSCRIBE_BACKOFF)
        .graft_flood_threshold(GRAFT_FLOOD_THRESHOLD)
        .iwant_followup_time(IWANT_FOLLOWUP_TIME)
        .max_transmit_size(max_gossip_wire_size(max_block_bytes))
        .validation_mode(gossipsub::ValidationMode::Anonymous)
        .message_id_fn(message_id_fn(genesis_hash))
        .build()
}

/// The G2 topics a profile-1 node subscribes to (AM34 item 4): gov5's
/// `hotstuff_consensus`, `block` and H2-v4, plus `transaction_v2` when the
/// node takes transactions (a participant). Nothing else: not the native
/// `/n42/*/1` topics, not `blob_sidecar` (no blobs, B9), not `zk_proof`, and
/// not the mobileverify cohort topics (AM34 item 5).
///
/// G2's `mobileverify_{packet,registration}` are not joined: this node has no
/// mobileverify service yet (I1), and joining a topic it cannot validate
/// would only make it a relay for traffic it does not read.
pub fn g2_topics(genesis_hash: B256, takes_transactions: bool) -> Vec<IdentTopic> {
    let mut topics = vec![
        gov5_block_topic(genesis_hash),
        gov5_h2_topic(genesis_hash),
        h2_v4_topic(),
    ];
    if takes_transactions {
        topics.push(gov5_tx_topic(genesis_hash));
    }
    topics
}

/// What the router cap means for one decoded gov5 block on gossip: at most
/// `max_block_bytes` (B15).
pub const fn gov5_block_fits(max_block_bytes: usize, decoded_bytes: usize) -> bool {
    decoded_bytes <= max_block_bytes
}
