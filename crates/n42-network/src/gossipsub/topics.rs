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

/// GossipSub topic for transaction pool (mempool) synchronization.
///
/// Transactions submitted via RPC to any node are broadcast to all peers
/// via this topic. This ensures that when a different node becomes leader,
/// its transaction pool contains transactions from other nodes.
pub fn mempool_topic() -> IdentTopic {
    IdentTopic::new("/n42/mempool/1")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_topic_strings() {
        // Verify each topic function returns the expected topic string.
        // IdentTopic stores the raw string; its hash is derived from it.
        // We compare the hash against a freshly-created IdentTopic with the
        // expected string to confirm they match.
        let consensus = consensus_topic();
        assert_eq!(
            consensus.hash(),
            IdentTopic::new("/n42/consensus/1").hash(),
            "consensus topic should be /n42/consensus/1"
        );

        let blocks = block_announce_topic();
        assert_eq!(
            blocks.hash(),
            IdentTopic::new("/n42/blocks/1").hash(),
            "block announce topic should be /n42/blocks/1"
        );

        let verification = verification_receipts_topic();
        assert_eq!(
            verification.hash(),
            IdentTopic::new("/n42/verification/1").hash(),
            "verification receipts topic should be /n42/verification/1"
        );

        // Also verify that the three topics are distinct from each other.
        assert_ne!(consensus.hash(), blocks.hash(), "consensus and blocks topics should differ");
        assert_ne!(consensus.hash(), verification.hash(), "consensus and verification topics should differ");
        assert_ne!(blocks.hash(), verification.hash(), "blocks and verification topics should differ");
    }
}
