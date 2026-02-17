pub mod dissemination;
pub mod error;
pub mod gossipsub;
pub mod mobile;
pub mod service;
pub mod state_sync;
pub mod transport;

pub use error::NetworkError;
pub use mobile::{StarHub, StarHubConfig, StarHubHandle};
pub use service::{NetworkCommand, NetworkEvent, NetworkHandle, NetworkService};
pub use state_sync::{BlockSyncRequest, BlockSyncResponse, SyncBlock};
pub use transport::{N42Behaviour, TransportConfig, build_swarm};

// Re-export libp2p types used by consumers (e.g., PeerId in sync events)
pub use libp2p::PeerId;
