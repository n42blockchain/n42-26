use libp2p::gossipsub::IdentTopic;

/// GossipSub topic for all HotStuff-2 consensus messages.
///
/// All consensus messages (Proposal, Vote, CommitVote, Timeout, NewView)
/// are published to this single topic. With 100-500 validators and small
/// message sizes (~128-500 bytes), a single topic provides sufficient
/// throughput without the complexity of per-message-type routing.
pub fn consensus_topic() -> IdentTopic {
    IdentTopic::new("/n42/consensus/1")
}

/// GossipSub topic for block announcements (header-first dissemination).
///
/// Block headers are announced on this topic first. Full block bodies
/// are fetched on-demand via request-response after header validation.
pub fn block_announce_topic() -> IdentTopic {
    IdentTopic::new("/n42/blocks/1")
}

/// GossipSub topic for mobile verification receipt aggregates.
///
/// Aggregated verification receipts from mobile nodes are published here
/// for other IDC nodes to observe mobile attestation coverage.
pub fn verification_receipts_topic() -> IdentTopic {
    IdentTopic::new("/n42/verification/1")
}
