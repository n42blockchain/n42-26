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
pub mod state_sync;
pub mod transport;
pub mod tx_forward;

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
    N42Behaviour, TransportConfig, build_interop_observer_swarm, build_interop_participant_swarm,
    build_swarm, build_swarm_with_validator_index, deterministic_validator_keypair,
    deterministic_validator_peer_id,
};

// Re-export libp2p types used by consumers.
pub use libp2p::PeerId;
pub use libp2p::identity as libp2p_identity;
