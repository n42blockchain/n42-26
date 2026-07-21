use alloy_primitives::B256;
use libp2p::gossipsub::IdentTopic;

/// GossipSub topic for all HotStuff-2 consensus messages.
///
/// All consensus messages (Proposal, Vote, CommitVote, Timeout, NewView)
/// are published to this single topic. With 100-500 validators and small
/// message sizes (~128-500 bytes), a single topic provides sufficient
/// throughput without per-message-type routing complexity.
pub fn consensus_topic() -> IdentTopic {
    IdentTopic::new("/n42/consensus/1")
}

/// Legacy gov5 HotStuff-2 topic for observation during cross-client migration.
///
/// gov5 derives the fork digest from the first four genesis-hash bytes and
/// appends its SSZ-snappy transport suffix.
pub fn gov5_h2_topic(genesis_hash: B256) -> IdentTopic {
    let fork_digest = hex::encode(&genesis_hash.as_slice()[..4]);
    IdentTopic::new(format!("/n42/{fork_digest}/hotstuff_consensus/ssz_snappy"))
}

/// GossipSub topic for block announcements (header-first dissemination).
///
/// Block headers are announced first; full bodies are fetched on-demand
/// via request-response after header validation.
pub fn block_announce_topic() -> IdentTopic {
    IdentTopic::new("/n42/blocks/1")
}

/// GossipSub topic for mobile verification receipt aggregates.
pub fn verification_receipts_topic() -> IdentTopic {
    IdentTopic::new("/n42/verification/1")
}

/// GossipSub topic for transaction pool (mempool) synchronization.
pub fn mempool_topic() -> IdentTopic {
    IdentTopic::new("/n42/mempool/1")
}

/// GossipSub topic for EIP-4844 blob sidecar propagation.
///
/// Leaders broadcast blob sidecars alongside block data. Followers
/// receive and insert into local DiskFileBlobStore. Best-effort:
/// sidecar availability is NOT required for block acceptance.
pub fn blob_sidecar_topic() -> IdentTopic {
    IdentTopic::new("/n42/blobs/1")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_topic_strings() {
        assert_eq!(
            consensus_topic().hash(),
            IdentTopic::new("/n42/consensus/1").hash()
        );
        assert_eq!(
            block_announce_topic().hash(),
            IdentTopic::new("/n42/blocks/1").hash()
        );
        assert_eq!(
            verification_receipts_topic().hash(),
            IdentTopic::new("/n42/verification/1").hash()
        );
        assert_eq!(
            mempool_topic().hash(),
            IdentTopic::new("/n42/mempool/1").hash()
        );
        assert_eq!(
            blob_sidecar_topic().hash(),
            IdentTopic::new("/n42/blobs/1").hash()
        );
        assert_eq!(
            gov5_h2_topic(B256::repeat_byte(0xab)).hash(),
            IdentTopic::new("/n42/abababab/hotstuff_consensus/ssz_snappy").hash()
        );

        // All five topics must be distinct.
        let all = [
            consensus_topic(),
            block_announce_topic(),
            verification_receipts_topic(),
            mempool_topic(),
            blob_sidecar_topic(),
        ];
        for i in 0..all.len() {
            for j in (i + 1)..all.len() {
                assert_ne!(
                    all[i].hash(),
                    all[j].hash(),
                    "topics at index {i} and {j} should differ"
                );
            }
        }
    }
}
