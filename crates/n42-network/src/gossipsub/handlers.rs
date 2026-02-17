use alloy_primitives::keccak256;
use libp2p::gossipsub::{self, Message, MessageId, TopicHash};
use n42_primitives::ConsensusMessage;

use crate::error::NetworkError;

/// Encodes a consensus message to bytes using bincode for GossipSub publishing.
pub fn encode_consensus_message(msg: &ConsensusMessage) -> Result<Vec<u8>, NetworkError> {
    bincode::serialize(msg).map_err(|e| NetworkError::Codec(e.to_string()))
}

/// Decodes a consensus message from GossipSub bytes.
pub fn decode_consensus_message(data: &[u8]) -> Result<ConsensusMessage, NetworkError> {
    bincode::deserialize(data).map_err(|e| NetworkError::Codec(e.to_string()))
}

/// Generates a unique message ID for GossipSub deduplication.
///
/// Uses keccak256 (cryptographic hash) to deduplicate identical messages
/// from different peers. This prevents processing the same vote
/// or proposal twice when received via different gossip paths.
/// A cryptographic hash prevents intentional collision attacks.
pub fn message_id_fn(message: &Message) -> MessageId {
    let mut data = Vec::with_capacity(message.data.len() + message.topic.as_str().len());
    data.extend_from_slice(&message.data);
    // Include the topic to avoid cross-topic collisions
    data.extend_from_slice(message.topic.as_str().as_bytes());
    let hash = keccak256(&data);
    MessageId::from(hash.as_slice().to_vec())
}

/// Validates a gossipsub message before forwarding to other peers.
///
/// Performs lightweight checks to prevent obviously invalid messages
/// from propagating through the gossip network:
/// - Rejects empty messages
/// - Verifies the message is decodable (well-formed) for consensus topic
/// - Rejects oversized messages (per-topic size limits)
///
/// Full semantic validation (signature checks, view verification)
/// is done by the consensus engine after the message is delivered.
pub fn validate_message(
    topic: &TopicHash,
    data: &[u8],
    consensus_topic_hash: &TopicHash,
    block_topic_hash: &TopicHash,
) -> gossipsub::MessageAcceptance {
    if data.is_empty() {
        return gossipsub::MessageAcceptance::Reject;
    }

    // Per-topic size limits.
    // Block data can reach several MB; consensus messages are small (~130-500 bytes).
    // Reference: Ethereum uses 10MB for beacon blocks.
    let max_size = if topic == block_topic_hash {
        4 * 1024 * 1024 // 4MB for block data
    } else {
        1024 * 1024 // 1MB for consensus messages and other topics
    };

    if data.len() > max_size {
        return gossipsub::MessageAcceptance::Reject;
    }

    // For consensus topic, also verify message is decodable
    if topic == consensus_topic_hash {
        match decode_consensus_message(data) {
            Ok(_) => gossipsub::MessageAcceptance::Accept,
            Err(_) => gossipsub::MessageAcceptance::Reject,
        }
    } else {
        gossipsub::MessageAcceptance::Accept
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::B256;
    use n42_primitives::{BlsSecretKey, ConsensusMessage, Vote};
    use crate::gossipsub::topics::{block_announce_topic, consensus_topic};

    /// Helper: create a dummy Vote wrapped in ConsensusMessage for testing.
    fn dummy_consensus_vote() -> ConsensusMessage {
        let sk = BlsSecretKey::random().unwrap();
        let msg = b"test vote message";
        let sig = sk.sign(msg);
        let vote = Vote {
            view: 1,
            block_hash: B256::repeat_byte(0xAA),
            voter: 0,
            signature: sig,
        };
        ConsensusMessage::Vote(vote)
    }

    #[test]
    fn test_encode_decode_roundtrip() {
        let consensus_msg = dummy_consensus_vote();

        let encoded = encode_consensus_message(&consensus_msg)
            .expect("encoding should succeed");
        assert!(!encoded.is_empty(), "encoded bytes should not be empty");

        let decoded = decode_consensus_message(&encoded)
            .expect("decoding should succeed");

        // Verify fields match by inspecting the decoded Vote variant.
        match (&consensus_msg, &decoded) {
            (ConsensusMessage::Vote(original), ConsensusMessage::Vote(recovered)) => {
                assert_eq!(original.view, recovered.view, "view should match");
                assert_eq!(original.block_hash, recovered.block_hash, "block_hash should match");
                assert_eq!(original.voter, recovered.voter, "voter should match");
                assert_eq!(original.signature, recovered.signature, "signature should match");
            }
            _ => panic!("decoded message should be a Vote variant"),
        }
    }

    #[test]
    fn test_validate_message_accept() {
        let consensus_msg = dummy_consensus_vote();
        let encoded = encode_consensus_message(&consensus_msg).unwrap();

        let topic = consensus_topic();
        let topic_hash = topic.hash();
        let block_hash = block_announce_topic().hash();

        let result = validate_message(&topic_hash, &encoded, &topic_hash, &block_hash);
        assert!(
            matches!(result, gossipsub::MessageAcceptance::Accept),
            "valid encoded consensus message should be accepted"
        );
    }

    #[test]
    fn test_validate_message_reject_empty() {
        let topic = consensus_topic();
        let topic_hash = topic.hash();
        let block_hash = block_announce_topic().hash();

        let result = validate_message(&topic_hash, &[], &topic_hash, &block_hash);
        assert!(
            matches!(result, gossipsub::MessageAcceptance::Reject),
            "empty data should be rejected"
        );
    }

    #[test]
    fn test_validate_message_reject_oversized() {
        let topic = consensus_topic();
        let topic_hash = topic.hash();
        let block_hash = block_announce_topic().hash();

        // Create data that exceeds 1MB (consensus topic limit).
        let oversized = vec![0u8; 1_048_576 + 1];
        let result = validate_message(&topic_hash, &oversized, &topic_hash, &block_hash);
        assert!(
            matches!(result, gossipsub::MessageAcceptance::Reject),
            "oversized data (>1MB) should be rejected for consensus topic"
        );
    }

    #[test]
    fn test_validate_message_block_topic_large_accepted() {
        let consensus_hash = consensus_topic().hash();
        let block_topic = block_announce_topic();
        let block_hash = block_topic.hash();

        // 2MB data on block topic should be accepted (limit is 4MB).
        let large_data = vec![0u8; 2 * 1024 * 1024];
        let result = validate_message(&block_hash, &large_data, &consensus_hash, &block_hash);
        assert!(
            matches!(result, gossipsub::MessageAcceptance::Accept),
            "2MB data should be accepted on block topic (4MB limit)"
        );
    }

    #[test]
    fn test_validate_message_block_topic_oversized_rejected() {
        let consensus_hash = consensus_topic().hash();
        let block_topic = block_announce_topic();
        let block_hash = block_topic.hash();

        // Data exceeding 4MB on block topic should be rejected.
        let oversized = vec![0u8; 4 * 1024 * 1024 + 1];
        let result = validate_message(&block_hash, &oversized, &consensus_hash, &block_hash);
        assert!(
            matches!(result, gossipsub::MessageAcceptance::Reject),
            "oversized data (>4MB) should be rejected for block topic"
        );
    }

    #[test]
    fn test_validate_message_reject_malformed() {
        let topic = consensus_topic();
        let topic_hash = topic.hash();
        let block_hash = block_announce_topic().hash();

        // Random bytes that do not form a valid ConsensusMessage.
        let garbage = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02, 0x03, 0x04];
        let result = validate_message(&topic_hash, &garbage, &topic_hash, &block_hash);
        assert!(
            matches!(result, gossipsub::MessageAcceptance::Reject),
            "malformed data should be rejected"
        );
    }

    #[test]
    fn test_validate_message_non_consensus_topic_valid() {
        let consensus_topic_hash = consensus_topic().hash();
        let block_hash = block_announce_topic().hash();
        let other_topic_hash = libp2p::gossipsub::IdentTopic::new("other").hash();

        // Non-empty data on a non-consensus topic should be accepted.
        let result = validate_message(&other_topic_hash, &[1, 2, 3], &consensus_topic_hash, &block_hash);
        assert!(
            matches!(result, gossipsub::MessageAcceptance::Accept),
            "non-consensus topic messages with data should be accepted"
        );
    }

    #[test]
    fn test_validate_message_non_consensus_topic_empty_rejected() {
        let consensus_topic_hash = consensus_topic().hash();
        let block_hash = block_announce_topic().hash();
        let other_topic_hash = libp2p::gossipsub::IdentTopic::new("other").hash();

        // Empty data should be rejected for ALL topics.
        let result = validate_message(&other_topic_hash, &[], &consensus_topic_hash, &block_hash);
        assert!(
            matches!(result, gossipsub::MessageAcceptance::Reject),
            "empty messages should be rejected on all topics"
        );
    }
}
