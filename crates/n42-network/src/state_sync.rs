use alloy_primitives::B256;
use futures::prelude::*;
use libp2p::request_response;
use libp2p::StreamProtocol;
use n42_primitives::consensus::QuorumCertificate;
use serde::{Deserialize, Serialize};
use std::io;

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
        Box::pin(read_length_prefixed(io, MAX_SYNC_MSG_SIZE))
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
        Box::pin(read_length_prefixed(io, MAX_SYNC_MSG_SIZE))
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
        Box::pin(async move { write_length_prefixed(io, &req).await })
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
        Box::pin(async move { write_length_prefixed(io, &res).await })
    }
}

/// Reads a length-prefixed bincode-encoded message (big-endian u32 length header).
async fn read_length_prefixed<T, M>(io: &mut T, max_size: usize) -> io::Result<M>
where
    T: AsyncRead + Unpin + Send,
    M: serde::de::DeserializeOwned,
{
    let mut len_buf = [0u8; 4];
    io.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > max_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("message too large: {len} > {max_size}"),
        ));
    }
    let mut buf = vec![0u8; len];
    io.read_exact(&mut buf).await?;
    bincode::deserialize(&buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Writes a length-prefixed bincode-encoded message (big-endian u32 length header).
///
/// Enforces `MAX_SYNC_MSG_SIZE` at the sender so oversized messages fail early
/// rather than wasting bandwidth only to be rejected by the receiver.
async fn write_length_prefixed<T, M>(io: &mut T, msg: &M) -> io::Result<()>
where
    T: AsyncWrite + Unpin + Send,
    M: serde::Serialize,
{
    let data = bincode::serialize(msg)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if data.len() > MAX_SYNC_MSG_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("serialized message too large: {} > {MAX_SYNC_MSG_SIZE}", data.len()),
        ));
    }
    io.write_all(&(data.len() as u32).to_be_bytes()).await?;
    io.write_all(&data).await?;
    io.flush().await
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
        write_length_prefixed(&mut buf, &req).await.unwrap();

        let data = buf.into_inner();
        let len = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
        assert_eq!(len, data.len() - 4);

        let mut reader = futures::io::Cursor::new(data);
        let decoded: BlockSyncRequest = read_length_prefixed(&mut reader, MAX_SYNC_MSG_SIZE).await.unwrap();
        assert_eq!(decoded.from_view, req.from_view);
        assert_eq!(decoded.to_view, req.to_view);
        assert_eq!(decoded.local_committed_view, req.local_committed_view);
    }

    #[tokio::test]
    async fn test_codec_write_read_response_roundtrip() {
        let resp = sample_response();
        let mut buf = futures::io::Cursor::new(Vec::new());
        write_length_prefixed(&mut buf, &resp).await.unwrap();

        let data = buf.into_inner();
        let mut reader = futures::io::Cursor::new(data);
        let decoded: BlockSyncResponse = read_length_prefixed(&mut reader, MAX_SYNC_MSG_SIZE).await.unwrap();
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
            read_length_prefixed(&mut reader, MAX_SYNC_MSG_SIZE).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
    }
}
