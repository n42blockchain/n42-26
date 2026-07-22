use alloy_primitives::B256;
use futures::prelude::*;
use libp2p::{StreamProtocol, request_response};
use snap::read::FrameDecoder;
use snap::write::FrameEncoder;
use std::io::{self, Read, Write};

/// Gov5's reliable leader-to-peer block push protocol.
pub const GOV5_BLOCK_PUSH_PROTOCOL: &str = "/rpc/block_push/1/ssz_snappy";

/// Gov5's fetch-on-miss protocol for a single block hash.
pub const GOV5_BLOCK_BY_HASH_PROTOCOL: &str = "/rpc/block_by_hash/1/ssz_snappy";

/// Gov5's periodic chain-status handshake.
pub const GOV5_STATUS_PROTOCOL: &str = "/rpc/status/1/ssz_snappy";

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Gov5Status {
    pub genesis_hash: B256,
    pub current_height: u64,
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

fn encode_uvarint(mut value: usize, output: &mut Vec<u8>) {
    while value >= 0x80 {
        output.push((value as u8) | 0x80);
        value >>= 7;
    }
    output.push(value as u8);
}

fn encode_status_ssz(status: Gov5Status) -> [u8; 72] {
    let mut encoded = [0u8; 72];
    encoded[..4].copy_from_slice(&8u32.to_le_bytes());
    encoded[4..8].copy_from_slice(&40u32.to_le_bytes());
    for (source, target) in status
        .genesis_hash
        .as_slice()
        .chunks_exact(8)
        .zip(encoded[8..40].chunks_exact_mut(8))
    {
        target.copy_from_slice(&u64::from_be_bytes(source.try_into().unwrap()).to_le_bytes());
    }
    encoded[64..72].copy_from_slice(&status.current_height.to_le_bytes());
    encoded
}

fn decode_status_ssz(encoded: &[u8]) -> io::Result<Gov5Status> {
    if encoded.len() != 72
        || encoded[..4] != 8u32.to_le_bytes()
        || encoded[4..8] != 40u32.to_le_bytes()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid gov5 Status SSZ layout",
        ));
    }
    let mut genesis_hash = [0u8; 32];
    for (source, target) in encoded[8..40]
        .chunks_exact(8)
        .zip(genesis_hash.chunks_exact_mut(8))
    {
        target.copy_from_slice(&u64::from_le_bytes(source.try_into().unwrap()).to_be_bytes());
    }
    if encoded[40..64].iter().any(|byte| *byte != 0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "gov5 Status height exceeds u64",
        ));
    }
    Ok(Gov5Status {
        genesis_hash: B256::from(genesis_hash),
        current_height: u64::from_le_bytes(encoded[64..72].try_into().unwrap()),
    })
}

fn encode_status(status: Gov5Status, response: bool) -> io::Result<Vec<u8>> {
    let payload = encode_status_ssz(status);
    let mut frame = FrameEncoder::new(Vec::new());
    frame.write_all(&payload)?;
    let compressed = frame.into_inner().map_err(io::Error::other)?;
    let mut encoded = Vec::with_capacity(1 + 10 + compressed.len());
    if response {
        encoded.push(0);
    }
    encode_uvarint(payload.len(), &mut encoded);
    encoded.extend_from_slice(&compressed);
    Ok(encoded)
}

fn decode_status(encoded: &[u8], response: bool) -> io::Result<Gov5Status> {
    let payload = if response {
        let (&status, payload) = encoded.split_first().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "gov5 Status response is empty",
            )
        })?;
        if status != 0 {
            return Err(io::Error::other(format!(
                "gov5 peer returned Status code {status}"
            )));
        }
        payload
    } else {
        encoded
    };
    let (declared_len, prefix_len) = decode_uvarint(payload)?;
    if declared_len != 72 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "gov5 Status declared length is not 72",
        ));
    }
    let compressed = payload.get(prefix_len..).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "gov5 Status payload is missing",
        )
    })?;
    let mut decoded = Vec::with_capacity(declared_len);
    FrameDecoder::new(compressed)
        .take((declared_len + 1) as u64)
        .read_to_end(&mut decoded)?;
    if decoded.len() != declared_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "gov5 Status decoded length mismatch",
        ));
    }
    decode_status_ssz(&decoded)
}

async fn read_status<T>(io: &mut T, response: bool) -> io::Result<Gov5Status>
where
    T: AsyncRead + Unpin,
{
    tracing::debug!(response, "reading gov5 Status stream");
    let mut encoded = Vec::with_capacity(if response { 64 } else { 63 });
    if response {
        let mut status = [0u8; 1];
        io.read_exact(&mut status).await?;
        if status[0] != 0 {
            return Err(io::Error::other(format!(
                "gov5 peer returned Status code {}",
                status[0]
            )));
        }
        encoded.push(status[0]);
    }

    let mut prefix = Vec::with_capacity(2);
    let declared_len = loop {
        if prefix.len() == 10 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "gov5 Status length prefix is too long",
            ));
        }
        let mut byte = [0u8; 1];
        io.read_exact(&mut byte).await?;
        prefix.push(byte[0]);
        if byte[0] & 0x80 == 0 {
            break decode_uvarint(&prefix)?.0;
        }
    };
    if declared_len != 72 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "gov5 Status declared length is not 72",
        ));
    }
    encoded.extend_from_slice(&prefix);

    // Treat the framed payload, rather than connection EOF, as the message
    // boundary on the bidirectional libp2p stream. Read complete Snappy chunks
    // until they produce the fixed-size Status payload.
    let mut framed = Vec::with_capacity(64);
    for _ in 0..8 {
        let mut header = [0u8; 4];
        io.read_exact(&mut header).await?;
        let chunk_len =
            usize::from(header[1]) | (usize::from(header[2]) << 8) | (usize::from(header[3]) << 16);
        if chunk_len > 256 || framed.len() + 4 + chunk_len > 256 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "gov5 Status Snappy frame is too large",
            ));
        }
        framed.extend_from_slice(&header);
        let start = framed.len();
        framed.resize(start + chunk_len, 0);
        io.read_exact(&mut framed[start..]).await?;

        let mut decoded = Vec::with_capacity(declared_len);
        FrameDecoder::new(framed.as_slice())
            .take((declared_len + 1) as u64)
            .read_to_end(&mut decoded)?;
        if decoded.len() == declared_len {
            encoded.extend_from_slice(&framed);
            let status = decode_status(&encoded, response)?;
            tracing::debug!(
                response,
                height = status.current_height,
                "decoded gov5 Status stream"
            );
            return Ok(status);
        }
        if decoded.len() > declared_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "gov5 Status decoded length exceeds declaration",
            ));
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "gov5 Status has too many Snappy chunks",
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

#[derive(Clone, Debug, Default)]
pub struct Gov5StatusCodec;

#[async_trait::async_trait]
impl request_response::Codec for Gov5StatusCodec {
    type Protocol = StreamProtocol;
    type Request = Gov5Status;
    type Response = Gov5Status;

    async fn read_request<T>(
        &mut self,
        _protocol: &Self::Protocol,
        io: &mut T,
    ) -> io::Result<Self::Request>
    where
        T: AsyncRead + Unpin + Send,
    {
        read_status(io, false).await
    }

    async fn read_response<T>(
        &mut self,
        _protocol: &Self::Protocol,
        io: &mut T,
    ) -> io::Result<Self::Response>
    where
        T: AsyncRead + Unpin + Send,
    {
        read_status(io, true).await
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
        tracing::debug!(
            height = request.current_height,
            "writing gov5 Status request"
        );
        io.write_all(&encode_status(request, false)?).await
    }

    async fn write_response<T>(
        &mut self,
        _protocol: &Self::Protocol,
        io: &mut T,
        response: Self::Response,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        tracing::debug!(
            height = response.current_height,
            "writing gov5 Status response"
        );
        io.write_all(&encode_status(response, true)?).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{pin::Pin, task::Poll};

    struct NoEof {
        inner: futures::io::Cursor<Vec<u8>>,
    }

    impl AsyncRead for NoEof {
        fn poll_read(
            mut self: Pin<&mut Self>,
            context: &mut std::task::Context<'_>,
            buffer: &mut [u8],
        ) -> Poll<io::Result<usize>> {
            if self.inner.position() == self.inner.get_ref().len() as u64 {
                Poll::Pending
            } else {
                Pin::new(&mut self.inner).poll_read(context, buffer)
            }
        }
    }

    fn chunk(payload: &[u8]) -> Vec<u8> {
        let mut encoded = vec![0, 1, 2, 3, 4, payload.len() as u8];
        let mut frame = FrameEncoder::new(Vec::new());
        frame.write_all(payload).unwrap();
        encoded.extend(frame.into_inner().unwrap());
        encoded
    }

    #[test]
    fn status_codec_roundtrips_exact_gov5_ssz_shape() {
        let status = Gov5Status {
            genesis_hash: B256::from([0xabu8; 32]),
            current_height: 0x0102_0304_0506_0708,
        };
        let request = encode_status(status, false).unwrap();
        let response = encode_status(status, true).unwrap();
        assert_eq!(decode_status(&request, false).unwrap(), status);
        assert_eq!(decode_status(&response, true).unwrap(), status);

        // Keep github.com/golang/snappy's exact framed output as a bidirectional
        // fixture so this exercises the cross-language stream boundary.
        let gov5_request = alloy_primitives::hex::decode(
            "48ff060000734e61507059002000006832d6a248200800000028000000ab7a010000005a01001c0807060504030201",
        )
        .unwrap();
        assert_eq!(request, gov5_request);
        assert_eq!(decode_status(&gov5_request, false).unwrap(), status);

        let (_, prefix_len) = decode_uvarint(&request).unwrap();
        let mut raw = Vec::new();
        FrameDecoder::new(&request[prefix_len..])
            .read_to_end(&mut raw)
            .unwrap();
        assert_eq!(&raw[..4], &8u32.to_le_bytes());
        assert_eq!(&raw[4..8], &40u32.to_le_bytes());
        assert_eq!(&raw[64..72], &status.current_height.to_le_bytes());
    }

    #[test]
    fn status_codec_rejects_wrong_genesis_layout_and_error_status() {
        let status = Gov5Status {
            genesis_hash: B256::ZERO,
            current_height: 49,
        };
        let mut response = encode_status(status, true).unwrap();
        response[0] = 2;
        assert!(decode_status(&response, true).is_err());

        let mut ssz = encode_status_ssz(status);
        ssz[..4].copy_from_slice(&9u32.to_le_bytes());
        assert!(decode_status_ssz(&ssz).is_err());
    }

    #[test]
    fn status_reader_uses_frame_boundary_without_waiting_for_eof() {
        let status = Gov5Status {
            genesis_hash: B256::from([0x42; 32]),
            current_height: 151,
        };
        let mut stream = NoEof {
            inner: futures::io::Cursor::new(encode_status(status, false).unwrap()),
        };
        assert_eq!(
            futures::executor::block_on(read_status(&mut stream, false)).unwrap(),
            status
        );
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
