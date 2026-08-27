use alloy_primitives::B256;
use futures::prelude::*;
use libp2p::{StreamProtocol, request_response};
use snap::read::FrameDecoder;
use snap::write::FrameEncoder;
use std::fmt;
use std::io::{self, Read, Write};
use std::sync::Arc;

/// Gov5's reliable leader-to-peer block push protocol.
pub const GOV5_BLOCK_PUSH_PROTOCOL: &str = "/rpc/block_push/1/ssz_snappy";

/// Gov5's fetch-on-miss protocol for a single block hash.
pub const GOV5_BLOCK_BY_HASH_PROTOCOL: &str = "/rpc/block_by_hash/1/ssz_snappy";

/// Gov5's periodic chain-status handshake.
pub const GOV5_STATUS_PROTOCOL: &str = "/rpc/status/1/ssz_snappy";

/// Gov5's canonical-chain catch-up protocol.
pub const GOV5_BODIES_BY_RANGE_PROTOCOL: &str = "/rpc/bodies_by_range/1/ssz_snappy";

/// Gov5's one-way Rotor/leader-direct HotStuff stream.
pub const GOV5_HOTSTUFF_DIRECT_PROTOCOL: &str = "/rpc/hotstuff_direct/1";

const MAX_GOV5_BLOCK_SIZE: usize = 1 << 20;
const MAX_SNAPPY_FRAME_SIZE: usize = MAX_GOV5_BLOCK_SIZE + (MAX_GOV5_BLOCK_SIZE / 6) + 1024;
const MAX_GOV5_HOTSTUFF_SIZE: usize = 16 * 1024;

/// Gov5's `maxRequestBlocks` / `rangeLimit`.
pub const MAX_GOV5_RANGE_BLOCKS: u64 = 1024;
/// Gov5 uses a dedicated 64 MiB decoded limit for block response chunks.
pub const MAX_GOV5_RANGE_BLOCK_SIZE: usize = 64 * 1024 * 1024;
const MAX_GOV5_RANGE_WIRE_SIZE: usize =
    MAX_GOV5_RANGE_BLOCK_SIZE + (MAX_GOV5_RANGE_BLOCK_SIZE / 6) + 1024;
const GOV5_RANGE_REQUEST_SSZ_LEN: usize = 52;
type Gov5BestBlockNumber = dyn Fn() -> Result<u64, String> + Send + Sync;
type Gov5BlockRlpByNumber = dyn Fn(u64) -> Result<Option<Vec<u8>>, String> + Send + Sync;

/// A persistent canonical-chain reader installed by the node layer.
///
/// Keeping this as a narrow callback boundary avoids making the transport
/// crate depend on Reth's concrete provider type. Implementations must read
/// canonical storage by number; the recent in-memory Gov5 body cache is not a
/// valid source for this protocol.
#[derive(Clone)]
pub struct Gov5CanonicalBlockReader {
    best_block_number: Arc<Gov5BestBlockNumber>,
    block_rlp_by_number: Arc<Gov5BlockRlpByNumber>,
}

impl fmt::Debug for Gov5CanonicalBlockReader {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Gov5CanonicalBlockReader")
            .finish_non_exhaustive()
    }
}

impl Gov5CanonicalBlockReader {
    pub fn new<B, R>(best_block_number: B, block_rlp_by_number: R) -> Self
    where
        B: Fn() -> Result<u64, String> + Send + Sync + 'static,
        R: Fn(u64) -> Result<Option<Vec<u8>>, String> + Send + Sync + 'static,
    {
        Self {
            best_block_number: Arc::new(best_block_number),
            block_rlp_by_number: Arc::new(block_rlp_by_number),
        }
    }

    pub fn best_block_number(&self) -> Result<u64, String> {
        (self.best_block_number)()
    }

    pub fn block_rlp_by_number(&self, number: u64) -> Result<Option<Vec<u8>>, String> {
        (self.block_rlp_by_number)(number)
    }
}

/// Gov5's SSZ `{StartBlockNumber: H256, Count: uint64, Step: uint64}` request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Gov5BodiesByRangeRequest {
    pub start: u64,
    pub count: u64,
    pub step: u64,
}

impl Gov5BodiesByRangeRequest {
    pub fn validate(self) -> io::Result<()> {
        if self.count == 0 || self.count > MAX_GOV5_RANGE_BLOCKS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Gov5 bodies-by-range count must be in 1..=1024",
            ));
        }
        if self.step == 0 || self.step > MAX_GOV5_RANGE_BLOCKS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Gov5 bodies-by-range step must be in 1..=1024",
            ));
        }
        let span = self.step.checked_mul(self.count - 1).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Gov5 bodies-by-range step*count overflows u64",
            )
        })?;
        if span > MAX_GOV5_RANGE_BLOCKS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Gov5 bodies-by-range span exceeds 1024 blocks",
            ));
        }
        self.start.checked_add(span).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Gov5 bodies-by-range end block overflows u64",
            )
        })?;
        Ok(())
    }

    fn to_ssz(self) -> [u8; GOV5_RANGE_REQUEST_SSZ_LEN] {
        let mut encoded = [0u8; GOV5_RANGE_REQUEST_SSZ_LEN];
        encoded[..4].copy_from_slice(&20u32.to_le_bytes());
        encoded[4..12].copy_from_slice(&self.count.to_le_bytes());
        encoded[12..20].copy_from_slice(&self.step.to_le_bytes());
        // H256 is four uint64 words in SSZ. For a u64 block number only the
        // low word is non-zero; its SSZ representation is little-endian.
        encoded[44..52].copy_from_slice(&self.start.to_le_bytes());
        encoded
    }

    fn from_ssz(encoded: &[u8]) -> io::Result<Self> {
        if encoded.len() != GOV5_RANGE_REQUEST_SSZ_LEN
            || encoded[..4] != 20u32.to_le_bytes()
            || encoded[20..44].iter().any(|byte| *byte != 0)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid Gov5 bodies-by-range SSZ layout",
            ));
        }
        Ok(Self {
            start: u64::from_le_bytes(encoded[44..52].try_into().unwrap()),
            count: u64::from_le_bytes(encoded[4..12].try_into().unwrap()),
            step: u64::from_le_bytes(encoded[12..20].try_into().unwrap()),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Gov5RangeBlockChunk {
    pub fork_digest: [u8; 4],
    pub rlp: Vec<u8>,
}

/// A range reply read from the wire or streamed from persistent storage.
#[derive(Clone)]
pub enum Gov5BodiesByRangeResponse {
    Blocks(Vec<Gov5RangeBlockChunk>),
    Error {
        code: u8,
        message: String,
    },
    Stream {
        request: Gov5BodiesByRangeRequest,
        fork_digest: [u8; 4],
        reader: Gov5CanonicalBlockReader,
    },
}

impl fmt::Debug for Gov5BodiesByRangeResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Blocks(blocks) => formatter.debug_tuple("Blocks").field(blocks).finish(),
            Self::Error { code, message } => formatter
                .debug_struct("Error")
                .field("code", code)
                .field("message", message)
                .finish(),
            Self::Stream {
                request,
                fork_digest,
                ..
            } => formatter
                .debug_struct("Stream")
                .field("request", request)
                .field("fork_digest", fork_digest)
                .finish_non_exhaustive(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Gov5HotstuffDirectRequest {
    /// Exact raw-Snappy canonical Gov5 consensus gossip payload.
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, Default)]
pub struct Gov5HotstuffDirectResponse;

#[derive(Clone, Debug, Default)]
pub struct Gov5HotstuffDirectCodec;

impl request_response::Codec for Gov5HotstuffDirectCodec {
    type Protocol = StreamProtocol;
    type Request = Gov5HotstuffDirectRequest;
    type Response = Gov5HotstuffDirectResponse;

    async fn read_request<T>(&mut self, _: &StreamProtocol, io: &mut T) -> io::Result<Self::Request>
    where
        T: AsyncRead + Unpin + Send,
    {
        Ok(Gov5HotstuffDirectRequest {
            data: read_bounded(io, MAX_GOV5_HOTSTUFF_SIZE).await?,
        })
    }

    async fn read_response<T>(
        &mut self,
        _: &StreamProtocol,
        io: &mut T,
    ) -> io::Result<Self::Response>
    where
        T: AsyncRead + Unpin + Send,
    {
        let response = read_bounded(io, 0).await?;
        if !response.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Gov5 HotStuff direct response is not empty",
            ));
        }
        Ok(Gov5HotstuffDirectResponse)
    }

    async fn write_request<T>(
        &mut self,
        _: &StreamProtocol,
        io: &mut T,
        request: Self::Request,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        if request.data.len() > MAX_GOV5_HOTSTUFF_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Gov5 HotStuff direct request exceeds size limit",
            ));
        }
        io.write_all(&request.data).await?;
        io.close().await
    }

    async fn write_response<T>(
        &mut self,
        _: &StreamProtocol,
        io: &mut T,
        _: Self::Response,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        io.close().await
    }
}

#[derive(Clone, Debug)]
pub struct Gov5BlockPushRequest {
    pub rlp: Vec<u8>,
    pub fork_digest: [u8; 4],
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
    // Cap the decompressed stream at the declaration, as the Status paths do.
    // The wire frame is bounded, but Snappy expansion is not: a ~1 MiB frame of
    // minimal chunks expands to several GiB, and reading it to the end would
    // exhaust memory before the length check below ever runs. Reading one byte
    // past the declaration is enough to still detect an over-long payload.
    FrameDecoder::new(frame)
        .take((declared_len + 1) as u64)
        .read_to_end(&mut decoded)?;
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

fn encode_chunked_block(rlp: &[u8], fork_digest: [u8; 4]) -> io::Result<Vec<u8>> {
    if rlp.len() > MAX_GOV5_BLOCK_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "gov5 RPC block exceeds encoded size limit",
        ));
    }

    let mut frame = FrameEncoder::new(Vec::new());
    frame.write_all(rlp)?;
    let compressed = frame.into_inner().map_err(io::Error::other)?;

    let mut encoded = Vec::with_capacity(5 + 10 + compressed.len());
    encoded.push(0);
    encoded.extend_from_slice(&fork_digest);
    encode_uvarint(rlp.len(), &mut encoded);
    encoded.extend_from_slice(&compressed);
    Ok(encoded)
}

fn encode_framed_payload(payload: &[u8], max_decoded: usize) -> io::Result<Vec<u8>> {
    if payload.len() > max_decoded {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Gov5 framed payload exceeds decoded size limit",
        ));
    }
    let mut frame = FrameEncoder::new(Vec::new());
    frame.write_all(payload)?;
    let compressed = frame.into_inner().map_err(io::Error::other)?;
    let mut encoded = Vec::with_capacity(10 + compressed.len());
    encode_uvarint(payload.len(), &mut encoded);
    encoded.extend_from_slice(&compressed);
    Ok(encoded)
}

async fn read_framed_payload<T>(
    io: &mut T,
    max_wire: usize,
    max_decoded: usize,
) -> io::Result<Vec<u8>>
where
    T: AsyncRead + Unpin + Send,
{
    let mut encoded = Vec::with_capacity(128);
    let declared_len = loop {
        if encoded.len() == 10 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Gov5 framed payload length varint is too long",
            ));
        }
        let mut byte = [0u8; 1];
        io.read_exact(&mut byte).await?;
        encoded.push(byte[0]);
        if byte[0] & 0x80 == 0 {
            break decode_uvarint(&encoded)?.0;
        }
    };
    if declared_len > max_decoded {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Gov5 framed payload exceeds decoded size limit",
        ));
    }

    let prefix_len = encoded.len();
    let mut produced = 0usize;
    let mut saw_stream_identifier = false;
    while produced < declared_len || !saw_stream_identifier {
        let mut header = [0u8; 4];
        io.read_exact(&mut header).await?;
        let chunk_len =
            usize::from(header[1]) | (usize::from(header[2]) << 8) | (usize::from(header[3]) << 16);
        let next_len = encoded
            .len()
            .checked_add(4)
            .and_then(|length| length.checked_add(chunk_len))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Gov5 Snappy frame size overflows",
                )
            })?;
        if next_len > max_wire {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Gov5 Snappy frame exceeds wire size limit",
            ));
        }
        encoded.extend_from_slice(&header);
        let body_start = encoded.len();
        encoded.resize(next_len, 0);
        io.read_exact(&mut encoded[body_start..]).await?;
        let body = &encoded[body_start..];

        let decoded = match header[0] {
            0xff => {
                saw_stream_identifier = true;
                0
            }
            0x00 => {
                let compressed = body.get(4..).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "compressed Gov5 Snappy chunk is missing its checksum",
                    )
                })?;
                snap::raw::decompress_len(compressed).map_err(io::Error::other)?
            }
            0x01 => body.len().checked_sub(4).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "uncompressed Gov5 Snappy chunk is missing its checksum",
                )
            })?,
            0x80..=0xfe => 0,
            kind => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("reserved Gov5 Snappy chunk type {kind:#x}"),
                ));
            }
        };
        produced = produced.checked_add(decoded).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Gov5 Snappy decoded length overflows",
            )
        })?;
        if produced > declared_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Gov5 Snappy payload exceeds its declared length",
            ));
        }
    }

    let mut decoded = Vec::with_capacity(declared_len);
    FrameDecoder::new(&encoded[prefix_len..])
        .take((declared_len + 1) as u64)
        .read_to_end(&mut decoded)?;
    if decoded.len() != declared_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Gov5 Snappy payload does not match its declared length",
        ));
    }
    Ok(decoded)
}

fn encode_range_chunk(chunk: &Gov5RangeBlockChunk) -> io::Result<Vec<u8>> {
    let mut encoded = Vec::with_capacity(5 + chunk.rlp.len());
    encoded.push(0);
    encoded.extend_from_slice(&chunk.fork_digest);
    encoded.extend_from_slice(&encode_framed_payload(
        &chunk.rlp,
        MAX_GOV5_RANGE_BLOCK_SIZE,
    )?);
    Ok(encoded)
}

fn encode_range_error(code: u8, message: &str) -> io::Result<Vec<u8>> {
    let mut encoded = vec![code];
    encoded.extend_from_slice(&encode_framed_payload(
        message.as_bytes(),
        MAX_GOV5_BLOCK_SIZE,
    )?);
    Ok(encoded)
}

#[derive(Clone, Debug, Default)]
pub struct Gov5BodiesByRangeCodec;

impl request_response::Codec for Gov5BodiesByRangeCodec {
    type Protocol = StreamProtocol;
    type Request = Gov5BodiesByRangeRequest;
    type Response = Gov5BodiesByRangeResponse;

    async fn read_request<T>(&mut self, _: &Self::Protocol, io: &mut T) -> io::Result<Self::Request>
    where
        T: AsyncRead + Unpin + Send,
    {
        let encoded = read_framed_payload(io, 1024, GOV5_RANGE_REQUEST_SSZ_LEN).await?;
        Gov5BodiesByRangeRequest::from_ssz(&encoded)
    }

    async fn read_response<T>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
    ) -> io::Result<Self::Response>
    where
        T: AsyncRead + Unpin + Send,
    {
        let mut blocks = Vec::new();
        loop {
            let mut status = [0u8; 1];
            match io.read(&mut status).await? {
                0 => return Ok(Gov5BodiesByRangeResponse::Blocks(blocks)),
                1 if status[0] == 0 => {}
                1 => {
                    let message =
                        read_framed_payload(io, MAX_SNAPPY_FRAME_SIZE, MAX_GOV5_BLOCK_SIZE).await?;
                    return Ok(Gov5BodiesByRangeResponse::Error {
                        code: status[0],
                        message: String::from_utf8_lossy(&message).into_owned(),
                    });
                }
                _ => unreachable!("one-byte read buffer"),
            }
            if blocks.len() >= MAX_GOV5_RANGE_BLOCKS as usize {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Gov5 peer served more than 1024 range blocks",
                ));
            }
            let mut fork_digest = [0u8; 4];
            io.read_exact(&mut fork_digest).await?;
            let rlp = read_framed_payload(io, MAX_GOV5_RANGE_WIRE_SIZE, MAX_GOV5_RANGE_BLOCK_SIZE)
                .await?;
            blocks.push(Gov5RangeBlockChunk { fork_digest, rlp });
        }
    }

    async fn write_request<T>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
        request: Self::Request,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        request.validate()?;
        io.write_all(&encode_framed_payload(
            &request.to_ssz(),
            GOV5_RANGE_REQUEST_SSZ_LEN,
        )?)
        .await?;
        io.close().await
    }

    async fn write_response<T>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
        response: Self::Response,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        match response {
            Gov5BodiesByRangeResponse::Blocks(blocks) => {
                if blocks.len() > MAX_GOV5_RANGE_BLOCKS as usize {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "Gov5 range response contains more than 1024 blocks",
                    ));
                }
                for block in blocks {
                    io.write_all(&encode_range_chunk(&block)?).await?;
                }
            }
            Gov5BodiesByRangeResponse::Error { code, message } => {
                io.write_all(&encode_range_error(code, &message)?).await?;
            }
            Gov5BodiesByRangeResponse::Stream {
                request,
                fork_digest,
                reader,
            } => {
                request.validate()?;
                // This deliberately matches gov5: validate the requested
                // step/span first, then normalize any step > 1 to a
                // contiguous response. Current gov5 clients always send 1.
                let end = request
                    .start
                    .checked_add(request.count - 1)
                    .expect("validated range cannot overflow");
                let mut previous_hash = None;
                for number in request.start..=end {
                    let rlp = match reader.block_rlp_by_number(number) {
                        Ok(Some(rlp)) => rlp,
                        Ok(None) => {
                            io.write_all(&encode_range_error(2, "block not found")?)
                                .await?;
                            break;
                        }
                        Err(error) => {
                            io.write_all(&encode_range_error(2, &error)?).await?;
                            break;
                        }
                    };
                    if rlp.len() > MAX_GOV5_RANGE_BLOCK_SIZE {
                        io.write_all(&encode_range_error(2, "block exceeds 64 MiB chunk limit")?)
                            .await?;
                        break;
                    }
                    let decoded = match crate::gov5_block::decode_gov5_block_rlp(&rlp) {
                        Ok(decoded) => decoded,
                        Err(error) => {
                            io.write_all(&encode_range_error(
                                2,
                                &format!("invalid canonical block: {error}"),
                            )?)
                            .await?;
                            break;
                        }
                    };
                    if decoded.header.number != number {
                        io.write_all(&encode_range_error(2, "canonical block number mismatch")?)
                            .await?;
                        break;
                    }
                    if previous_hash.is_some_and(|previous| decoded.header.parent_hash != previous)
                    {
                        // Match gov5: a broken by-number canonical sequence is
                        // truncated at the linked prefix rather than poisoning
                        // the requester with a disjoint block.
                        break;
                    }
                    previous_hash = Some(decoded.block_hash);
                    io.write_all(&encode_range_chunk(&Gov5RangeBlockChunk {
                        fork_digest,
                        rlp,
                    })?)
                    .await?;
                }
            }
        }
        io.close().await
    }
}

#[derive(Clone, Debug, Default)]
pub struct Gov5BlockPushCodec;

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
        let fork_digest = encoded
            .get(1..5)
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "gov5 block push is missing fork digest",
                )
            })?;
        Ok(Gov5BlockPushRequest {
            rlp: decode_chunked_block(&encoded)?,
            fork_digest,
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
        io: &mut T,
        request: Self::Request,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        // Gov5 treats block-push as a one-way stream carrying its normal
        // chunked-block response shape.
        io.write_all(&encode_chunked_block(&request.rlp, request.fork_digest)?)
            .await
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
        io: &mut T,
        response: Self::Response,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        if response.rlp.is_empty() {
            // Gov5's first response byte is its RPC status. A non-zero status
            // makes a cache miss fail closed instead of looking like a valid
            // zero-length block.
            io.write_all(&[1]).await
        } else {
            // Rust readers validate the block hash after decoding, so a zero
            // digest is sufficient for the Rust-to-Rust recovery path. Gov5
            // does not currently request blocks from Rust over this protocol.
            io.write_all(&encode_chunked_block(&response.rlp, [0; 4])?)
                .await
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Gov5StatusCodec;

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
    use alloy_consensus::{Header, TxEnvelope, proofs::calculate_transaction_root};
    use alloy_primitives::{Bytes, U256, keccak256};
    use alloy_rlp::{Encodable, Header as RlpHeader};
    use libp2p::request_response::Codec;
    use std::collections::HashMap;
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

    fn range_block(number: u64, parent_hash: B256) -> (B256, Vec<u8>) {
        let mut extra_data = Vec::from(&b"N42H"[..]);
        extra_data.extend_from_slice(&number.to_le_bytes());
        let header = Header {
            parent_hash,
            number,
            ommers_hash: B256::ZERO,
            transactions_root: calculate_transaction_root::<TxEnvelope>(&[]),
            difficulty: U256::ZERO,
            base_fee_per_gas: Some(0),
            extra_data: Bytes::from(extra_data),
            ..Default::default()
        };
        let mut header_rlp = Vec::new();
        header.encode(&mut header_rlp);
        let transactions = Vec::<Bytes>::new();
        let verifiers = Vec::<Bytes>::new();
        let rewards = Vec::<Bytes>::new();
        let payload_length =
            header_rlp.len() + transactions.length() + verifiers.length() + rewards.length();
        let mut encoded = Vec::new();
        RlpHeader {
            list: true,
            payload_length,
        }
        .encode(&mut encoded);
        encoded.extend_from_slice(&header_rlp);
        transactions.encode(&mut encoded);
        verifiers.encode(&mut encoded);
        rewards.encode(&mut encoded);
        (keccak256(header_rlp), encoded)
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
    fn gov5_chunked_block_encoding_roundtrips() {
        let encoded = encode_chunked_block(b"block-rlp", [1, 2, 3, 4]).unwrap();
        assert_eq!(&encoded[..5], &[0, 1, 2, 3, 4]);
        assert_eq!(decode_chunked_block(&encoded).unwrap(), b"block-rlp");
    }

    #[test]
    fn block_by_hash_response_roundtrips_and_cache_miss_fails_closed() {
        let protocol = StreamProtocol::new(GOV5_BLOCK_BY_HASH_PROTOCOL);
        let mut writer = futures::io::Cursor::new(Vec::new());
        futures::executor::block_on(Gov5BlockByHashCodec.write_response(
            &protocol,
            &mut writer,
            Gov5BlockByHashResponse {
                rlp: b"block-rlp".to_vec(),
            },
        ))
        .unwrap();
        assert_eq!(
            decode_chunked_block(&writer.into_inner()).unwrap(),
            b"block-rlp"
        );

        let mut miss = futures::io::Cursor::new(Vec::new());
        futures::executor::block_on(Gov5BlockByHashCodec.write_response(
            &protocol,
            &mut miss,
            Gov5BlockByHashResponse { rlp: Vec::new() },
        ))
        .unwrap();
        assert!(decode_chunked_block(&miss.into_inner()).is_err());
    }

    #[test]
    fn hotstuff_direct_codec_preserves_exact_one_way_payload() {
        let protocol = StreamProtocol::new(GOV5_HOTSTUFF_DIRECT_PROTOCOL);
        let payload = vec![0xff, 0x06, 0, 0, 0, 0x42, 0x24];
        let mut writer = futures::io::Cursor::new(Vec::new());
        futures::executor::block_on(Gov5HotstuffDirectCodec.write_request(
            &protocol,
            &mut writer,
            Gov5HotstuffDirectRequest {
                data: payload.clone(),
            },
        ))
        .unwrap();
        assert_eq!(writer.into_inner(), payload);

        let mut reader = futures::io::Cursor::new(payload.clone());
        let decoded = futures::executor::block_on(
            Gov5HotstuffDirectCodec.read_request(&protocol, &mut reader),
        )
        .unwrap();
        assert_eq!(decoded.data, payload);
    }

    #[test]
    fn rejects_declared_length_mismatch() {
        let mut encoded = chunk(b"block-rlp");
        encoded[5] += 1;
        assert!(decode_chunked_block(&encoded).is_err());
    }

    /// A peer controls both the declared length and the Snappy frame, and the
    /// two need not agree. Repetitive input compresses about 21x here, so
    /// decoding to the end of the frame — rather than to the declaration — lets
    /// one wire-legal response allocate roughly twenty times the 1 MiB block
    /// cap, and every concurrent request multiplies that. Stop at the
    /// declaration instead.
    #[test]
    fn rejects_snappy_expansion_beyond_the_declared_length() {
        const DECODED_BYTES: usize = 16 * 1024 * 1024;
        let mut frame = FrameEncoder::new(Vec::new());
        frame.write_all(&vec![0u8; DECODED_BYTES]).unwrap();
        let compressed = frame.into_inner().unwrap();
        assert!(
            compressed.len() < MAX_GOV5_BLOCK_SIZE,
            "the bomb must fit inside a wire-legal frame to be a real attack: {} bytes",
            compressed.len()
        );

        let mut encoded = vec![0, 1, 2, 3, 4];
        encode_uvarint(9, &mut encoded);
        encoded.extend_from_slice(&compressed);

        let error = decode_chunked_block(&encoded).expect_err("expansion must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn bodies_by_range_request_matches_gov5_ssz_and_framing() {
        let request = Gov5BodiesByRangeRequest {
            start: 7,
            count: 1024,
            step: 1,
        };
        let ssz = request.to_ssz();
        assert_eq!(&ssz[..4], &20u32.to_le_bytes());
        assert_eq!(&ssz[4..12], &1024u64.to_le_bytes());
        assert_eq!(&ssz[12..20], &1u64.to_le_bytes());
        assert_eq!(&ssz[20..44], &[0u8; 24]);
        assert_eq!(&ssz[44..52], &7u64.to_le_bytes());

        let protocol = StreamProtocol::new(GOV5_BODIES_BY_RANGE_PROTOCOL);
        let mut wire = futures::io::Cursor::new(Vec::new());
        futures::executor::block_on(
            Gov5BodiesByRangeCodec.write_request(&protocol, &mut wire, request),
        )
        .unwrap();
        let mut wire = futures::io::Cursor::new(wire.into_inner());
        assert_eq!(
            futures::executor::block_on(Gov5BodiesByRangeCodec.read_request(&protocol, &mut wire))
                .unwrap(),
            request
        );
    }

    #[test]
    fn bodies_by_range_rejects_invalid_count_step_and_overflow() {
        for request in [
            Gov5BodiesByRangeRequest {
                start: 0,
                count: 0,
                step: 1,
            },
            Gov5BodiesByRangeRequest {
                start: 0,
                count: MAX_GOV5_RANGE_BLOCKS + 1,
                step: 1,
            },
            Gov5BodiesByRangeRequest {
                start: 0,
                count: 1,
                step: 0,
            },
            Gov5BodiesByRangeRequest {
                start: 0,
                count: 1,
                step: MAX_GOV5_RANGE_BLOCKS + 1,
            },
            Gov5BodiesByRangeRequest {
                start: 0,
                count: 514,
                step: 2,
            },
            Gov5BodiesByRangeRequest {
                start: u64::MAX,
                count: 2,
                step: 1,
            },
        ] {
            assert!(request.validate().is_err(), "accepted {request:?}");
        }
    }

    #[test]
    fn bodies_by_range_matches_gov5_step_normalization() {
        let request = Gov5BodiesByRangeRequest {
            start: 10,
            count: 2,
            step: 2,
        };
        request.validate().unwrap();

        let (hash_10, block_10) = range_block(10, B256::repeat_byte(9));
        let (_, block_11) = range_block(11, hash_10);
        let expected = [block_10, block_11];
        let stored: Arc<HashMap<u64, Vec<u8>>> =
            Arc::new([(10, expected[0].clone()), (11, expected[1].clone())].into());
        let blocks = Arc::clone(&stored);
        let reader = Gov5CanonicalBlockReader::new(
            || Ok(11),
            move |number| Ok(blocks.get(&number).cloned()),
        );
        let response = Gov5BodiesByRangeResponse::Stream {
            request,
            fork_digest: [1, 2, 3, 4],
            reader,
        };
        let protocol = StreamProtocol::new(GOV5_BODIES_BY_RANGE_PROTOCOL);
        let mut wire = futures::io::Cursor::new(Vec::new());
        futures::executor::block_on(
            Gov5BodiesByRangeCodec.write_response(&protocol, &mut wire, response),
        )
        .unwrap();
        let mut wire = futures::io::Cursor::new(wire.into_inner());
        let Gov5BodiesByRangeResponse::Blocks(decoded) =
            futures::executor::block_on(Gov5BodiesByRangeCodec.read_response(&protocol, &mut wire))
                .unwrap()
        else {
            panic!("expected block response");
        };
        assert_eq!(
            decoded
                .into_iter()
                .map(|chunk| chunk.rlp)
                .collect::<Vec<_>>(),
            expected
        );
    }

    #[test]
    fn bodies_by_range_streams_persistent_blocks_and_checks_continuity() {
        let (_, block_10) = range_block(10, B256::repeat_byte(9));
        let (hash_10, _) = range_block(10, B256::repeat_byte(9));
        let (_, block_11) = range_block(11, hash_10);
        let (hash_11, _) = range_block(11, hash_10);
        let (_, block_12) = range_block(12, hash_11);
        let expected = vec![block_10, block_11, block_12];
        let stored: Arc<HashMap<u64, Vec<u8>>> = Arc::new(
            expected
                .iter()
                .cloned()
                .enumerate()
                .map(|(offset, block)| (10 + offset as u64, block))
                .collect(),
        );
        let blocks = Arc::clone(&stored);
        let reader = Gov5CanonicalBlockReader::new(
            || Ok(12),
            move |number| Ok(blocks.get(&number).cloned()),
        );
        let response = Gov5BodiesByRangeResponse::Stream {
            request: Gov5BodiesByRangeRequest {
                start: 10,
                count: 3,
                step: 1,
            },
            fork_digest: [1, 2, 3, 4],
            reader,
        };
        let protocol = StreamProtocol::new(GOV5_BODIES_BY_RANGE_PROTOCOL);
        let mut wire = futures::io::Cursor::new(Vec::new());
        futures::executor::block_on(
            Gov5BodiesByRangeCodec.write_response(&protocol, &mut wire, response),
        )
        .unwrap();
        let mut wire = futures::io::Cursor::new(wire.into_inner());
        let decoded =
            futures::executor::block_on(Gov5BodiesByRangeCodec.read_response(&protocol, &mut wire))
                .unwrap();
        let Gov5BodiesByRangeResponse::Blocks(decoded) = decoded else {
            panic!("expected block response");
        };
        assert_eq!(decoded.len(), 3);
        assert_eq!(
            decoded
                .into_iter()
                .map(|chunk| chunk.rlp)
                .collect::<Vec<_>>(),
            expected
        );
    }

    #[test]
    fn bodies_by_range_truncates_a_disjoint_canonical_sequence() {
        let (_, block_10) = range_block(10, B256::repeat_byte(9));
        let (_, disjoint_block_11) = range_block(11, B256::repeat_byte(7));
        let stored: Arc<HashMap<u64, Vec<u8>>> =
            Arc::new([(10, block_10.clone()), (11, disjoint_block_11)].into());
        let blocks = Arc::clone(&stored);
        let reader = Gov5CanonicalBlockReader::new(
            || Ok(11),
            move |number| Ok(blocks.get(&number).cloned()),
        );
        let response = Gov5BodiesByRangeResponse::Stream {
            request: Gov5BodiesByRangeRequest {
                start: 10,
                count: 2,
                step: 1,
            },
            fork_digest: [1, 2, 3, 4],
            reader,
        };
        let protocol = StreamProtocol::new(GOV5_BODIES_BY_RANGE_PROTOCOL);
        let mut wire = futures::io::Cursor::new(Vec::new());
        futures::executor::block_on(
            Gov5BodiesByRangeCodec.write_response(&protocol, &mut wire, response),
        )
        .unwrap();
        let mut wire = futures::io::Cursor::new(wire.into_inner());
        let Gov5BodiesByRangeResponse::Blocks(decoded) =
            futures::executor::block_on(Gov5BodiesByRangeCodec.read_response(&protocol, &mut wire))
                .unwrap()
        else {
            panic!("expected block response");
        };
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].rlp, block_10);
    }

    #[test]
    fn bodies_by_range_serves_three_full_persistent_batches() {
        let mut parent = B256::repeat_byte(9);
        let mut stored = HashMap::with_capacity(3 * MAX_GOV5_RANGE_BLOCKS as usize);
        for number in 1..=3 * MAX_GOV5_RANGE_BLOCKS {
            let (hash, block) = range_block(number, parent);
            stored.insert(number, block);
            parent = hash;
        }
        let stored = Arc::new(stored);
        let reader_blocks = Arc::clone(&stored);
        let reader = Gov5CanonicalBlockReader::new(
            || Ok(3 * MAX_GOV5_RANGE_BLOCKS),
            move |number| Ok(reader_blocks.get(&number).cloned()),
        );
        let protocol = StreamProtocol::new(GOV5_BODIES_BY_RANGE_PROTOCOL);

        for batch in 0..3 {
            let start = 1 + batch * MAX_GOV5_RANGE_BLOCKS;
            let response = Gov5BodiesByRangeResponse::Stream {
                request: Gov5BodiesByRangeRequest {
                    start,
                    count: MAX_GOV5_RANGE_BLOCKS,
                    step: 1,
                },
                fork_digest: [1, 2, 3, 4],
                reader: reader.clone(),
            };
            let mut wire = futures::io::Cursor::new(Vec::new());
            futures::executor::block_on(
                Gov5BodiesByRangeCodec.write_response(&protocol, &mut wire, response),
            )
            .unwrap();
            let mut wire = futures::io::Cursor::new(wire.into_inner());
            let Gov5BodiesByRangeResponse::Blocks(decoded) = futures::executor::block_on(
                Gov5BodiesByRangeCodec.read_response(&protocol, &mut wire),
            )
            .unwrap() else {
                panic!("expected block response");
            };
            assert_eq!(decoded.len(), MAX_GOV5_RANGE_BLOCKS as usize);
            assert_eq!(decoded[0].rlp, stored[&start]);
            assert_eq!(
                decoded.last().unwrap().rlp,
                stored[&(start + MAX_GOV5_RANGE_BLOCKS - 1)]
            );
        }
    }

    #[test]
    fn bodies_by_range_supports_blocks_above_the_old_one_mib_limit() {
        let payload = vec![0x42; 2 * 1024 * 1024];
        let response = Gov5BodiesByRangeResponse::Blocks(vec![Gov5RangeBlockChunk {
            fork_digest: [9, 8, 7, 6],
            rlp: payload.clone(),
        }]);
        let protocol = StreamProtocol::new(GOV5_BODIES_BY_RANGE_PROTOCOL);
        let mut wire = futures::io::Cursor::new(Vec::new());
        futures::executor::block_on(
            Gov5BodiesByRangeCodec.write_response(&protocol, &mut wire, response),
        )
        .unwrap();
        let mut wire = futures::io::Cursor::new(wire.into_inner());
        let decoded =
            futures::executor::block_on(Gov5BodiesByRangeCodec.read_response(&protocol, &mut wire))
                .unwrap();
        let Gov5BodiesByRangeResponse::Blocks(decoded) = decoded else {
            panic!("expected block response");
        };
        assert_eq!(decoded[0].rlp, payload);
    }
}
