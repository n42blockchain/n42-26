use futures::prelude::*;
use libp2p::request_response;
use libp2p::StreamProtocol;
use serde::{Deserialize, Serialize};
use std::io;

/// Protocol identifier for consensus direct messaging.
pub const CONSENSUS_DIRECT_PROTOCOL: &str = "/n42/consensus-direct/1";

/// Maximum consensus direct message size (1 MB — sufficient for any single consensus message).
const MAX_CONSENSUS_DIRECT_SIZE: usize = 1024 * 1024;

/// Request: wraps a serialized `ConsensusMessage` as raw bytes.
///
/// Uses `message_bytes: Vec<u8>` rather than a concrete `ConsensusMessage` type to keep
/// `n42-network` decoupled from `n42-primitives` consensus types. Encoding/decoding is
/// performed by the caller.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConsensusDirectRequest {
    pub message_bytes: Vec<u8>,
}

/// Response: simple acknowledgement.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConsensusDirectResponse {
    pub accepted: bool,
}

/// Codec for the consensus direct request-response protocol.
///
/// Mirrors the length-prefixed bincode pattern used by `StateSyncCodec`.
#[derive(Clone, Debug, Default)]
pub struct ConsensusDirectCodec;

impl request_response::Codec for ConsensusDirectCodec {
    type Protocol = StreamProtocol;
    type Request = ConsensusDirectRequest;
    type Response = ConsensusDirectResponse;

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
        Box::pin(read_length_prefixed(io, MAX_CONSENSUS_DIRECT_SIZE))
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
        Box::pin(read_length_prefixed(io, MAX_CONSENSUS_DIRECT_SIZE))
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
async fn write_length_prefixed<T, M>(io: &mut T, msg: &M) -> io::Result<()>
where
    T: AsyncWrite + Unpin + Send,
    M: serde::Serialize,
{
    let data =
        bincode::serialize(msg).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if data.len() > MAX_CONSENSUS_DIRECT_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "serialized message too large: {} > {MAX_CONSENSUS_DIRECT_SIZE}",
                data.len()
            ),
        ));
    }
    let len_u32 = u32::try_from(data.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("serialized message length {} overflows u32", data.len()),
        )
    })?;
    io.write_all(&len_u32.to_be_bytes()).await?;
    io.write_all(&data).await?;
    io.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_request() -> ConsensusDirectRequest {
        ConsensusDirectRequest {
            message_bytes: vec![1, 2, 3, 4, 5],
        }
    }

    fn sample_response_accepted() -> ConsensusDirectResponse {
        ConsensusDirectResponse { accepted: true }
    }

    fn sample_response_rejected() -> ConsensusDirectResponse {
        ConsensusDirectResponse { accepted: false }
    }

    #[test]
    fn test_request_serde_roundtrip() {
        let req = sample_request();
        let bytes = bincode::serialize(&req).unwrap();
        let decoded: ConsensusDirectRequest = bincode::deserialize(&bytes).unwrap();
        assert_eq!(decoded.message_bytes, req.message_bytes);
    }

    #[test]
    fn test_response_serde_roundtrip() {
        let resp = sample_response_accepted();
        let bytes = bincode::serialize(&resp).unwrap();
        let decoded: ConsensusDirectResponse = bincode::deserialize(&bytes).unwrap();
        assert!(decoded.accepted);

        let resp = sample_response_rejected();
        let bytes = bincode::serialize(&resp).unwrap();
        let decoded: ConsensusDirectResponse = bincode::deserialize(&bytes).unwrap();
        assert!(!decoded.accepted);
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
        let decoded: ConsensusDirectRequest =
            read_length_prefixed(&mut reader, MAX_CONSENSUS_DIRECT_SIZE)
                .await
                .unwrap();
        assert_eq!(decoded.message_bytes, req.message_bytes);
    }

    #[tokio::test]
    async fn test_codec_write_read_response_roundtrip() {
        let resp = sample_response_accepted();
        let mut buf = futures::io::Cursor::new(Vec::new());
        write_length_prefixed(&mut buf, &resp).await.unwrap();

        let data = buf.into_inner();
        let mut reader = futures::io::Cursor::new(data);
        let decoded: ConsensusDirectResponse =
            read_length_prefixed(&mut reader, MAX_CONSENSUS_DIRECT_SIZE)
                .await
                .unwrap();
        assert!(decoded.accepted);
    }

    #[tokio::test]
    async fn test_codec_read_rejects_oversized() {
        let fake_len = (MAX_CONSENSUS_DIRECT_SIZE as u32 + 1).to_be_bytes();
        let mut data = Vec::new();
        data.extend_from_slice(&fake_len);
        data.extend_from_slice(&[0u8; 100]);

        let mut reader = futures::io::Cursor::new(data);
        let result: io::Result<ConsensusDirectRequest> =
            read_length_prefixed(&mut reader, MAX_CONSENSUS_DIRECT_SIZE).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
    }
}
