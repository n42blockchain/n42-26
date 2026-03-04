use alloy_primitives::B256;
use futures::prelude::*;
use libp2p::request_response;
use libp2p::StreamProtocol;
use n42_primitives::consensus::QuorumCertificate;
use serde::{Deserialize, Serialize};
use std::io;

use crate::codec;

/// Maximum number of blocks that can be requested in a single sync request.
pub const MAX_BLOCKS_PER_SYNC_REQUEST: u64 = 128;

/// Block sync request: asks a peer for blocks in a view range.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BlockSyncRequest {
    /// Start of the requested view range (inclusive).
    pub from_view: u64,
    /// End of the requested view range (inclusive).
    pub to_view: u64,
    /// Requester's committed view (for peer to gauge how far behind we are).
    pub local_committed_view: u64,
}

impl BlockSyncRequest {
    /// Validates the request range. Returns an error string if invalid.
    pub fn validate(&self) -> Result<(), String> {
        if self.from_view > self.to_view {
            return Err(format!(
                "invalid sync range: from_view ({}) > to_view ({})",
                self.from_view, self.to_view
            ));
        }
        // Inclusive range [from_view, to_view] contains (to_view - from_view + 1) views.
        let block_count = self.to_view - self.from_view + 1;
        if block_count > MAX_BLOCKS_PER_SYNC_REQUEST {
            return Err(format!(
                "sync range too large: {} blocks (max {})",
                block_count, MAX_BLOCKS_PER_SYNC_REQUEST
            ));
        }
        Ok(())
    }
}

/// Block sync response: contains committed blocks for the requested range.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BlockSyncResponse {
    /// Blocks in view order.
    pub blocks: Vec<SyncBlock>,
    /// Peer's latest committed view.
    pub peer_committed_view: u64,
}

/// A single committed block for sync purposes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SyncBlock {
    pub view: u64,
    pub block_hash: B256,
    pub commit_qc: QuorumCertificate,
    /// Serialized block payload (RLP or JSON encoded).
    pub payload: Vec<u8>,
}

// ── libp2p request-response codec ──

/// Protocol identifier for block sync.
pub const SYNC_PROTOCOL: &str = "/n42/sync/1";

/// Maximum sync message size (16 MB — supports batches of blocks).
const MAX_SYNC_MSG_SIZE: usize = 16 * 1024 * 1024;

/// Codec for serializing/deserializing block sync messages.
#[derive(Clone, Debug, Default)]
pub struct StateSyncCodec;

impl request_response::Codec for StateSyncCodec {
    type Protocol = StreamProtocol;
    type Request = BlockSyncRequest;
    type Response = BlockSyncResponse;

    fn read_request<'life0, 'life1, 'life2, 'async_trait, T>(
        &'life0 mut self,
        _protocol: &'life1 Self::Protocol,
        io: &'life2 mut T,
    ) -> std::pin::Pin<Box<dyn Future<Output = io::Result<Self::Request>> + Send + 'async_trait>>
    where
        T: AsyncRead + Unpin + Send + 'async_trait,
        'life0: 'async_trait,
        'life1: 'async_trait,
        'life2: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(codec::read_length_prefixed(io, MAX_SYNC_MSG_SIZE))
    }

    fn read_response<'life0, 'life1, 'life2, 'async_trait, T>(
        &'life0 mut self,
        _protocol: &'life1 Self::Protocol,
        io: &'life2 mut T,
    ) -> std::pin::Pin<Box<dyn Future<Output = io::Result<Self::Response>> + Send + 'async_trait>>
    where
        T: AsyncRead + Unpin + Send + 'async_trait,
        'life0: 'async_trait,
        'life1: 'async_trait,
        'life2: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(codec::read_length_prefixed(io, MAX_SYNC_MSG_SIZE))
    }

    fn write_request<'life0, 'life1, 'life2, 'async_trait, T>(
        &'life0 mut self,
        _protocol: &'life1 Self::Protocol,
        io: &'life2 mut T,
        req: Self::Request,
    ) -> std::pin::Pin<Box<dyn Future<Output = io::Result<()>> + Send + 'async_trait>>
    where
        T: AsyncWrite + Unpin + Send + 'async_trait,
        'life0: 'async_trait,
        'life1: 'async_trait,
        'life2: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move { codec::write_length_prefixed(io, &req, MAX_SYNC_MSG_SIZE).await })
    }

    fn write_response<'life0, 'life1, 'life2, 'async_trait, T>(
        &'life0 mut self,
        _protocol: &'life1 Self::Protocol,
        io: &'life2 mut T,
        res: Self::Response,
    ) -> std::pin::Pin<Box<dyn Future<Output = io::Result<()>> + Send + 'async_trait>>
    where
        T: AsyncWrite + Unpin + Send + 'async_trait,
        'life0: 'async_trait,
        'life1: 'async_trait,
        'life2: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move { codec::write_length_prefixed(io, &res, MAX_SYNC_MSG_SIZE).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::B256;
    use n42_primitives::consensus::QuorumCertificate;

    fn sample_request() -> BlockSyncRequest {
        BlockSyncRequest { from_view: 10, to_view: 20, local_committed_view: 8 }
    }

    fn sample_sync_block() -> SyncBlock {
        use n42_primitives::BlsSecretKey;
        let sk = BlsSecretKey::random().unwrap();
        let sig = sk.sign(b"test");
        SyncBlock {
            view: 15,
            block_hash: B256::repeat_byte(0xAA),
            commit_qc: QuorumCertificate {
                view: 15,
                block_hash: B256::repeat_byte(0xAA),
                aggregate_signature: sig,
                signers: Default::default(),
            },
            payload: vec![1, 2, 3, 4],
        }
    }

    fn sample_response() -> BlockSyncResponse {
        BlockSyncResponse { blocks: vec![sample_sync_block()], peer_committed_view: 25 }
    }

    #[test]
    fn test_block_sync_request_serde_roundtrip() {
        let req = sample_request();
        let bytes = bincode::serialize(&req).unwrap();
        let decoded: BlockSyncRequest = bincode::deserialize(&bytes).unwrap();
        assert_eq!(decoded.from_view, req.from_view);
        assert_eq!(decoded.to_view, req.to_view);
        assert_eq!(decoded.local_committed_view, req.local_committed_view);
    }

    #[test]
    fn test_block_sync_response_serde_roundtrip() {
        let resp = sample_response();
        let bytes = bincode::serialize(&resp).unwrap();
        let decoded: BlockSyncResponse = bincode::deserialize(&bytes).unwrap();
        assert_eq!(decoded.blocks.len(), 1);
        assert_eq!(decoded.blocks[0].view, 15);
        assert_eq!(decoded.blocks[0].block_hash, B256::repeat_byte(0xAA));
        assert_eq!(decoded.blocks[0].payload, vec![1, 2, 3, 4]);
        assert_eq!(decoded.peer_committed_view, 25);
    }

    #[test]
    fn test_block_sync_response_empty_blocks() {
        let resp = BlockSyncResponse { blocks: vec![], peer_committed_view: 100 };
        let bytes = bincode::serialize(&resp).unwrap();
        let decoded: BlockSyncResponse = bincode::deserialize(&bytes).unwrap();
        assert!(decoded.blocks.is_empty());
        assert_eq!(decoded.peer_committed_view, 100);
    }

    #[tokio::test]
    async fn test_codec_write_read_request_roundtrip() {
        let req = sample_request();
        let mut buf = futures::io::Cursor::new(Vec::new());
        codec::write_length_prefixed(&mut buf, &req, MAX_SYNC_MSG_SIZE).await.unwrap();

        let data = buf.into_inner();
        let len = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
        assert_eq!(len, data.len() - 4);

        let mut reader = futures::io::Cursor::new(data);
        let decoded: BlockSyncRequest = codec::read_length_prefixed(&mut reader, MAX_SYNC_MSG_SIZE).await.unwrap();
        assert_eq!(decoded.from_view, req.from_view);
        assert_eq!(decoded.to_view, req.to_view);
        assert_eq!(decoded.local_committed_view, req.local_committed_view);
    }

    #[tokio::test]
    async fn test_codec_write_read_response_roundtrip() {
        let resp = sample_response();
        let mut buf = futures::io::Cursor::new(Vec::new());
        codec::write_length_prefixed(&mut buf, &resp, MAX_SYNC_MSG_SIZE).await.unwrap();

        let data = buf.into_inner();
        let mut reader = futures::io::Cursor::new(data);
        let decoded: BlockSyncResponse = codec::read_length_prefixed(&mut reader, MAX_SYNC_MSG_SIZE).await.unwrap();
        assert_eq!(decoded.blocks.len(), 1);
        assert_eq!(decoded.blocks[0].view, 15);
        assert_eq!(decoded.peer_committed_view, 25);
    }

    #[tokio::test]
    async fn test_codec_read_rejects_oversized() {
        let fake_len = (MAX_SYNC_MSG_SIZE as u32 + 1).to_be_bytes();
        let mut data = Vec::new();
        data.extend_from_slice(&fake_len);
        data.extend_from_slice(&[0u8; 100]);

        let mut reader = futures::io::Cursor::new(data);
        let result: io::Result<BlockSyncRequest> =
            codec::read_length_prefixed(&mut reader, MAX_SYNC_MSG_SIZE).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
    }
}
