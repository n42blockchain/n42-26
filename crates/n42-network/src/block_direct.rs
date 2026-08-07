use futures::prelude::*;
use libp2p::request_response;
use libp2p::StreamProtocol;
use serde::{Deserialize, Serialize};
use std::io;

use crate::codec;

/// Protocol identifier for direct block data push from leader to validators.
pub const BLOCK_DIRECT_PROTOCOL: &str = "/n42/block-direct/1";

/// Maximum block direct message size (16 MB — sufficient for large execution payloads).
pub const MAX_BLOCK_DIRECT_SIZE: usize = 16 * 1024 * 1024;

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

    async fn read_request<T>(
        &mut self,
        _protocol: &Self::Protocol,
        io: &mut T,
    ) -> io::Result<Self::Request>
    where
        T: AsyncRead + Unpin + Send,
    {
        codec::read_length_prefixed(io, MAX_BLOCK_DIRECT_SIZE).await
    }

    async fn read_response<T>(
        &mut self,
        _protocol: &Self::Protocol,
        io: &mut T,
    ) -> io::Result<Self::Response>
    where
        T: AsyncRead + Unpin + Send,
    {
        codec::read_length_prefixed(io, MAX_BLOCK_DIRECT_SIZE).await
    }

    async fn write_request<T>(
        &mut self,
        _protocol: &Self::Protocol,
        io: &mut T,
        req: Self::Request,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        codec::write_length_prefixed(io, &req, MAX_BLOCK_DIRECT_SIZE).await
    }

    async fn write_response<T>(
        &mut self,
        _protocol: &Self::Protocol,
        io: &mut T,
        res: Self::Response,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        codec::write_length_prefixed(io, &res, MAX_BLOCK_DIRECT_SIZE).await
    }
}
