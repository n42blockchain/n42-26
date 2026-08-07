use futures::prelude::*;
use libp2p::request_response;
use libp2p::StreamProtocol;
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

/// Response: acknowledgement with credit-based flow control.
///
/// Inspired by Firedancer's fctl credit system: the leader tells each follower
/// how many more transactions it can accept, preventing leader overload.
/// A `remaining_credit` of 0 means "stop sending until I grant more".
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TxForwardResponse {
    pub accepted: bool,
    /// Remaining TX credit for this follower. Follower must not send more than
    /// this many transactions until the next response grants more credit.
    /// `None` means unlimited (backward-compatible default).
    #[serde(default)]
    pub remaining_credit: Option<u32>,
}

/// Codec for the tx forward request-response protocol.
#[derive(Clone, Debug, Default)]
pub struct TxForwardCodec;

impl request_response::Codec for TxForwardCodec {
    type Protocol = StreamProtocol;
    type Request = TxForwardRequest;
    type Response = TxForwardResponse;

    async fn read_request<T>(
        &mut self,
        _protocol: &Self::Protocol,
        io: &mut T,
    ) -> io::Result<Self::Request>
    where
        T: AsyncRead + Unpin + Send,
    {
        codec::read_length_prefixed(io, MAX_TX_FORWARD_SIZE).await
    }

    async fn read_response<T>(
        &mut self,
        _protocol: &Self::Protocol,
        io: &mut T,
    ) -> io::Result<Self::Response>
    where
        T: AsyncRead + Unpin + Send,
    {
        codec::read_length_prefixed(io, MAX_TX_FORWARD_SIZE).await
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
        codec::write_length_prefixed(io, &req, MAX_TX_FORWARD_SIZE).await
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
        codec::write_length_prefixed(io, &res, MAX_TX_FORWARD_SIZE).await
    }
}
