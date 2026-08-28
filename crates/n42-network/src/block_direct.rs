use futures::prelude::*;
use libp2p::StreamProtocol;
use libp2p::request_response;
use serde::{Deserialize, Serialize};
use std::io;
use std::sync::Arc;
use std::time::Instant;

use crate::codec;

/// Version 3 replaces one 32 MiB request with a manifest and bounded chunks.
/// QUIC keeps the peer connection alive; ACK pacing permits only one direct
/// frame per peer at a time and prevents substream/backlog explosions.
pub const BLOCK_DIRECT_PROTOCOL: &str = "/n42/block-direct/3";

/// Maximum reconstructed direct block envelope (independent of frame size).
pub const MAX_BLOCK_DIRECT_SIZE: usize = 256 * 1024 * 1024;
pub const MIN_BLOCK_DIRECT_CHUNK_SIZE: usize = 2 * 1024 * 1024;
pub const MAX_BLOCK_DIRECT_CHUNK_SIZE: usize = 4 * 1024 * 1024;
const FRAME_HEADER_MAX: usize = 64;
pub const MAX_BLOCK_DIRECT_FRAME_SIZE: usize = MAX_BLOCK_DIRECT_CHUNK_SIZE + FRAME_HEADER_MAX;

const FRAME_COMPLETE: u8 = 0;
const FRAME_MANIFEST: u8 = 1;
const FRAME_CHUNK: u8 = 2;

pub fn block_direct_chunk_size() -> usize {
    static SIZE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *SIZE.get_or_init(|| {
        std::env::var("N42_BLOCK_DIRECT_CHUNK_MIB")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(4)
            .clamp(2, 4)
            * 1024
            * 1024
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockDirectManifest {
    pub transfer_id: [u8; 32],
    pub total_len: u64,
    pub chunk_size: u32,
    pub chunk_count: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlockDirectFrame {
    Complete {
        data: Arc<Vec<u8>>,
    },
    Manifest(BlockDirectManifest),
    /// One long-lived request substream: manifest followed by bounded chunks.
    /// The underlying QUIC peer connection remains shared across transfers.
    Transfer {
        manifest: BlockDirectManifest,
        data: Arc<Vec<u8>>,
    },
    Chunk {
        transfer_id: [u8; 32],
        index: u32,
        data: Arc<Vec<u8>>,
        offset: usize,
        len: usize,
    },
}

impl BlockDirectFrame {
    pub fn payload_len(&self) -> usize {
        match self {
            Self::Complete { data } => data.len(),
            Self::Manifest(_) => 0,
            Self::Transfer { data, .. } => data.len(),
            Self::Chunk { len, .. } => *len,
        }
    }

    fn wire_len(&self) -> usize {
        match self {
            Self::Complete { data } => 1 + data.len(),
            Self::Manifest(_) => 1 + 32 + 8 + 4 + 4,
            // Transfer has no outer frame length. `u32::MAX` is a sentinel,
            // followed by a fixed manifest and length-prefixed chunks.
            Self::Transfer { .. } => 0,
            Self::Chunk { len, .. } => 1 + 32 + 4 + len,
        }
    }
}

#[derive(Clone, Debug)]
pub struct BlockDirectRequest {
    pub frame: BlockDirectFrame,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BlockDirectResponse {
    pub accepted: bool,
}

#[derive(Clone, Debug, Default)]
pub struct BlockDirectCodec;

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

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
        let read_started = Instant::now();
        let mut len_buf = [0u8; 4];
        io.read_exact(&mut len_buf).await?;
        let len = u32::from_be_bytes(len_buf) as usize;
        if len == u32::MAX as usize {
            let mut manifest_bytes = [0u8; 48];
            io.read_exact(&mut manifest_bytes).await?;
            let manifest = BlockDirectManifest {
                transfer_id: manifest_bytes[..32]
                    .try_into()
                    .expect("fixed manifest transfer id"),
                total_len: u64::from_be_bytes(
                    manifest_bytes[32..40]
                        .try_into()
                        .expect("fixed manifest total length"),
                ),
                chunk_size: u32::from_be_bytes(
                    manifest_bytes[40..44]
                        .try_into()
                        .expect("fixed manifest chunk size"),
                ),
                chunk_count: u32::from_be_bytes(
                    manifest_bytes[44..48]
                        .try_into()
                        .expect("fixed manifest chunk count"),
                ),
            };
            let total_len = usize::try_from(manifest.total_len)
                .map_err(|_| invalid_data("block direct transfer length overflows usize"))?;
            let chunk_size = manifest.chunk_size as usize;
            if total_len <= chunk_size
                || total_len > MAX_BLOCK_DIRECT_SIZE
                || !(MIN_BLOCK_DIRECT_CHUNK_SIZE..=MAX_BLOCK_DIRECT_CHUNK_SIZE)
                    .contains(&chunk_size)
                || manifest.chunk_count as usize != total_len.div_ceil(chunk_size)
            {
                return Err(invalid_data("invalid block direct transfer manifest"));
            }
            let mut data = Vec::new();
            data.try_reserve_exact(total_len)
                .map_err(|_| invalid_data("could not reserve block direct transfer"))?;
            for index in 0..manifest.chunk_count {
                io.read_exact(&mut len_buf).await?;
                let chunk_len = u32::from_be_bytes(len_buf) as usize;
                let expected = (total_len - index as usize * chunk_size).min(chunk_size);
                if chunk_len != expected {
                    return Err(invalid_data(format!(
                        "invalid block direct chunk {index} length {chunk_len}, expected {expected}"
                    )));
                }
                let offset = data.len();
                data.resize(offset + chunk_len, 0);
                io.read_exact(&mut data[offset..]).await?;
            }
            if blake3::hash(&data).as_bytes() != &manifest.transfer_id {
                return Err(invalid_data("block direct transfer digest mismatch"));
            }
            metrics::counter!("n42_block_direct_chunks_received_total")
                .increment(manifest.chunk_count as u64);
            metrics::counter!("n42_block_direct_chunk_bytes_received_total")
                .increment(total_len as u64);
            tracing::info!(
                bytes = total_len,
                chunks = manifest.chunk_count,
                elapsed_ms = read_started.elapsed().as_millis() as u64,
                "N42_BLOCK_DIRECT_STREAM_READ_DONE: long-lived chunk stream complete"
            );
            return Ok(BlockDirectRequest {
                frame: BlockDirectFrame::Transfer {
                    manifest,
                    data: Arc::new(data),
                },
            });
        }
        if len == 0 || len > MAX_BLOCK_DIRECT_FRAME_SIZE {
            return Err(invalid_data(format!(
                "block direct frame size invalid: {len} (max {MAX_BLOCK_DIRECT_FRAME_SIZE})"
            )));
        }
        let mut encoded = vec![0u8; len];
        io.read_exact(&mut encoded).await?;
        let tag = encoded[0];
        let frame = match tag {
            FRAME_COMPLETE => {
                let data = encoded.split_off(1);
                BlockDirectFrame::Complete {
                    data: Arc::new(data),
                }
            }
            FRAME_MANIFEST => {
                if encoded.len() != 49 {
                    return Err(invalid_data("invalid block direct manifest length"));
                }
                let transfer_id = encoded[1..33].try_into().expect("checked manifest length");
                let total_len = u64::from_be_bytes(
                    encoded[33..41].try_into().expect("checked manifest length"),
                );
                let chunk_size = u32::from_be_bytes(
                    encoded[41..45].try_into().expect("checked manifest length"),
                );
                let chunk_count = u32::from_be_bytes(
                    encoded[45..49].try_into().expect("checked manifest length"),
                );
                BlockDirectFrame::Manifest(BlockDirectManifest {
                    transfer_id,
                    total_len,
                    chunk_size,
                    chunk_count,
                })
            }
            FRAME_CHUNK => {
                if encoded.len() < 37 {
                    return Err(invalid_data("invalid block direct chunk length"));
                }
                let transfer_id = encoded[1..33].try_into().expect("checked chunk length");
                let index =
                    u32::from_be_bytes(encoded[33..37].try_into().expect("checked chunk length"));
                let data = encoded.split_off(37);
                let len = data.len();
                BlockDirectFrame::Chunk {
                    transfer_id,
                    index,
                    data: Arc::new(data),
                    offset: 0,
                    len,
                }
            }
            other => {
                return Err(invalid_data(format!(
                    "unknown block direct frame tag {other}"
                )));
            }
        };
        if frame.payload_len() >= 1024 * 1024 {
            tracing::debug!(
                bytes = frame.payload_len(),
                elapsed_ms = read_started.elapsed().as_millis() as u64,
                "N42_BLOCK_DIRECT_READ_DONE: completed bounded direct frame"
            );
        }
        Ok(BlockDirectRequest { frame })
    }

    async fn read_response<T>(
        &mut self,
        _protocol: &Self::Protocol,
        io: &mut T,
    ) -> io::Result<Self::Response>
    where
        T: AsyncRead + Unpin + Send,
    {
        codec::read_length_prefixed(io, 1024).await
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
        if let BlockDirectFrame::Transfer { manifest, data } = &req.frame {
            if data.len() != manifest.total_len as usize
                || data.len() > MAX_BLOCK_DIRECT_SIZE
                || manifest.chunk_size as usize != block_direct_chunk_size()
                || manifest.chunk_count as usize
                    != data.len().div_ceil(manifest.chunk_size as usize)
            {
                return Err(invalid_data("invalid outbound block direct transfer"));
            }
            // The network service computes the digest once for the immutable
            // Arc and reuses it across fanout and retries. The receiver verifies
            // these exact bytes before constructing `BlockDirectFrame::Transfer`.
            metrics::counter!(
                "n42_block_direct_digest_rehash_avoided_bytes_total",
                "site" => "outbound_codec",
            )
            .increment(data.len() as u64);
            io.write_all(&u32::MAX.to_be_bytes()).await?;
            io.write_all(&manifest.transfer_id).await?;
            io.write_all(&manifest.total_len.to_be_bytes()).await?;
            io.write_all(&manifest.chunk_size.to_be_bytes()).await?;
            io.write_all(&manifest.chunk_count.to_be_bytes()).await?;
            for chunk in data.chunks(manifest.chunk_size as usize) {
                io.write_all(&(chunk.len() as u32).to_be_bytes()).await?;
                io.write_all(chunk).await?;
            }
            io.flush().await?;
            metrics::counter!("n42_block_direct_chunks_sent_total")
                .increment(manifest.chunk_count as u64);
            tracing::debug!(
                bytes = data.len(),
                chunks = manifest.chunk_count,
                "N42_BLOCK_DIRECT_STREAM_WRITE_DONE: long-lived chunk stream flushed"
            );
            return Ok(());
        }

        let wire_len = req.frame.wire_len();
        if wire_len > MAX_BLOCK_DIRECT_FRAME_SIZE {
            return Err(invalid_data(format!(
                "block direct frame too large: {wire_len} > {MAX_BLOCK_DIRECT_FRAME_SIZE}"
            )));
        }
        io.write_all(&(wire_len as u32).to_be_bytes()).await?;
        match req.frame {
            BlockDirectFrame::Complete { data } => {
                io.write_all(&[FRAME_COMPLETE]).await?;
                io.write_all(data.as_slice()).await?;
            }
            BlockDirectFrame::Manifest(manifest) => {
                io.write_all(&[FRAME_MANIFEST]).await?;
                io.write_all(&manifest.transfer_id).await?;
                io.write_all(&manifest.total_len.to_be_bytes()).await?;
                io.write_all(&manifest.chunk_size.to_be_bytes()).await?;
                io.write_all(&manifest.chunk_count.to_be_bytes()).await?;
            }
            BlockDirectFrame::Transfer { .. } => unreachable!("transfer returned above"),
            BlockDirectFrame::Chunk {
                transfer_id,
                index,
                data,
                offset,
                len,
            } => {
                let end = offset
                    .checked_add(len)
                    .filter(|end| *end <= data.len())
                    .ok_or_else(|| invalid_data("block direct chunk range out of bounds"))?;
                io.write_all(&[FRAME_CHUNK]).await?;
                io.write_all(&transfer_id).await?;
                io.write_all(&index.to_be_bytes()).await?;
                io.write_all(&data[offset..end]).await?;
            }
        }
        io.flush().await
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
        codec::write_length_prefixed(io, &res, 1024).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libp2p::request_response::Codec as _;

    async fn roundtrip(frame: BlockDirectFrame) -> BlockDirectFrame {
        let mut writer = futures::io::Cursor::new(Vec::new());
        let mut codec = BlockDirectCodec;
        let protocol = StreamProtocol::new(BLOCK_DIRECT_PROTOCOL);
        codec
            .write_request(&protocol, &mut writer, BlockDirectRequest { frame })
            .await
            .unwrap();
        let mut reader = futures::io::Cursor::new(writer.into_inner());
        codec
            .read_request(&protocol, &mut reader)
            .await
            .unwrap()
            .frame
    }

    #[tokio::test]
    async fn complete_frame_roundtrips() {
        let payload = Arc::new(vec![1, 2, 3, 4, 5]);
        let decoded = roundtrip(BlockDirectFrame::Complete {
            data: Arc::clone(&payload),
        })
        .await;
        assert!(
            matches!(decoded, BlockDirectFrame::Complete { data } if data.as_slice() == payload.as_slice())
        );
    }

    #[tokio::test]
    async fn manifest_and_zero_copy_chunk_range_roundtrip() {
        let id = [7; 32];
        let manifest = BlockDirectManifest {
            transfer_id: id,
            total_len: 9,
            chunk_size: 4,
            chunk_count: 3,
        };
        assert_eq!(
            roundtrip(BlockDirectFrame::Manifest(manifest)).await,
            BlockDirectFrame::Manifest(manifest)
        );

        let backing = Arc::new(vec![0, 1, 2, 3, 4, 5]);
        let decoded = roundtrip(BlockDirectFrame::Chunk {
            transfer_id: id,
            index: 1,
            data: backing,
            offset: 2,
            len: 3,
        })
        .await;
        assert!(
            matches!(decoded, BlockDirectFrame::Chunk { transfer_id, index: 1, data, offset: 0, len: 3 } if transfer_id == id && data.as_slice() == [2, 3, 4])
        );
    }

    #[tokio::test]
    async fn transfer_uses_one_stream_with_manifest_and_chunks() {
        let chunk_size = block_direct_chunk_size();
        let data = Arc::new(vec![0x5a; chunk_size + 17]);
        let manifest = BlockDirectManifest {
            transfer_id: *blake3::hash(data.as_slice()).as_bytes(),
            total_len: data.len() as u64,
            chunk_size: chunk_size as u32,
            chunk_count: 2,
        };
        let decoded = roundtrip(BlockDirectFrame::Transfer {
            manifest,
            data: Arc::clone(&data),
        })
        .await;
        assert!(
            matches!(decoded, BlockDirectFrame::Transfer { manifest: decoded_manifest, data: decoded } if decoded_manifest == manifest && decoded.as_slice() == data.as_slice())
        );
    }

    #[tokio::test]
    async fn transfer_receiver_still_rejects_digest_mismatch() {
        let chunk_size = block_direct_chunk_size();
        let data = Arc::new(vec![0x5a; chunk_size + 17]);
        let manifest = BlockDirectManifest {
            transfer_id: [0x11; 32],
            total_len: data.len() as u64,
            chunk_size: chunk_size as u32,
            chunk_count: 2,
        };
        let mut writer = futures::io::Cursor::new(Vec::new());
        let mut codec = BlockDirectCodec;
        let protocol = StreamProtocol::new(BLOCK_DIRECT_PROTOCOL);
        codec
            .write_request(
                &protocol,
                &mut writer,
                BlockDirectRequest {
                    frame: BlockDirectFrame::Transfer { manifest, data },
                },
            )
            .await
            .expect("sender trusts its prepared immutable digest");

        let mut reader = futures::io::Cursor::new(writer.into_inner());
        let error = codec
            .read_request(&protocol, &mut reader)
            .await
            .expect_err("receiver must verify the transfer digest");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("digest mismatch"));
    }

    #[tokio::test]
    async fn rejects_oversized_length_before_allocation() {
        let mut frame = futures::io::Cursor::new(
            ((MAX_BLOCK_DIRECT_FRAME_SIZE as u32) + 1)
                .to_be_bytes()
                .to_vec(),
        );
        let mut codec = BlockDirectCodec;
        let protocol = StreamProtocol::new(BLOCK_DIRECT_PROTOCOL);
        let error = codec.read_request(&protocol, &mut frame).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
