//! Consensus network port — a trait boundary over the libp2p `NetworkHandle`
//! that the consensus orchestrator drives. The in-process adapter
//! (`impl ConsensusNetwork for NetworkHandle`) lives node-side; this crate holds
//! only the trait. Caplin EL-seam refactor (stage 4 / 6c).

use n42_network::{
    BlockSyncRequest, BlockSyncResponse, NetworkError, NetworkHandle, PeerId, h2_v4::H2V4Envelope,
};
use n42_primitives::ConsensusMessage;
use std::collections::HashMap;
use std::sync::Arc;

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

    /// Publish RLP-encoded (network form) transactions on gov5's
    /// `transaction_v2` topic; H2-v4 participant mode only.
    fn publish_gov5_transactions(&self, txs: Vec<Vec<u8>>) -> Result<(), NetworkError>;

    /// Send a block-sync request to a peer (standard channel).
    fn request_sync(&self, peer: PeerId, request: BlockSyncRequest) -> Result<(), NetworkError>;

    /// Fetch one gov5 block body whose hash was authenticated by H2-v4.
    fn request_gov5_block_by_hash(
        &self,
        peer: PeerId,
        block_hash: alloy_primitives::B256,
    ) -> Result<(), NetworkError>;

    /// Request `count` contiguous gov5 blocks from `start` over
    /// `/rpc/bodies_by_range/1` for the range catch-up client. The answer
    /// arrives as `NetworkEvent::Gov5RangeResponse`.
    fn request_gov5_range(&self, peer: PeerId, start: u64, count: u64) -> Result<(), NetworkError>;

    /// Ask a peer for a CommitQC anchoring `(number, hash)` over
    /// `/rpc/commit_qc/1`; the answer arrives as
    /// `NetworkEvent::Gov5CommitQcResponse`.
    fn request_gov5_commit_qc(
        &self,
        _peer: PeerId,
        _number: u64,
        _hash: alloy_primitives::B256,
    ) -> Result<(), NetworkError> {
        Err(NetworkError::ChannelClosed)
    }

    /// Hand the network the CommitQC (gov5 QC wire) of the block just
    /// committed; `commit_qc/1` serves it as the persisted committed QC.
    fn note_committed_qc(&self, _wire: Vec<u8>) {}

    /// Hand the network a committed block imported without its own CommitQC
    /// (range catch-up below a commit anchor), for the served committed head.
    fn note_committed_block(&self, _hash: alloy_primitives::B256) {}

    /// Retire a hash fetch whose body arrived through another authenticated
    /// delivery path before the request-response stream completed.
    async fn cancel_gov5_block_fetch_reliable(
        &self,
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
        data: Arc<Vec<u8>>,
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

    fn publish_gov5_transactions(&self, txs: Vec<Vec<u8>>) -> Result<(), NetworkError> {
        NetworkHandle::publish_gov5_transactions(self, txs)
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

    fn request_gov5_range(&self, peer: PeerId, start: u64, count: u64) -> Result<(), NetworkError> {
        NetworkHandle::request_gov5_range(self, peer, start, count)
    }

    fn request_gov5_commit_qc(
        &self,
        peer: PeerId,
        number: u64,
        hash: alloy_primitives::B256,
    ) -> Result<(), NetworkError> {
        NetworkHandle::request_gov5_commit_qc(self, peer, number, hash)
    }

    fn note_committed_qc(&self, wire: Vec<u8>) {
        let _ = NetworkHandle::note_committed_qc(self, wire);
    }

    fn note_committed_block(&self, hash: alloy_primitives::B256) {
        let _ = NetworkHandle::note_committed_block(self, hash);
    }

    async fn cancel_gov5_block_fetch_reliable(
        &self,
        block_hash: alloy_primitives::B256,
    ) -> Result<(), NetworkError> {
        NetworkHandle::cancel_gov5_block_fetch_reliable(self, block_hash).await
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
        data: Arc<Vec<u8>>,
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

/// The network of a Profile-1 follower (AM6: a verifying non-validator). It
/// receives blocks and consensus messages, fetches blocks by hash and by
/// range, answers range and status requests and forwards user transactions,
/// but publishes nothing of consensus: no vote, commit vote, timeout,
/// NewView, proposal, QC or Decide, and no block or blob announcement. Every
/// such call is swallowed here and counted in
/// `n42_follower_publish_refused_total{kind}`, whatever the caller above it
/// does; it returns `Ok` so a caller's error path never retries it.
pub struct FollowerNetwork {
    inner: Arc<dyn ConsensusNetwork>,
}

impl FollowerNetwork {
    pub fn new(inner: Arc<dyn ConsensusNetwork>) -> Self {
        Self { inner }
    }

    fn refuse(kind: &'static str) -> Result<(), NetworkError> {
        metrics::counter!("n42_follower_publish_refused_total", "kind" => kind).increment(1);
        tracing::debug!(target: "n42::follower", kind, "follower: publication refused");
        Ok(())
    }
}

/// The name of a consensus message for the follower's refusal counter.
pub fn consensus_message_kind(msg: &ConsensusMessage) -> &'static str {
    match msg {
        ConsensusMessage::Proposal(_) => "proposal",
        ConsensusMessage::Vote(_) => "vote",
        ConsensusMessage::CommitVote(_) => "commit_vote",
        ConsensusMessage::PrepareQC(_) => "prepare_qc",
        ConsensusMessage::Timeout(_) => "timeout",
        ConsensusMessage::NewView(_) => "new_view",
        ConsensusMessage::Decide(_) => "decide",
    }
}

#[async_trait::async_trait]
impl ConsensusNetwork for FollowerNetwork {
    fn broadcast_consensus(&self, msg: ConsensusMessage) -> Result<(), NetworkError> {
        Self::refuse(consensus_message_kind(&msg))
    }

    fn broadcast_h2_v4(&self, envelope: H2V4Envelope) -> Result<(), NetworkError> {
        Self::refuse(envelope.message.kind().as_str())
    }

    fn validator_peer(&self, index: u32) -> Option<PeerId> {
        self.inner.validator_peer(index)
    }

    fn send_direct(&self, _peer: PeerId, msg: ConsensusMessage) -> Result<(), NetworkError> {
        Self::refuse(consensus_message_kind(&msg))
    }

    fn all_validator_peers(&self) -> Vec<(u32, PeerId)> {
        self.inner.all_validator_peers()
    }

    fn forward_tx_batch(&self, peer: PeerId, txs: Vec<Vec<u8>>) -> Result<(), NetworkError> {
        self.inner.forward_tx_batch(peer, txs)
    }

    /// A follower seals nothing but still passes its RPC submissions on, so
    /// the fleet's leaders seal them.
    fn publish_gov5_transactions(&self, txs: Vec<Vec<u8>>) -> Result<(), NetworkError> {
        self.inner.publish_gov5_transactions(txs)
    }

    fn request_sync(&self, peer: PeerId, request: BlockSyncRequest) -> Result<(), NetworkError> {
        self.inner.request_sync(peer, request)
    }

    fn request_gov5_block_by_hash(
        &self,
        peer: PeerId,
        block_hash: alloy_primitives::B256,
    ) -> Result<(), NetworkError> {
        self.inner.request_gov5_block_by_hash(peer, block_hash)
    }

    fn request_gov5_range(&self, peer: PeerId, start: u64, count: u64) -> Result<(), NetworkError> {
        self.inner.request_gov5_range(peer, start, count)
    }

    fn request_gov5_commit_qc(
        &self,
        peer: PeerId,
        number: u64,
        hash: alloy_primitives::B256,
    ) -> Result<(), NetworkError> {
        self.inner.request_gov5_commit_qc(peer, number, hash)
    }

    fn note_committed_qc(&self, wire: Vec<u8>) {
        self.inner.note_committed_qc(wire);
    }

    fn note_committed_block(&self, hash: alloy_primitives::B256) {
        self.inner.note_committed_block(hash);
    }

    async fn cancel_gov5_block_fetch_reliable(
        &self,
        block_hash: alloy_primitives::B256,
    ) -> Result<(), NetworkError> {
        self.inner.cancel_gov5_block_fetch_reliable(block_hash).await
    }

    fn broadcast_blob_sidecar(&self, _data: Vec<u8>) -> Result<(), NetworkError> {
        Self::refuse("blob_sidecar")
    }

    async fn announce_block_reliable(&self, _data: Vec<u8>) -> Result<(), NetworkError> {
        Self::refuse("block_announce")
    }

    async fn broadcast_gov5_block_reliable(&self, _rlp: Vec<u8>) -> Result<(), NetworkError> {
        Self::refuse("gov5_block")
    }

    async fn send_block_direct_reliable(
        &self,
        _peer: PeerId,
        _data: Arc<Vec<u8>>,
    ) -> Result<(), NetworkError> {
        Self::refuse("block_direct")
    }

    async fn send_sync_response_reliable(
        &self,
        request_id: u64,
        response: BlockSyncResponse,
    ) -> Result<(), NetworkError> {
        self.inner.send_sync_response_reliable(request_id, response).await
    }

    async fn authenticate_validator_peer_reliable(
        &self,
        peer: PeerId,
        validator_index: u32,
    ) -> Result<(), NetworkError> {
        self.inner.authenticate_validator_peer_reliable(peer, validator_index).await
    }

    async fn replace_expected_validator_peers_reliable(
        &self,
        peers: HashMap<u32, PeerId>,
    ) -> Result<(), NetworkError> {
        self.inner.replace_expected_validator_peers_reliable(peers).await
    }

    /// A follower holds no validator index: the network keeps treating this
    /// node as a non-validator (no Rotor relay role).
    async fn set_validator_context(&self, _my_index: u32, _validator_count: u32) {}
}
