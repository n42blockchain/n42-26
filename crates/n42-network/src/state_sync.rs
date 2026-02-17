use alloy_primitives::B256;
use n42_primitives::consensus::QuorumCertificate;
use serde::{Deserialize, Serialize};

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
    /// Peer's latest committed view (helps detect if we need more rounds).
    pub peer_committed_view: u64,
}

/// A single committed block for sync purposes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SyncBlock {
    /// View this block was committed at.
    pub view: u64,
    /// Block hash.
    pub block_hash: B256,
    /// CommitQC proving the block was committed.
    pub commit_qc: QuorumCertificate,
    /// Serialized payload data (JSON or RLP encoded block content).
    pub payload: Vec<u8>,
}

// ── libp2p request-response codec ──

use futures::prelude::*;
use libp2p::request_response;
use libp2p::StreamProtocol;
use std::io;

/// Protocol identifier for block sync.
pub const SYNC_PROTOCOL: &str = "/n42/sync/1";

/// Maximum size for sync messages (16 MB — supports batches of blocks).
const MAX_SYNC_MSG_SIZE: usize = 16 * 1024 * 1024;

/// Codec for serializing/deserializing block sync requests and responses.
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
        Box::pin(async move {
            read_length_prefixed(io, MAX_SYNC_MSG_SIZE).await
        })
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
        Box::pin(async move {
            read_length_prefixed(io, MAX_SYNC_MSG_SIZE).await
        })
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
        Box::pin(async move {
            write_length_prefixed(io, &req).await
        })
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
        Box::pin(async move {
            write_length_prefixed(io, &res).await
        })
    }
}

/// Reads a length-prefixed bincode-encoded message.
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
            format!("message too large: {} > {}", len, max_size),
        ));
    }
    let mut buf = vec![0u8; len];
    io.read_exact(&mut buf).await?;
    bincode::deserialize(&buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Writes a length-prefixed bincode-encoded message.
async fn write_length_prefixed<T, M>(io: &mut T, msg: &M) -> io::Result<()>
where
    T: AsyncWrite + Unpin + Send,
    M: serde::Serialize,
{
    let data = bincode::serialize(msg)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let len = (data.len() as u32).to_be_bytes();
    io.write_all(&len).await?;
    io.write_all(&data).await?;
    io.flush().await?;
    Ok(())
}
