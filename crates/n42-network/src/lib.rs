pub mod block_direct;
pub mod codec;
pub mod consensus_direct;
pub mod dissemination;
pub mod error;
pub mod gossipsub;
pub mod h2_wire;
pub mod h2_v4;
pub mod mobile;
pub mod reconnection;
pub mod service;
pub mod state_sync;
pub mod transport;
pub mod tx_forward;

pub use block_direct::MAX_BLOCK_DIRECT_SIZE;
pub use consensus_direct::{ConsensusDirectCodec, ConsensusDirectRequest, ConsensusDirectResponse};
pub use error::NetworkError;
pub use mobile::{
    MSG_TYPE_CACHE_SYNC, MSG_TYPE_CACHE_SYNC_ZSTD, ShardedStarHub, ShardedStarHubConfig,
    ShardedStarHubHandle, StarHub, StarHubConfig, StarHubHandle,
};
pub use service::{NetworkCommand, NetworkEvent, NetworkHandle, NetworkService};
pub use state_sync::{
    BlockSyncRequest, BlockSyncResponse, MAX_BLOCKS_PER_SYNC_REQUEST, MAX_SYNC_MESSAGE_SIZE,
    SyncBlock, SyncPayload,
};
pub use transport::{
    MAX_GOSSIP_MESSAGE_SIZE, N42Behaviour, TransportConfig, build_swarm,
    build_swarm_with_validator_index, deterministic_validator_keypair,
    deterministic_validator_peer_id,
};

/// Largest serialized block-data broadcast that is certain to propagate.
///
/// A block leaves the leader on two channels — `block_direct` unicast and, as a
/// reliability fallback, GossipSub — so the narrower of the two governs. The
/// remaining headroom covers the GossipSub envelope (topic, signature, peer id)
/// and the request-response length prefix, none of which are counted in the
/// payload the producer sees.
///
/// Exceeding this is not a recoverable error anywhere in the stack: GossipSub
/// drops the publish with a warning and receivers `Reject` it, so an oversized
/// block simply never reaches the validators. They cannot vote on what they
/// never received, the view never reaches quorum, and because the same mempool
/// deterministically rebuilds the same oversized block, a restart does not
/// clear it. Block producers must treat this as a hard budget.
pub const MAX_BROADCAST_PAYLOAD_BYTES: usize = {
    let ceiling = if MAX_GOSSIP_MESSAGE_SIZE < MAX_BLOCK_DIRECT_SIZE {
        MAX_GOSSIP_MESSAGE_SIZE
    } else {
        MAX_BLOCK_DIRECT_SIZE
    };
    // 90% of the ceiling, less a fixed allowance for wire framing.
    ceiling / 10 * 9 - 64 * 1024
};

// Re-export libp2p types used by consumers.
pub use libp2p::PeerId;
pub use libp2p::identity as libp2p_identity;
