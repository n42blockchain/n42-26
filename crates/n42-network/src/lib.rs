pub mod block_direct;
pub mod codec;
pub mod compact_receipts;
pub mod consensus_direct;
pub mod dissemination;
pub mod error;
pub mod finalized_range;
pub mod gossipsub;
pub mod gov5_block;
pub mod gov5_rpc;
pub mod h2_bridge;
pub mod h2_v4;
pub mod h2_wire;
pub mod mobile;
pub mod reconnection;
pub mod service;
pub mod snappy_pool;
pub mod state_sync;
pub mod transport;
pub mod tx_forward;

pub use block_direct::MAX_BLOCK_DIRECT_SIZE;
pub use compact_receipts::{
    CompactReceiptError, MAX_RECEIPTS_PER_BLOCK, decode_compact_receipts, gov5_native_receipts_root,
};
pub use consensus_direct::{ConsensusDirectCodec, ConsensusDirectRequest, ConsensusDirectResponse};
pub use error::NetworkError;
pub use finalized_range::{
    FinalizedRangeError, FinalizedRangeVerification, MAX_FINALIZED_RANGE_BLOCKS,
    MAX_MATERIALIZED_FINALIZED_RANGE_BYTES, VerifiedFinalizedRange, VerifiedFinalizedRangeEntry,
    decode_finalized_range_stream, verify_finalized_range_stream,
};
pub use gov5_block::{
    Gov5BlockError, Gov5GossipBlock, decode_gov5_block_rlp, encode_gov5_block_rlp,
    gov5_header_view, normalize_execution_payload_for_gov5_h2,
};
pub use gov5_rpc::{
    GOV5_BODIES_BY_RANGE_PROTOCOL, Gov5BodiesByRangeRequest, Gov5CanonicalBlockReader,
    MAX_GOV5_RANGE_BLOCK_SIZE, MAX_GOV5_RANGE_BLOCKS,
};
pub use h2_bridge::{
    H2BridgeError, consensus_from_h2_v4, consensus_to_h2_v4, quorum_certificate_from_h2,
};
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
    MAX_BLOB_GOSSIP_MESSAGE_SIZE, MAX_GOSSIP_MESSAGE_SIZE, N42Behaviour, TransportConfig,
    build_interop_observer_swarm, build_interop_participant_swarm, build_swarm,
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
/// On the paths that have no fallback, exceeding the ceiling itself is not
/// recoverable: GossipSub drops the publish with a warning and receivers
/// `Reject` it, so the block never reaches the validators. They cannot vote on
/// what they never received, the view never reaches quorum, and because the same
/// mempool deterministically rebuilds the same oversized block, a restart does
/// not clear it.
///
/// This constant sits *below* that ceiling and marks the point where a block is
/// close enough to it to be worth reporting — it is a warning line, not the
/// failure point. The margin is deliberately small because measured traffic
/// already runs near the ceiling: `docs/devlog-78` records direct-push encoded
/// sizes of 6,987 KiB p50 and 7,374 KiB p95 against the 8,192 KiB ceiling, on
/// pure transfers, which is the load zstd compresses best. Anything crossing
/// this line is inside the last few percent of the budget.
///
/// The reserve covers what the producer never sees in its own payload: the
/// GossipSub envelope (topic, signature, peer id) and the request-response
/// length prefix. It has not been measured — 256 KiB is an estimate, and if the
/// real envelope is larger this line sits closer to the ceiling than intended.
pub const MAX_BROADCAST_PAYLOAD_BYTES: usize = {
    let ceiling = if MAX_GOSSIP_MESSAGE_SIZE < MAX_BLOCK_DIRECT_SIZE {
        MAX_GOSSIP_MESSAGE_SIZE
    } else {
        MAX_BLOCK_DIRECT_SIZE
    };
    ceiling - 256 * 1024
};

// Re-export libp2p types used by consumers.
pub use libp2p::PeerId;
pub use libp2p::identity as libp2p_identity;
