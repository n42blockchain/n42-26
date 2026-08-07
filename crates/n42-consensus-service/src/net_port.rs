//! Consensus network port — a trait boundary over the libp2p `NetworkHandle`
//! that the consensus orchestrator drives. The in-process adapter
//! (`impl ConsensusNetwork for NetworkHandle`) lives node-side; this crate holds
//! only the trait. Caplin EL-seam refactor (stage 4 / 6c).

use n42_network::{
    BlockSyncRequest, BlockSyncResponse, NetworkError, NetworkHandle, PeerId, h2_v4::H2V4Envelope,
};
use n42_primitives::ConsensusMessage;
use std::collections::HashMap;

/// The network operations the consensus orchestrator needs. One in-process
/// adapter today (`NetworkHandle`, node-side). `async_trait` is used for the
/// awaited backpressure variants; the rest stay synchronous.
#[async_trait::async_trait]
pub trait ConsensusNetwork: Send + Sync {
    /// Broadcast a consensus message via GossipSub (priority channel).
    fn broadcast_consensus(&self, msg: ConsensusMessage) -> Result<(), NetworkError>;

    /// Broadcast a chain-bound gov5-compatible H2-v4 message.
    fn broadcast_h2_v4(&self, envelope: H2V4Envelope) -> Result<(), NetworkError>;

    /// Resolve a validator index to its connected `PeerId`, if known.
    fn validator_peer(&self, index: u32) -> Option<PeerId>;

    /// Send a consensus message directly to a peer (priority channel).
    fn send_direct(&self, peer: PeerId, msg: ConsensusMessage) -> Result<(), NetworkError>;

    /// All known validator-index → `PeerId` mappings.
    fn all_validator_peers(&self) -> Vec<(u32, PeerId)>;

    /// Forward a batch of RLP-encoded transactions to the leader peer.
    fn forward_tx_batch(&self, peer: PeerId, txs: Vec<Vec<u8>>) -> Result<(), NetworkError>;

    /// Send a block-sync request to a peer (standard channel).
    fn request_sync(&self, peer: PeerId, request: BlockSyncRequest) -> Result<(), NetworkError>;

    /// Fetch one gov5 block body whose hash was authenticated by H2-v4.
    fn request_gov5_block_by_hash(
        &self,
        peer: PeerId,
        block_hash: alloy_primitives::B256,
    ) -> Result<(), NetworkError>;

    /// Broadcast a blob sidecar (standard channel).
    fn broadcast_blob_sidecar(&self, data: Vec<u8>) -> Result<(), NetworkError>;

    /// Announce a built block to followers with backpressure (awaited).
    async fn announce_block_reliable(&self, data: Vec<u8>) -> Result<(), NetworkError>;

    /// Publish a locally built block on gov5's canonical block topic.
    async fn broadcast_gov5_block_reliable(&self, rlp: Vec<u8>) -> Result<(), NetworkError>;

    /// Send block data directly to a peer with backpressure (awaited).
    async fn send_block_direct_reliable(
        &self,
        peer: PeerId,
        data: Vec<u8>,
    ) -> Result<(), NetworkError>;

    /// Reply to a sync request with backpressure (awaited).
    async fn send_sync_response_reliable(
        &self,
        request_id: u64,
        response: BlockSyncResponse,
    ) -> Result<(), NetworkError>;

    /// Authenticate a validator peer after message verification (awaited).
    async fn authenticate_validator_peer_reliable(
        &self,
        peer: PeerId,
        validator_index: u32,
    ) -> Result<(), NetworkError>;

    /// Replace the expected validator-index → `PeerId` mappings (awaited).
    async fn replace_expected_validator_peers_reliable(
        &self,
        peers: HashMap<u32, PeerId>,
    ) -> Result<(), NetworkError>;

    /// Update the Rotor relay context (my index + validator count; awaited).
    async fn set_validator_context(&self, my_index: u32, validator_count: u32);
}

/// In-process adapter: libp2p `NetworkHandle` is a cheap channel facade and
/// reth-free, so it lives with the trait. Each method forwards 1:1 to the
/// inherent `NetworkHandle` method.
#[async_trait::async_trait]
impl ConsensusNetwork for NetworkHandle {
    fn broadcast_consensus(&self, msg: ConsensusMessage) -> Result<(), NetworkError> {
        NetworkHandle::broadcast_consensus(self, msg)
    }

    fn broadcast_h2_v4(&self, envelope: H2V4Envelope) -> Result<(), NetworkError> {
        NetworkHandle::broadcast_h2_v4(self, envelope)
    }

    fn validator_peer(&self, index: u32) -> Option<PeerId> {
        NetworkHandle::validator_peer(self, index)
    }

    fn send_direct(&self, peer: PeerId, msg: ConsensusMessage) -> Result<(), NetworkError> {
        NetworkHandle::send_direct(self, peer, msg)
    }

    fn all_validator_peers(&self) -> Vec<(u32, PeerId)> {
        NetworkHandle::all_validator_peers(self)
    }

    fn forward_tx_batch(&self, peer: PeerId, txs: Vec<Vec<u8>>) -> Result<(), NetworkError> {
        NetworkHandle::forward_tx_batch(self, peer, txs)
    }

    fn request_sync(&self, peer: PeerId, request: BlockSyncRequest) -> Result<(), NetworkError> {
        NetworkHandle::request_sync(self, peer, request)
    }

    fn request_gov5_block_by_hash(
        &self,
        peer: PeerId,
        block_hash: alloy_primitives::B256,
    ) -> Result<(), NetworkError> {
        NetworkHandle::request_gov5_block_by_hash(self, peer, block_hash)
    }

    fn broadcast_blob_sidecar(&self, data: Vec<u8>) -> Result<(), NetworkError> {
        NetworkHandle::broadcast_blob_sidecar(self, data)
    }

    async fn announce_block_reliable(&self, data: Vec<u8>) -> Result<(), NetworkError> {
        NetworkHandle::announce_block_reliable(self, data).await
    }

    async fn broadcast_gov5_block_reliable(&self, rlp: Vec<u8>) -> Result<(), NetworkError> {
        NetworkHandle::broadcast_gov5_block_reliable(self, rlp).await
    }

    async fn send_block_direct_reliable(
        &self,
        peer: PeerId,
        data: Vec<u8>,
    ) -> Result<(), NetworkError> {
        NetworkHandle::send_block_direct_reliable(self, peer, data).await
    }

    async fn send_sync_response_reliable(
        &self,
        request_id: u64,
        response: BlockSyncResponse,
    ) -> Result<(), NetworkError> {
        NetworkHandle::send_sync_response_reliable(self, request_id, response).await
    }

    async fn authenticate_validator_peer_reliable(
        &self,
        peer: PeerId,
        validator_index: u32,
    ) -> Result<(), NetworkError> {
        NetworkHandle::authenticate_validator_peer_reliable(self, peer, validator_index).await
    }

    async fn replace_expected_validator_peers_reliable(
        &self,
        peers: HashMap<u32, PeerId>,
    ) -> Result<(), NetworkError> {
        NetworkHandle::replace_expected_validator_peers_reliable(self, peers).await
    }

    async fn set_validator_context(&self, my_index: u32, validator_count: u32) {
        NetworkHandle::set_validator_context(self, my_index, validator_count).await
    }
}
