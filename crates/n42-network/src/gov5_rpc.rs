use alloy_primitives::B256;
use futures::prelude::*;
use libp2p::{StreamProtocol, request_response};
use snap::read::FrameDecoder;
use std::io::{self, Read};

/// Gov5's reliable leader-to-peer block push protocol.
pub const GOV5_BLOCK_PUSH_PROTOCOL: &str = "/rpc/block_push/1/ssz_snappy";

/// Gov5's fetch-on-miss protocol for a single block hash.
pub const GOV5_BLOCK_BY_HASH_PROTOCOL: &str = "/rpc/block_by_hash/1/ssz_snappy";

const MAX_GOV5_BLOCK_SIZE: usize = 1 << 20;
const MAX_SNAPPY_FRAME_SIZE: usize = MAX_GOV5_BLOCK_SIZE + (MAX_GOV5_BLOCK_SIZE / 6) + 1024;

#[derive(Clone, Debug)]
pub struct Gov5BlockPushRequest {
    pub rlp: Vec<u8>,
}

#[derive(Clone, Debug, Default)]
pub struct Gov5BlockPushResponse;

#[derive(Clone, Debug)]
pub struct Gov5BlockByHashRequest {
    pub block_hash: B256,
}

#[derive(Clone, Debug)]
pub struct Gov5BlockByHashResponse {
    pub rlp: Vec<u8>,
}

async fn read_bounded<T: AsyncRead + Unpin>(io: &mut T, max: usize) -> io::Result<Vec<u8>> {
    let mut encoded = Vec::new();
    io.take((max + 1) as u64).read_to_end(&mut encoded).await?;
    if encoded.len() > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "gov5 RPC response exceeds encoded size limit",
        ));
    }
    Ok(encoded)
}

fn decode_uvarint(input: &[u8]) -> io::Result<(usize, usize)> {
    let mut value = 0u64;
    for (index, byte) in input.iter().copied().take(10).enumerate() {
        if index == 9 && byte > 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "gov5 RPC length varint overflows u64",
            ));
        }
        value |= u64::from(byte & 0x7f) << (index * 7);
        if byte & 0x80 == 0 {
            let value = usize::try_from(value).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "gov5 RPC length exceeds usize")
            })?;
            return Ok((value, index + 1));
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid gov5 RPC length varint",
    ))
}

fn decode_chunked_block(encoded: &[u8]) -> io::Result<Vec<u8>> {
    if encoded.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "gov5 RPC response is missing status",
        ));
    }
    if encoded[0] != 0 {
        return Err(io::Error::other(format!(
            "gov5 peer returned status {}",
            encoded[0]
        )));
    }
    if encoded.len() < 5 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "gov5 RPC response is missing fork digest",
        ));
    }

    let (declared_len, prefix_len) = decode_uvarint(&encoded[5..])?;
    if declared_len > MAX_GOV5_BLOCK_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "gov5 RPC block exceeds decoded size limit",
        ));
    }
    let frame = encoded
        .get(5 + prefix_len..)
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "missing gov5 Snappy frame"))?;
    let mut decoded = Vec::with_capacity(declared_len);
    FrameDecoder::new(frame).read_to_end(&mut decoded)?;
    if decoded.len() != declared_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "gov5 RPC block length mismatch: declared {declared_len}, decoded {}",
                decoded.len()
            ),
        ));
    }
    Ok(decoded)
}

#[derive(Clone, Debug, Default)]
pub struct Gov5BlockPushCodec;

#[async_trait::async_trait]
impl request_response::Codec for Gov5BlockPushCodec {
    type Protocol = StreamProtocol;
    type Request = Gov5BlockPushRequest;
    type Response = Gov5BlockPushResponse;

    async fn read_request<T>(
        &mut self,
        _protocol: &Self::Protocol,
        io: &mut T,
    ) -> io::Result<Self::Request>
    where
        T: AsyncRead + Unpin + Send,
    {
        let encoded = read_bounded(io, MAX_SNAPPY_FRAME_SIZE).await?;
        Ok(Gov5BlockPushRequest {
            rlp: decode_chunked_block(&encoded)?,
        })
    }

    async fn read_response<T>(
        &mut self,
        _protocol: &Self::Protocol,
        _io: &mut T,
    ) -> io::Result<Self::Response>
    where
        T: AsyncRead + Unpin + Send,
    {
        Ok(Gov5BlockPushResponse)
    }

    async fn write_request<T>(
        &mut self,
        _protocol: &Self::Protocol,
        _io: &mut T,
        _request: Self::Request,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        Ok(())
    }

    async fn write_response<T>(
        &mut self,
        _protocol: &Self::Protocol,
        _io: &mut T,
        _response: Self::Response,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        Ok(())
    }
}

#[derive(Clone, Debug, Default)]
pub struct Gov5BlockByHashCodec;

#[async_trait::async_trait]
impl request_response::Codec for Gov5BlockByHashCodec {
    type Protocol = StreamProtocol;
    type Request = Gov5BlockByHashRequest;
    type Response = Gov5BlockByHashResponse;

    async fn read_request<T>(
        &mut self,
        _protocol: &Self::Protocol,
        io: &mut T,
    ) -> io::Result<Self::Request>
    where
        T: AsyncRead + Unpin + Send,
    {
        let mut block_hash = [0u8; 32];
        io.read_exact(&mut block_hash).await?;
        Ok(Gov5BlockByHashRequest {
            block_hash: B256::from(block_hash),
        })
    }

    async fn read_response<T>(
        &mut self,
        _protocol: &Self::Protocol,
        io: &mut T,
    ) -> io::Result<Self::Response>
    where
        T: AsyncRead + Unpin + Send,
    {
        let encoded = read_bounded(io, MAX_SNAPPY_FRAME_SIZE).await?;
        Ok(Gov5BlockByHashResponse {
            rlp: decode_chunked_block(&encoded)?,
        })
    }

    async fn write_request<T>(
        &mut self,
        _protocol: &Self::Protocol,
        io: &mut T,
        request: Self::Request,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        io.write_all(request.block_hash.as_slice()).await
    }

    async fn write_response<T>(
        &mut self,
        _protocol: &Self::Protocol,
        _io: &mut T,
        _response: Self::Response,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use snap::write::FrameEncoder;
    use std::io::Write;

    fn chunk(payload: &[u8]) -> Vec<u8> {
        let mut encoded = vec![0, 1, 2, 3, 4, payload.len() as u8];
        let mut frame = FrameEncoder::new(Vec::new());
        frame.write_all(payload).unwrap();
        encoded.extend(frame.into_inner().unwrap());
        encoded
    }

    #[test]
    fn decodes_gov5_chunked_block() {
        assert_eq!(
            decode_chunked_block(&chunk(b"block-rlp")).unwrap(),
            b"block-rlp"
        );
    }

    #[test]
    fn rejects_declared_length_mismatch() {
        let mut encoded = chunk(b"block-rlp");
        encoded[5] += 1;
        assert!(decode_chunked_block(&encoded).is_err());
    }
}
