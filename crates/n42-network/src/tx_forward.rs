use futures::prelude::*;
use libp2p::StreamProtocol;
use libp2p::request_response;
use serde::{Deserialize, Serialize};
use std::io;

use crate::codec;

/// Protocol identifier for forwarding transactions to the current leader.
pub const TX_FORWARD_PROTOCOL: &str = "/n42/tx-forward/1";

/// Maximum tx forward message size (4 MB — enough for ~2000 txs batched).
const MAX_TX_FORWARD_SIZE: usize = 4 * 1024 * 1024;

/// Request: batch of RLP-encoded transactions forwarded to the leader.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TxForwardRequest {
    pub txs: Vec<Vec<u8>>,
}

/// Response: simple acknowledgement.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TxForwardResponse {
    pub accepted: bool,
}

/// Codec for the tx forward request-response protocol.
#[derive(Clone, Debug, Default)]
pub struct TxForwardCodec;

impl request_response::Codec for TxForwardCodec {
    type Protocol = StreamProtocol;
    type Request = TxForwardRequest;
    type Response = TxForwardResponse;

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
        Box::pin(codec::read_length_prefixed(io, MAX_TX_FORWARD_SIZE))
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
        Box::pin(codec::read_length_prefixed(io, MAX_TX_FORWARD_SIZE))
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
        Box::pin(async move { codec::write_length_prefixed(io, &req, MAX_TX_FORWARD_SIZE).await })
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
        Box::pin(async move { codec::write_length_prefixed(io, &res, MAX_TX_FORWARD_SIZE).await })
    }
}
