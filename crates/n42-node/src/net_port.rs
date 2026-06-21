//! Consensus network port — a trait boundary over the libp2p `NetworkHandle`
//! that the consensus orchestrator drives, decoupling consensus from the
//! concrete network handle (a Caplin-style sentinel-client seam; stage 4 of the
//! EL-seam refactor). `NetworkHandle` is itself a cheap channel facade, so the
//! port is thin: it exposes exactly the methods the `ConsensusOrchestrator`
//! (and its spawned payload-broadcast task) call, no more. See
//! `docs/task-caplin-cl-module.md`.

use n42_network::{BlockSyncRequest, BlockSyncResponse, NetworkError, NetworkHandle, PeerId};
use n42_primitives::ConsensusMessage;
use std::collections::HashMap;

/// The network operations the consensus orchestrator needs. One in-process
/// adapter today (`NetworkHandle`); the port lets the orchestrator move into a
/// service crate without depending on libp2p / `n42-network` internals.
///
/// `async_trait` is used only for `announce_block_reliable` (the sole awaited
/// call on the consensus path, in the payload-broadcast task); the remaining
/// methods stay synchronous.
#[async_trait::async_trait]
pub trait ConsensusNetwork: Send + Sync {
    /// Broadcast a consensus message via GossipSub (priority channel).
    fn broadcast_consensus(&self, msg: ConsensusMessage) -> Result<(), NetworkError>;

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

    /// Broadcast a blob sidecar (standard channel).
    fn broadcast_blob_sidecar(&self, data: Vec<u8>) -> Result<(), NetworkError>;

    /// Announce a built block to followers with backpressure (awaited).
    async fn announce_block_reliable(&self, data: Vec<u8>) -> Result<(), NetworkError>;

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

#[async_trait::async_trait]
impl ConsensusNetwork for NetworkHandle {
    fn broadcast_consensus(&self, msg: ConsensusMessage) -> Result<(), NetworkError> {
        NetworkHandle::broadcast_consensus(self, msg)
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

    fn broadcast_blob_sidecar(&self, data: Vec<u8>) -> Result<(), NetworkError> {
        NetworkHandle::broadcast_blob_sidecar(self, data)
    }

    async fn announce_block_reliable(&self, data: Vec<u8>) -> Result<(), NetworkError> {
        NetworkHandle::announce_block_reliable(self, data).await
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

#[cfg(test)]
mod tests {
    use super::*;
    use n42_network::NetworkCommand;
    use n42_primitives::{BlsSecretKey, Vote};
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn broadcast_consensus_through_port_emits_command() {
        let (cmd_tx, _cmd_rx) = mpsc::channel(8);
        let (ptx, mut priority_rx) = mpsc::channel(8);
        let handle = NetworkHandle::new(cmd_tx, ptx);

        let sk = BlsSecretKey::key_gen(&[0x21; 32]).expect("deterministic key");
        let vote = Vote {
            view: 1,
            block_hash: alloy_primitives::B256::repeat_byte(0xCC),
            voter: 0,
            signature: sk.sign(b"port-test"),
        };

        // Drive the call through the trait object, not the inherent method.
        let net: &dyn ConsensusNetwork = &handle;
        net.broadcast_consensus(ConsensusMessage::Vote(vote))
            .expect("broadcast_consensus via port should succeed");

        let cmd = priority_rx
            .try_recv()
            .expect("priority channel should carry the broadcast");
        assert!(
            matches!(cmd, NetworkCommand::BroadcastConsensus(_)),
            "should be a BroadcastConsensus command"
        );
    }
}
