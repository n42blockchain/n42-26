use alloy_primitives::keccak256;
use libp2p::gossipsub::{self, Message, MessageId, TopicHash};
use n42_primitives::{ConsensusMessage, VersionedMessage, CONSENSUS_PROTOCOL_VERSION};

use crate::error::NetworkError;

/// Maximum size for a consensus bincode message (4MB).
const MAX_CONSENSUS_MSG_SIZE: usize = 4 * 1024 * 1024;

/// Encodes a consensus message to versioned bytes for GossipSub publishing.
///
/// Wraps in a `VersionedMessage` envelope so receivers can detect version
/// mismatches during rolling upgrades.
pub fn encode_consensus_message(msg: &ConsensusMessage) -> Result<Vec<u8>, NetworkError> {
    let versioned = VersionedMessage {
        version: CONSENSUS_PROTOCOL_VERSION,
        message: msg.clone(),
    };
    bincode::serialize(&versioned).map_err(|e| NetworkError::Codec(e.to_string()))
}

/// Decodes a versioned consensus message from GossipSub bytes.
///
/// Rejects payloads exceeding 4MB to prevent OOM. Falls back to bare
/// `ConsensusMessage` deserialization for backward compatibility with
/// pre-versioned nodes.
pub fn decode_consensus_message(data: &[u8]) -> Result<ConsensusMessage, NetworkError> {
    if data.len() > MAX_CONSENSUS_MSG_SIZE {
        return Err(NetworkError::Codec(format!(
            "consensus message too large: {} bytes (max {})",
            data.len(),
            MAX_CONSENSUS_MSG_SIZE,
        )));
    }

    match bincode::deserialize::<VersionedMessage>(data) {
        Ok(versioned) => {
            if versioned.version != CONSENSUS_PROTOCOL_VERSION {
                return Err(NetworkError::Codec(format!(
                    "unsupported protocol version: got {}, expected {}",
                    versioned.version, CONSENSUS_PROTOCOL_VERSION,
                )));
            }
            Ok(versioned.message)
        }
        Err(_) => {
            // Fallback for pre-versioned nodes during rolling upgrades.
            bincode::deserialize::<ConsensusMessage>(data)
                .map_err(|e| NetworkError::Codec(e.to_string()))
        }
    }
}

/// Generates a unique message ID for GossipSub deduplication.
///
/// Hashes message data + topic to prevent cross-topic collisions and
/// resist intentional ID collision attacks.
pub fn message_id_fn(message: &Message) -> MessageId {
    let mut data = Vec::with_capacity(message.data.len() + message.topic.as_str().len());
    data.extend_from_slice(&message.data);
    data.extend_from_slice(message.topic.as_str().as_bytes());
    MessageId::from(keccak256(&data).as_slice().to_vec())
}

/// Validates a gossipsub message before forwarding.
///
/// Performs lightweight structural checks:
/// - Rejects empty messages
/// - Enforces per-topic size limits
/// - For the consensus topic, verifies the message is decodable
///
/// Full semantic validation is done by the consensus engine.
pub fn validate_message(
    topic: &TopicHash,
    data: &[u8],
    consensus_topic_hash: &TopicHash,
    block_topic_hash: &TopicHash,
    mempool_topic_hash: &TopicHash,
    blob_sidecar_topic_hash: &TopicHash,
) -> gossipsub::MessageAcceptance {
    if data.is_empty() {
        return gossipsub::MessageAcceptance::Reject;
    }

    // Per-topic size limits.
    // Block data can reach several MB; consensus messages are small (~130-500 bytes).
    // Mempool transactions capped at 128KB; blob sidecars at 1MB.
    let max_size = if topic == block_topic_hash {
        4 * 1024 * 1024 // 4MB for block data
    } else if topic == blob_sidecar_topic_hash {
        1024 * 1024 // 1MB for blob sidecars
    } else if topic == mempool_topic_hash {
        128 * 1024 // 128KB for individual transactions
    } else {
        1024 * 1024 // 1MB for consensus and other topics
    };

    if data.len() > max_size {
        return gossipsub::MessageAcceptance::Reject;
    }

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
    use crate::gossipsub::topics::{blob_sidecar_topic, block_announce_topic, consensus_topic, mempool_topic};

    fn dummy_consensus_vote() -> ConsensusMessage {
        let sk = BlsSecretKey::random().unwrap();
        let sig = sk.sign(b"test vote message");
        ConsensusMessage::Vote(Vote {
            view: 1,
            block_hash: B256::repeat_byte(0xAA),
            voter: 0,
            signature: sig,
        })
    }

    fn mem_hash() -> TopicHash {
        mempool_topic().hash()
    }

    fn blob_hash() -> TopicHash {
        blob_sidecar_topic().hash()
    }

    #[test]
    fn test_encode_decode_roundtrip() {
        let msg = dummy_consensus_vote();
        let encoded = encode_consensus_message(&msg).expect("encoding should succeed");
        assert!(!encoded.is_empty());

        let decoded = decode_consensus_message(&encoded).expect("decoding should succeed");
        match (&msg, &decoded) {
            (ConsensusMessage::Vote(original), ConsensusMessage::Vote(recovered)) => {
                assert_eq!(original.view, recovered.view);
                assert_eq!(original.block_hash, recovered.block_hash);
                assert_eq!(original.voter, recovered.voter);
                assert_eq!(original.signature, recovered.signature);
            }
            _ => panic!("decoded message should be a Vote variant"),
        }
    }

    #[test]
    fn test_validate_message_accept() {
        let msg = dummy_consensus_vote();
        let encoded = encode_consensus_message(&msg).unwrap();
        let topic_hash = consensus_topic().hash();
        let block_hash = block_announce_topic().hash();

        let result = validate_message(&topic_hash, &encoded, &topic_hash, &block_hash, &mem_hash(), &blob_hash());
        assert!(matches!(result, gossipsub::MessageAcceptance::Accept));
    }

    #[test]
    fn test_validate_message_reject_empty() {
        let topic_hash = consensus_topic().hash();
        let block_hash = block_announce_topic().hash();

        let result = validate_message(&topic_hash, &[], &topic_hash, &block_hash, &mem_hash(), &blob_hash());
        assert!(matches!(result, gossipsub::MessageAcceptance::Reject));
    }

    #[test]
    fn test_validate_message_reject_oversized() {
        let topic_hash = consensus_topic().hash();
        let block_hash = block_announce_topic().hash();
        let oversized = vec![0u8; 1_048_576 + 1];

        let result = validate_message(&topic_hash, &oversized, &topic_hash, &block_hash, &mem_hash(), &blob_hash());
        assert!(matches!(result, gossipsub::MessageAcceptance::Reject));
    }

    #[test]
    fn test_validate_message_block_topic_large_accepted() {
        let consensus_hash = consensus_topic().hash();
        let block_hash = block_announce_topic().hash();
        let large_data = vec![0u8; 2 * 1024 * 1024];

        let result = validate_message(&block_hash, &large_data, &consensus_hash, &block_hash, &mem_hash(), &blob_hash());
        assert!(matches!(result, gossipsub::MessageAcceptance::Accept));
    }

    #[test]
    fn test_validate_message_block_topic_oversized_rejected() {
        let consensus_hash = consensus_topic().hash();
        let block_hash = block_announce_topic().hash();
        let oversized = vec![0u8; 4 * 1024 * 1024 + 1];

        let result = validate_message(&block_hash, &oversized, &consensus_hash, &block_hash, &mem_hash(), &blob_hash());
        assert!(matches!(result, gossipsub::MessageAcceptance::Reject));
    }

    #[test]
    fn test_validate_message_reject_malformed() {
        let topic_hash = consensus_topic().hash();
        let block_hash = block_announce_topic().hash();
        let garbage = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02, 0x03, 0x04];

        let result = validate_message(&topic_hash, &garbage, &topic_hash, &block_hash, &mem_hash(), &blob_hash());
        assert!(matches!(result, gossipsub::MessageAcceptance::Reject));
    }

    #[test]
    fn test_validate_message_non_consensus_topic_valid() {
        let consensus_topic_hash = consensus_topic().hash();
        let block_hash = block_announce_topic().hash();
        let other = libp2p::gossipsub::IdentTopic::new("other").hash();

        let result = validate_message(&other, &[1, 2, 3], &consensus_topic_hash, &block_hash, &mem_hash(), &blob_hash());
        assert!(matches!(result, gossipsub::MessageAcceptance::Accept));
    }

    #[test]
    fn test_validate_message_non_consensus_topic_empty_rejected() {
        let consensus_topic_hash = consensus_topic().hash();
        let block_hash = block_announce_topic().hash();
        let other = libp2p::gossipsub::IdentTopic::new("other").hash();

        let result = validate_message(&other, &[], &consensus_topic_hash, &block_hash, &mem_hash(), &blob_hash());
        assert!(matches!(result, gossipsub::MessageAcceptance::Reject));
    }

    #[test]
    fn test_validate_message_mempool_size_limit() {
        let consensus_hash = consensus_topic().hash();
        let block_hash = block_announce_topic().hash();
        let mp_hash = mem_hash();

        let data = vec![0u8; 64 * 1024];
        let result = validate_message(&mp_hash, &data, &consensus_hash, &block_hash, &mp_hash, &blob_hash());
        assert!(matches!(result, gossipsub::MessageAcceptance::Accept));

        let oversized = vec![0u8; 128 * 1024 + 1];
        let result = validate_message(&mp_hash, &oversized, &consensus_hash, &block_hash, &mp_hash, &blob_hash());
        assert!(matches!(result, gossipsub::MessageAcceptance::Reject));
    }
}
