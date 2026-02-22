pub mod dissemination;
pub mod error;
pub mod gossipsub;
pub mod mobile;
pub mod reconnection;
pub mod service;
pub mod state_sync;
pub mod transport;

pub use error::NetworkError;
pub use mobile::{
    ShardedStarHub, ShardedStarHubConfig, ShardedStarHubHandle, StarHub, StarHubConfig,
    StarHubHandle, MSG_TYPE_CACHE_SYNC_ZSTD,
};
pub use service::{NetworkCommand, NetworkEvent, NetworkHandle, NetworkService};
pub use state_sync::{BlockSyncRequest, BlockSyncResponse, SyncBlock};
pub use transport::{
    build_swarm, build_swarm_with_validator_index, N42Behaviour, TransportConfig,
};

// Re-export libp2p types used by consumers.
pub use libp2p::PeerId;
