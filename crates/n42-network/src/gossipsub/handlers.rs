use libp2p::gossipsub::{self, Message, MessageId, TopicHash};
use n42_primitives::ConsensusMessage;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

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
/// Uses content-based hashing to deduplicate identical messages
/// from different peers. This prevents processing the same vote
/// or proposal twice when received via different gossip paths.
pub fn message_id_fn(message: &Message) -> MessageId {
    let mut hasher = DefaultHasher::new();
    message.data.hash(&mut hasher);
    // Include the topic to avoid cross-topic collisions
    message.topic.hash(&mut hasher);
    MessageId::from(hasher.finish().to_be_bytes().to_vec())
}

/// Validates a gossipsub message before forwarding to other peers.
///
/// Performs lightweight checks to prevent obviously invalid messages
/// from propagating through the gossip network:
/// - Rejects empty messages
/// - Verifies the message is decodable (well-formed)
/// - Rejects oversized messages (>1MB)
///
/// Full semantic validation (signature checks, view verification)
/// is done by the consensus engine after the message is delivered.
pub fn validate_message(
    topic: &TopicHash,
    data: &[u8],
    consensus_topic_hash: &TopicHash,
) -> gossipsub::MessageAcceptance {
    // Only validate consensus topic messages
    if topic != consensus_topic_hash {
        return gossipsub::MessageAcceptance::Accept;
    }

    // Reject empty or oversized messages
    if data.is_empty() || data.len() > 1_048_576 {
        return gossipsub::MessageAcceptance::Reject;
    }

    // Try to decode — reject malformed messages to prevent gossip amplification
    match decode_consensus_message(data) {
        Ok(_) => gossipsub::MessageAcceptance::Accept,
        Err(_) => gossipsub::MessageAcceptance::Reject,
    }
}
