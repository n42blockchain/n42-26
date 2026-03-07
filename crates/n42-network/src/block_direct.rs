use futures::prelude::*;
use libp2p::request_response;
use libp2p::StreamProtocol;
use serde::{Deserialize, Serialize};
use std::io;

use crate::codec;

/// Protocol identifier for direct block data push from leader to validators.
pub const BLOCK_DIRECT_PROTOCOL: &str = "/n42/block-direct/1";

/// Maximum block direct message size (16 MB — sufficient for large execution payloads).
const MAX_BLOCK_DIRECT_SIZE: usize = 16 * 1024 * 1024;

/// Request: carries serialized block data from leader to follower.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BlockDirectRequest {
    pub data: Vec<u8>,
}

/// Response: simple acknowledgement.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BlockDirectResponse {
    pub accepted: bool,
}

/// Codec for the block direct request-response protocol.
#[derive(Clone, Debug, Default)]
pub struct BlockDirectCodec;

impl request_response::Codec for BlockDirectCodec {
    type Protocol = StreamProtocol;
    type Request = BlockDirectRequest;
    type Response = BlockDirectResponse;

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
        Box::pin(codec::read_length_prefixed(io, MAX_BLOCK_DIRECT_SIZE))
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
        Box::pin(codec::read_length_prefixed(io, MAX_BLOCK_DIRECT_SIZE))
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
        Box::pin(async move { codec::write_length_prefixed(io, &req, MAX_BLOCK_DIRECT_SIZE).await })
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
        Box::pin(async move { codec::write_length_prefixed(io, &res, MAX_BLOCK_DIRECT_SIZE).await })
    }
}
