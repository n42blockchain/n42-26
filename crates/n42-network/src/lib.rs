pub mod block_direct;
pub mod codec;
pub mod consensus_direct;
pub mod dissemination;
pub mod error;
pub mod gossipsub;
pub mod mobile;
pub mod reconnection;
pub mod service;
pub mod state_sync;
pub mod transport;
pub mod tx_forward;

pub use consensus_direct::{ConsensusDirectCodec, ConsensusDirectRequest, ConsensusDirectResponse};
pub use error::NetworkError;
pub use mobile::{
    MSG_TYPE_CACHE_SYNC, MSG_TYPE_CACHE_SYNC_ZSTD, ShardedStarHub, ShardedStarHubConfig,
    ShardedStarHubHandle, StarHub, StarHubConfig, StarHubHandle,
};
pub use service::{NetworkCommand, NetworkEvent, NetworkHandle, NetworkService};
pub use state_sync::{BlockSyncRequest, BlockSyncResponse, MAX_BLOCKS_PER_SYNC_REQUEST, SyncBlock};
pub use transport::{
    N42Behaviour, TransportConfig, build_swarm, build_swarm_with_validator_index,
    deterministic_validator_keypair, deterministic_validator_peer_id,
};

// Re-export libp2p types used by consumers.
pub use libp2p::PeerId;
