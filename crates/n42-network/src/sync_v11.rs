//! N42 sync standard v1.1 request-response protocols (N42-SYNC-SPEC.md §5):
//!
//! - `/rpc/bodies_by_range/2/ssz_snappy` (§5.1): committed canonical blocks or
//!   headers in a requester byte budget, closed by exactly one END trailer;
//! - `/rpc/commit_qc/1/ssz_snappy` (§5.2): a CommitQC plus the header chain
//!   from its block down to a requested height, the anchor for a tail.
//!
//! Framing is the v1 one (§1.2): a request is `uvarint(len) || snappy(payload)`;
//! a response chunk is `code || [context 4] || uvarint(len) || snappy(payload)`,
//! with the context bytes (`genesis_hash[..4]`) only on SUCCESS chunks of
//! `bodies_by_range/2`. Integers are little-endian; fixed requests are plain
//! concatenations. Every decoder checks lengths before it allocates.

use crate::gov5_rpc::{
    FRAMED_PAYLOAD_OVER_LIMIT, GOV5_RANGE_BATCH_BLOCKS, Gov5CanonicalBlockReader,
    Gov5RangeBlockChunk, MAX_GOV5_RANGE_BLOCK_SIZE, MAX_GOV5_RANGE_BLOCKS,
    MAX_GOV5_RANGE_WIRE_SIZE, encode_framed_payload, encode_range_chunk, encode_range_error,
    read_framed_payload,
};
use crate::h2_wire::{H2QuorumCertificate, decode_h2_qc, encode_h2_qc};
use alloy_primitives::{B256, keccak256};
use alloy_rlp::Header as RlpHeader;
use futures::prelude::*;
use libp2p::{StreamProtocol, request_response};
use std::fmt;
use std::io;

/// Ranges v2 (§5.1). Servers register it next to `/1`.
pub const GOV5_BODIES_BY_RANGE_V2_PROTOCOL: &str = "/rpc/bodies_by_range/2/ssz_snappy";
/// The commit-QC anchor protocol (§5.2).
pub const GOV5_COMMIT_QC_PROTOCOL: &str = "/rpc/commit_qc/1/ssz_snappy";

/// Result codes of the v1.1 protocols (§5.0). v1 keeps codes 0-2 only.
pub mod result_code {
    pub const SUCCESS: u8 = 0x00;
    pub const INVALID_REQUEST: u8 = 0x01;
    pub const SERVER_ERROR: u8 = 0x02;
    pub const RESOURCE_UNAVAILABLE: u8 = 0x03;
    pub const RATE_LIMITED: u8 = 0x04;
    pub const END: u8 = 0x10;
}

/// END reasons (§5.0).
pub mod end_reason {
    pub const COMPLETE: u8 = 0;
    pub const BYTE_BUDGET: u8 = 1;
    pub const COUNT_CAP: u8 = 2;
    pub const CANONICAL_END: u8 = 3;
    pub const NOT_AVAILABLE: u8 = 4;
    pub const RATE_LIMITED: u8 = 5;
    pub const SHUTTING_DOWN: u8 = 6;
}

/// `flags` bit 0: serve RLP headers instead of whole blocks.
pub const RANGE_FLAG_HEADERS_ONLY: u64 = 1;
/// Fixed request size of `bodies_by_range/2`.
pub const RANGE_V2_REQUEST_LEN: usize = 32;
/// Fixed size of the END trailer payload.
pub const RANGE_END_LEN: usize = 17;
/// The byte budget a server applies when the requester sends `max_bytes = 0`.
pub const RANGE_V2_DEFAULT_BYTES: u64 = 64 << 20;
/// This server's cap on one reply (the standard allows up to 128 MiB).
pub const RANGE_V2_SERVER_CAP: u64 = 128 << 20;
/// What this client reads of one reply. It always requests `max_bytes = 0`
/// (the 64 MiB server default), so a conforming reply never carries more: a
/// first block alone is at most 64 MiB, and any further block must fit the
/// same budget. More is a provable fault, refused from its declared length.
pub const RANGE_V2_CLIENT_BUDGET: usize = RANGE_V2_DEFAULT_BYTES as usize;
/// `retry_after_ms` assumed for a RATE_LIMITED refusal without a readable one.
const DEFAULT_RETRY_AFTER: std::time::Duration = std::time::Duration::from_secs(1);
/// The longest `retry_after_ms` honoured; a larger one is clamped.
const MAX_RETRY_AFTER: std::time::Duration = std::time::Duration::from_secs(60);
/// One header (HEADERS_ONLY, commit_qc) is at most 64 KiB (§6.1).
pub const MAX_SYNC_HEADER_BYTES: usize = 64 << 10;
/// A QC wire is at most 2048 bytes (§6.1).
pub const MAX_QC_WIRE_BYTES: usize = 2048;
/// At most 256 headers in one commit_qc reply (§5.2).
pub const MAX_COMMIT_QC_HEADERS: usize = 256;
/// Fixed request size of `commit_qc/1`.
pub const COMMIT_QC_REQUEST_LEN: usize = 40;
/// The largest commit_qc SUCCESS payload a decoder accepts.
pub const MAX_COMMIT_QC_PAYLOAD: usize =
    4 + MAX_QC_WIRE_BYTES + 2 + MAX_COMMIT_QC_HEADERS * (4 + MAX_SYNC_HEADER_BYTES);
const MAX_COMMIT_QC_WIRE: usize = MAX_COMMIT_QC_PAYLOAD + MAX_COMMIT_QC_PAYLOAD / 6 + 1024;
/// Error chunk messages are short UTF-8 strings.
const MAX_ERROR_MESSAGE: usize = 1024;
/// Requests are at most 1 MiB framed (§6.1); these fixed ones are far smaller.
const MAX_REQUEST_WIRE: usize = 1024;
/// Headers above the requested height a commit_qc server reads to find the
/// lowest QC naming a block within reach: the QC naming the last reachable
/// block can sit in a header some views above it.
const COMMIT_QC_SCAN: u64 = MAX_COMMIT_QC_HEADERS as u64 + 64;
/// Headers below the committed head searched for the highest embedded QC.
const COMMIT_QC_HIGHEST_SCAN: u64 = 64;

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn le_u64(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(bytes.try_into().expect("eight bytes"))
}

/// Runs storage work on the blocking pool when a Tokio runtime is present;
/// codec unit tests drive the future without one and run it inline.
async fn run_blocking<T, F>(work: F) -> io::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => handle.spawn_blocking(work).await.map_err(io::Error::other),
        Err(_) => Ok(work()),
    }
}

// ---------------------------------------------------------------------------
// Headers
// ---------------------------------------------------------------------------

/// The header (first item) of a block in the gov5 wire form, as raw bytes.
pub fn header_rlp_of_block(block: &[u8]) -> Option<&[u8]> {
    let mut payload = block;
    let outer = RlpHeader::decode(&mut payload).ok()?;
    if !outer.list || outer.payload_length != payload.len() {
        return None;
    }
    let item = payload;
    let mut cursor = item;
    let header = RlpHeader::decode(&mut cursor).ok()?;
    let prefix = item.len().checked_sub(cursor.len())?;
    item.get(..prefix.checked_add(header.payload_length)?)
}

/// The fields sync needs from one header in either wire layout (gov5 native
/// or Profile 1). The hash is keccak of the exact bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SyncHeader {
    pub number: u64,
    pub parent: B256,
    pub hash: B256,
    pub extra: alloy_primitives::Bytes,
}

pub fn decode_sync_header(raw: &[u8]) -> Option<SyncHeader> {
    if raw.len() > MAX_SYNC_HEADER_BYTES {
        return None;
    }
    let header = n42_consensus::Gov5NativeHeader::decode(raw)
        .ok()
        .filter(|native| native.encode() == raw)
        .map(|native| native.header)
        .or_else(|| {
            n42_chainspec::profile1::Profile1Header::decode(raw)
                .ok()
                .map(|profile1| profile1.header)
        })?;
    Some(SyncHeader {
        number: header.number,
        parent: header.parent_hash,
        hash: keccak256(raw),
        extra: header.extra_data,
    })
}

/// The QC embedded in a header `Extra`, if any (view 0 counts as none).
fn header_extra_qc(extra: &[u8]) -> Option<H2QuorumCertificate> {
    crate::gov5_block::decode_gov5_header_extra(extra)
        .ok()?
        .qc
        .filter(|qc| qc.view != 0)
}

// ---------------------------------------------------------------------------
// bodies_by_range/2 wire
// ---------------------------------------------------------------------------

/// `start u64 || count u64 || max_bytes u64 || flags u64` (32 bytes).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RangeV2Request {
    pub start: u64,
    pub count: u64,
    /// 0 = the server default (64 MiB).
    pub max_bytes: u64,
    pub flags: u64,
}

impl RangeV2Request {
    pub fn encode(&self) -> [u8; RANGE_V2_REQUEST_LEN] {
        let mut out = [0u8; RANGE_V2_REQUEST_LEN];
        out[..8].copy_from_slice(&self.start.to_le_bytes());
        out[8..16].copy_from_slice(&self.count.to_le_bytes());
        out[16..24].copy_from_slice(&self.max_bytes.to_le_bytes());
        out[24..].copy_from_slice(&self.flags.to_le_bytes());
        out
    }

    pub fn decode(encoded: &[u8]) -> io::Result<Self> {
        if encoded.len() != RANGE_V2_REQUEST_LEN {
            return Err(invalid_data(format!(
                "bodies_by_range/2 request is {} bytes, expected 32",
                encoded.len()
            )));
        }
        Ok(Self {
            start: le_u64(&encoded[..8]),
            count: le_u64(&encoded[8..16]),
            max_bytes: le_u64(&encoded[16..24]),
            flags: le_u64(&encoded[24..]),
        })
    }

    /// The §5.1 bounds; the message is the INVALID_REQUEST text.
    pub fn validate(&self) -> Result<(), String> {
        if self.count == 0 || self.count > MAX_GOV5_RANGE_BLOCKS {
            return Err(format!("range count {} outside 1..1024", self.count));
        }
        if self.flags & !RANGE_FLAG_HEADERS_ONLY != 0 {
            return Err(format!("reserved range flag bits set: {:#x}", self.flags));
        }
        if self.start.checked_add(self.count).is_none() {
            return Err("range end overflows u64".to_owned());
        }
        Ok(())
    }

    pub const fn headers_only(&self) -> bool {
        self.flags & RANGE_FLAG_HEADERS_ONLY != 0
    }

    /// `min(max_bytes or 64 MiB, server cap)`.
    pub fn byte_budget(&self) -> u64 {
        let requested = if self.max_bytes == 0 {
            RANGE_V2_DEFAULT_BYTES
        } else {
            self.max_bytes
        };
        requested.min(RANGE_V2_SERVER_CAP)
    }
}

/// END trailer: `next u64 || served_bytes u64 || reason u8` (17 bytes).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RangeEnd {
    /// The first number not served.
    pub next: u64,
    /// The sum of the decoded payload lengths sent.
    pub served_bytes: u64,
    pub reason: u8,
}

impl RangeEnd {
    pub fn encode(&self) -> [u8; RANGE_END_LEN] {
        let mut out = [0u8; RANGE_END_LEN];
        out[..8].copy_from_slice(&self.next.to_le_bytes());
        out[8..16].copy_from_slice(&self.served_bytes.to_le_bytes());
        out[16] = self.reason;
        out
    }

    pub fn decode(encoded: &[u8]) -> io::Result<Self> {
        if encoded.len() != RANGE_END_LEN {
            return Err(invalid_data("range END is not 17 bytes"));
        }
        Ok(Self {
            next: le_u64(&encoded[..8]),
            served_bytes: le_u64(&encoded[8..16]),
            reason: encoded[16],
        })
    }
}

fn encode_end_chunk(end: &RangeEnd) -> io::Result<Vec<u8>> {
    let mut out = vec![result_code::END];
    out.extend_from_slice(&encode_framed_payload(&end.encode(), RANGE_END_LEN)?);
    Ok(out)
}

/// What a requester read from one `/2` stream. Nothing here is trusted yet;
/// [`check_range_v2_reply`] judges it against the request.
#[derive(Clone, Debug, Default)]
pub struct RangeV2Reply {
    pub chunks: Vec<Gov5RangeBlockChunk>,
    pub end: Option<RangeEnd>,
    /// An error chunk (code, message), which ends the stream.
    pub error: Option<(u8, String)>,
    /// A provable wire violation seen while reading (§3.5).
    pub violation: Option<&'static str>,
}

/// A `/2` response: a reply read from the wire, a single error chunk, or a
/// server stream from canonical storage.
#[derive(Clone)]
pub enum RangeV2Response {
    Reply(RangeV2Reply),
    Error {
        code: u8,
        message: String,
    },
    Stream {
        request: RangeV2Request,
        fork_digest: [u8; 4],
        reader: Gov5CanonicalBlockReader,
        /// The server's committed head when the request was accepted.
        committed_head: u64,
    },
}

impl fmt::Debug for RangeV2Response {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Reply(reply) => formatter.debug_tuple("Reply").field(reply).finish(),
            Self::Error { code, message } => formatter
                .debug_struct("Error")
                .field("code", code)
                .field("message", message)
                .finish(),
            Self::Stream {
                request,
                committed_head,
                ..
            } => formatter
                .debug_struct("Stream")
                .field("request", request)
                .field("committed_head", committed_head)
                .finish_non_exhaustive(),
        }
    }
}

/// The server's answer to a decoded `/2` request (after access checks):
/// INVALID_REQUEST, SERVER_ERROR when the head cannot be read, else a stream.
/// The head is [`Gov5CanonicalBlockReader::committed_head`]: the storage head
/// can be an uncommitted block built on a prepare-certified parent.
pub fn range_v2_response(
    request: RangeV2Request,
    fork_digest: [u8; 4],
    reader: Gov5CanonicalBlockReader,
) -> RangeV2Response {
    if let Err(message) = request.validate() {
        return RangeV2Response::Error {
            code: result_code::INVALID_REQUEST,
            message,
        };
    }
    match reader.committed_head() {
        Ok(committed_head) => RangeV2Response::Stream {
            request,
            fork_digest,
            reader,
            committed_head,
        },
        Err(error) => RangeV2Response::Error {
            code: result_code::SERVER_ERROR,
            message: format!("canonical head lookup failed: {error}"),
        },
    }
}

/// How a `/2` server batch ended its part of the stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum V2Stop {
    /// Close with an END of this reason.
    End(u8),
    /// An error chunk was written before any block; no END follows.
    Error,
}

struct V2Batch {
    frames: Vec<Vec<u8>>,
    last_hash: Option<B256>,
    next: u64,
    served_bytes: u64,
    stop: Option<V2Stop>,
}

/// Reads, checks and encodes one batch of a `/2` reply. A storage or decode
/// failure before the first block is an error chunk; after it the reply ends
/// with NOT_AVAILABLE. A block that would push `served_bytes` above `budget`
/// ends it with BYTE_BUDGET, except the first block of the reply.
fn build_range_v2_batch(
    reader: &Gov5CanonicalBlockReader,
    numbers: std::ops::RangeInclusive<u64>,
    fork_digest: [u8; 4],
    headers_only: bool,
    mut previous_hash: Option<B256>,
    mut served_bytes: u64,
    budget: u64,
) -> io::Result<V2Batch> {
    let start = *numbers.start();
    let expected = (numbers.end() - start + 1) as usize;
    let mut frames = Vec::new();
    let mut next = start;
    let fail = |served: bool, frames: &mut Vec<Vec<u8>>, message: &str| -> io::Result<V2Stop> {
        if served {
            Ok(V2Stop::End(end_reason::NOT_AVAILABLE))
        } else {
            frames.push(encode_range_error(result_code::SERVER_ERROR, message)?);
            Ok(V2Stop::Error)
        }
    };
    let read = if headers_only {
        reader.header_rlps_by_range(numbers)
    } else {
        reader.block_rlps_by_range(numbers)
    };
    let payloads = match read {
        Ok(payloads) => payloads,
        Err(error) => {
            let stop = fail(previous_hash.is_some(), &mut frames, &error)?;
            return Ok(V2Batch {
                frames,
                last_hash: previous_hash,
                next,
                served_bytes,
                stop: Some(stop),
            });
        }
    };
    let available = payloads.len().min(expected);
    let mut stop = None;
    for (offset, payload) in payloads.into_iter().take(expected).enumerate() {
        let number = start + offset as u64;
        let served = previous_hash.is_some();
        let limit = if headers_only {
            MAX_SYNC_HEADER_BYTES
        } else {
            MAX_GOV5_RANGE_BLOCK_SIZE
        };
        if payload.len() > limit {
            stop = Some(fail(
                served,
                &mut frames,
                "canonical payload exceeds its chunk limit",
            )?);
            break;
        }
        let decoded = if headers_only {
            decode_sync_header(&payload).map(|header| (header.number, header.parent, header.hash))
        } else {
            crate::gov5_block::decode_wire_block_rlp(&payload)
                .ok()
                .map(|block| {
                    (
                        block.header.number,
                        block.header.parent_hash,
                        block.block_hash,
                    )
                })
        };
        let Some((decoded_number, parent, hash)) = decoded else {
            stop = Some(fail(
                served,
                &mut frames,
                "canonical payload does not decode",
            )?);
            break;
        };
        if decoded_number != number {
            stop = Some(fail(
                served,
                &mut frames,
                "canonical block number mismatch",
            )?);
            break;
        }
        if previous_hash.is_some_and(|previous| parent != previous) {
            // The canonical chain moved under the read: end the linked prefix.
            stop = Some(V2Stop::End(end_reason::NOT_AVAILABLE));
            break;
        }
        let length = payload.len() as u64;
        if served && served_bytes.saturating_add(length) > budget {
            stop = Some(V2Stop::End(end_reason::BYTE_BUDGET));
            break;
        }
        frames.push(encode_range_chunk(&Gov5RangeBlockChunk {
            fork_digest,
            rlp: payload,
        })?);
        served_bytes = served_bytes.saturating_add(length);
        previous_hash = Some(hash);
        next = number + 1;
    }
    if stop.is_none() && available < expected {
        // A committed height this node does not hold (below its earliest
        // body, or pruned): zero or more blocks, then NOT_AVAILABLE.
        stop = Some(V2Stop::End(end_reason::NOT_AVAILABLE));
    }
    Ok(V2Batch {
        frames,
        last_hash: previous_hash,
        next,
        served_bytes,
        stop,
    })
}

async fn write_range_v2_stream<T>(
    io: &mut T,
    request: RangeV2Request,
    fork_digest: [u8; 4],
    reader: Gov5CanonicalBlockReader,
    committed_head: u64,
) -> io::Result<()>
where
    T: AsyncWrite + Unpin + Send,
{
    if let Err(message) = request.validate() {
        return io
            .write_all(&encode_range_error(result_code::INVALID_REQUEST, &message)?)
            .await;
    }
    let requested_end = request.start + request.count;
    let budget = request.byte_budget();
    let headers_only = request.headers_only();
    let mut next = request.start;
    let mut served_bytes = 0u64;
    let mut last_hash = None;
    let mut stop = None;
    if request.start <= committed_head {
        let last = (requested_end - 1).min(committed_head);
        while next <= last {
            let numbers = next..=last.min(next.saturating_add(GOV5_RANGE_BATCH_BLOCKS - 1));
            let batch_reader = reader.clone();
            let batch = run_blocking(move || {
                build_range_v2_batch(
                    &batch_reader,
                    numbers,
                    fork_digest,
                    headers_only,
                    last_hash,
                    served_bytes,
                    budget,
                )
            })
            .await??;
            for frame in &batch.frames {
                io.write_all(frame).await?;
            }
            next = batch.next;
            served_bytes = batch.served_bytes;
            last_hash = batch.last_hash;
            if batch.stop.is_some() {
                stop = batch.stop;
                break;
            }
        }
    }
    let reason = match stop {
        Some(V2Stop::Error) => return Ok(()),
        Some(V2Stop::End(reason)) => reason,
        None if next == requested_end => end_reason::COMPLETE,
        // The committed head was reached (or the start lies above it).
        None => end_reason::CANONICAL_END,
    };
    io.write_all(&encode_end_chunk(&RangeEnd {
        next,
        served_bytes,
        reason,
    })?)
    .await
}

/// Reads one `/2` reply within `budget` decoded bytes. Wire violations are
/// recorded, not returned as errors, so the caller can score them; a stream
/// that breaks off leaves `end` unset (a soft fault).
async fn read_range_v2_reply<T>(io: &mut T, budget: usize) -> RangeV2Reply
where
    T: AsyncRead + Unpin + Send,
{
    let mut reply = RangeV2Reply::default();
    let mut decoded = 0usize;
    loop {
        let mut code = [0u8; 1];
        match io.read(&mut code).await {
            Ok(1) => {}
            // EOF or a broken stream: no END.
            _ => return reply,
        }
        match code[0] {
            result_code::SUCCESS => {
                if reply.chunks.len() >= MAX_GOV5_RANGE_BLOCKS as usize {
                    reply.violation = Some("more than 1024 range chunks");
                    return reply;
                }
                let mut fork_digest = [0u8; 4];
                if io.read_exact(&mut fork_digest).await.is_err() {
                    return reply;
                }
                let remaining = budget.saturating_sub(decoded);
                match read_framed_payload(
                    io,
                    MAX_GOV5_RANGE_WIRE_SIZE,
                    MAX_GOV5_RANGE_BLOCK_SIZE.min(remaining),
                )
                .await
                {
                    Ok(rlp) => {
                        decoded = decoded.saturating_add(rlp.len());
                        reply.chunks.push(Gov5RangeBlockChunk { fork_digest, rlp });
                    }
                    Err(error) if error.to_string() == FRAMED_PAYLOAD_OVER_LIMIT => {
                        reply.violation = Some("declared length above the cap");
                        return reply;
                    }
                    Err(_) => return reply,
                }
            }
            result_code::END => {
                match read_framed_payload(io, MAX_REQUEST_WIRE, RANGE_END_LEN)
                    .await
                    .and_then(|payload| RangeEnd::decode(&payload))
                {
                    Ok(end) => reply.end = Some(end),
                    Err(_) => {
                        reply.violation = Some("malformed END");
                        return reply;
                    }
                }
                let mut trailing = [0u8; 1];
                if matches!(io.read(&mut trailing).await, Ok(n) if n > 0) {
                    reply.violation = Some("data after END");
                }
                return reply;
            }
            code => {
                let message = read_framed_payload(io, MAX_REQUEST_WIRE * 2, MAX_ERROR_MESSAGE)
                    .await
                    .map(|message| String::from_utf8_lossy(&message).into_owned())
                    .unwrap_or_default();
                reply.error = Some((code, message));
                return reply;
            }
        }
    }
}

/// A requester's verdict on one `/2` reply (§5.1 client MUSTs, §3.5).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RangeV2Verdict {
    /// A consistent reply: the payloads in order (possibly none).
    Payloads(Vec<Vec<u8>>),
    /// An error chunk before the first block.
    Refused { code: u8, message: String },
    /// A soft fault (missing END, an error after blocks): back off, no score.
    Soft(&'static str),
    /// Legitimately nothing served: zero blocks with END reason 3-6, or a
    /// RATE_LIMITED refusal. Neither a fault nor a reason to back off
    /// (sections 3.5, 6.3); `retry_after` is the server's request to wait.
    Unavailable {
        reason: u8,
        retry_after: Option<std::time::Duration>,
    },
    /// A provable fault: score the peer.
    Fault(&'static str),
}

pub fn check_range_v2_reply(
    request: &RangeV2Request,
    reply: RangeV2Reply,
    fork_digest: [u8; 4],
) -> RangeV2Verdict {
    if let Some(violation) = reply.violation {
        return RangeV2Verdict::Fault(violation);
    }
    let served = reply.chunks.len() as u64;
    if served > request.count {
        return RangeV2Verdict::Fault("more chunks than requested");
    }
    if reply
        .chunks
        .iter()
        .any(|chunk| chunk.fork_digest != fork_digest)
    {
        return RangeV2Verdict::Fault("foreign context bytes");
    }
    if let Some((code, message)) = reply.error {
        return if reply.chunks.is_empty() && code == result_code::RATE_LIMITED {
            RangeV2Verdict::Unavailable {
                reason: end_reason::RATE_LIMITED,
                retry_after: Some(
                    message
                        .trim()
                        .parse::<u64>()
                        .map(std::time::Duration::from_millis)
                        .unwrap_or(DEFAULT_RETRY_AFTER)
                        .min(MAX_RETRY_AFTER),
                ),
            }
        } else if reply.chunks.is_empty() {
            RangeV2Verdict::Refused { code, message }
        } else {
            RangeV2Verdict::Soft("error chunk after blocks")
        };
    }
    let Some(end) = reply.end else {
        return RangeV2Verdict::Soft("missing END");
    };
    let requested_end = request.start.saturating_add(request.count);
    if Some(end.next) != request.start.checked_add(served) {
        return RangeV2Verdict::Fault("END next does not match the chunks");
    }
    let decoded: u64 = reply
        .chunks
        .iter()
        .map(|chunk| chunk.rlp.len() as u64)
        .sum();
    if end.served_bytes != decoded {
        return RangeV2Verdict::Fault("END served_bytes does not match the chunks");
    }
    if end.reason > end_reason::SHUTTING_DOWN {
        return RangeV2Verdict::Fault("unknown END reason");
    }
    if (end.reason == end_reason::COMPLETE) != (end.next == requested_end) {
        return RangeV2Verdict::Fault("END reason contradicts next");
    }
    if served == 0
        && matches!(
            end.reason,
            end_reason::COMPLETE | end_reason::BYTE_BUDGET | end_reason::COUNT_CAP
        )
    {
        return RangeV2Verdict::Fault("empty reply with a non-terminal END reason");
    }
    // Only the first block may exceed the requested budget on its own.
    if served > 1 && decoded > request.byte_budget() {
        return RangeV2Verdict::Fault("reply exceeds the requested byte budget");
    }
    if served == 0 {
        return RangeV2Verdict::Unavailable {
            reason: end.reason,
            retry_after: (end.reason == end_reason::RATE_LIMITED).then_some(DEFAULT_RETRY_AFTER),
        };
    }
    RangeV2Verdict::Payloads(reply.chunks.into_iter().map(|chunk| chunk.rlp).collect())
}

#[derive(Clone, Debug, Default)]
pub struct Gov5BodiesByRangeV2Codec;

impl request_response::Codec for Gov5BodiesByRangeV2Codec {
    type Protocol = StreamProtocol;
    type Request = RangeV2Request;
    type Response = RangeV2Response;

    async fn read_request<T>(&mut self, _: &Self::Protocol, io: &mut T) -> io::Result<Self::Request>
    where
        T: AsyncRead + Unpin + Send,
    {
        let encoded = read_framed_payload(io, MAX_REQUEST_WIRE, RANGE_V2_REQUEST_LEN).await?;
        RangeV2Request::decode(&encoded)
    }

    async fn read_response<T>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
    ) -> io::Result<Self::Response>
    where
        T: AsyncRead + Unpin + Send,
    {
        let budget = RANGE_V2_CLIENT_BUDGET;
        Ok(RangeV2Response::Reply(
            read_range_v2_reply(io, budget).await,
        ))
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
        request
            .validate()
            .map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))?;
        io.write_all(&encode_framed_payload(
            &request.encode(),
            RANGE_V2_REQUEST_LEN,
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
            RangeV2Response::Error { code, message } => {
                io.write_all(&encode_range_error(code, &message)?).await?;
            }
            RangeV2Response::Stream {
                request,
                fork_digest,
                reader,
                committed_head,
            } => {
                write_range_v2_stream(io, request, fork_digest, reader, committed_head).await?;
            }
            RangeV2Response::Reply(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "a read range reply cannot be served",
                ));
            }
        }
        io.close().await
    }
}

// ---------------------------------------------------------------------------
// commit_qc/1
// ---------------------------------------------------------------------------

/// `number u64 || hash Bytes32` (40 bytes). `(0, 0)` asks for the server's
/// highest CommitQC.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommitQcRequest {
    pub number: u64,
    /// Zero: any block at `number`.
    pub hash: B256,
}

impl CommitQcRequest {
    pub fn encode(&self) -> [u8; COMMIT_QC_REQUEST_LEN] {
        let mut out = [0u8; COMMIT_QC_REQUEST_LEN];
        out[..8].copy_from_slice(&self.number.to_le_bytes());
        out[8..].copy_from_slice(self.hash.as_slice());
        out
    }

    pub fn decode(encoded: &[u8]) -> io::Result<Self> {
        if encoded.len() != COMMIT_QC_REQUEST_LEN {
            return Err(invalid_data(format!(
                "commit_qc request is {} bytes, expected 40",
                encoded.len()
            )));
        }
        Ok(Self {
            number: le_u64(&encoded[..8]),
            hash: B256::from_slice(&encoded[8..]),
        })
    }

    /// The `(0, 0)` "your highest" form.
    pub fn is_highest(&self) -> bool {
        self.number == 0 && self.hash == B256::ZERO
    }
}

/// `qc_len u32 || QC wire || header_count u16 || header_count x (u32 len || RLP header)`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitQcPayload {
    pub qc_wire: Vec<u8>,
    /// From the QC's block downwards.
    pub headers: Vec<Vec<u8>>,
}

impl CommitQcPayload {
    pub fn encode(&self) -> io::Result<Vec<u8>> {
        if self.qc_wire.len() > MAX_QC_WIRE_BYTES
            || self.headers.len() > MAX_COMMIT_QC_HEADERS
            || self
                .headers
                .iter()
                .any(|header| header.len() > MAX_SYNC_HEADER_BYTES)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "commit_qc payload exceeds its bounds",
            ));
        }
        let mut out = Vec::with_capacity(
            6 + self.qc_wire.len() + self.headers.iter().map(|h| 4 + h.len()).sum::<usize>(),
        );
        out.extend_from_slice(&(self.qc_wire.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.qc_wire);
        out.extend_from_slice(&(self.headers.len() as u16).to_le_bytes());
        for header in &self.headers {
            out.extend_from_slice(&(header.len() as u32).to_le_bytes());
            out.extend_from_slice(header);
        }
        Ok(out)
    }

    pub fn decode(encoded: &[u8]) -> io::Result<Self> {
        let mut cursor = encoded;
        let mut take = |len: usize| -> io::Result<&[u8]> {
            if cursor.len() < len {
                return Err(invalid_data("commit_qc payload is truncated"));
            }
            let (head, rest) = cursor.split_at(len);
            cursor = rest;
            Ok(head)
        };
        let qc_len = u32::from_le_bytes(take(4)?.try_into().expect("four bytes")) as usize;
        if qc_len > MAX_QC_WIRE_BYTES {
            return Err(invalid_data("commit_qc QC exceeds 2048 bytes"));
        }
        let qc_wire = take(qc_len)?.to_vec();
        let count = u16::from_le_bytes(take(2)?.try_into().expect("two bytes")) as usize;
        if count > MAX_COMMIT_QC_HEADERS {
            return Err(invalid_data("commit_qc carries more than 256 headers"));
        }
        let mut headers = Vec::with_capacity(count);
        for _ in 0..count {
            let len = u32::from_le_bytes(take(4)?.try_into().expect("four bytes")) as usize;
            if len > MAX_SYNC_HEADER_BYTES {
                return Err(invalid_data("commit_qc header exceeds 64 KiB"));
            }
            headers.push(take(len)?.to_vec());
        }
        if !cursor.is_empty() {
            return Err(invalid_data("commit_qc payload has trailing bytes"));
        }
        Ok(Self { qc_wire, headers })
    }
}

/// Where a commit_qc server finds what it holds: the committed chain and,
/// when known, the persisted committed QC (wire form).
#[derive(Clone)]
pub struct CommitQcSource {
    pub reader: Gov5CanonicalBlockReader,
    pub persisted_qc: Option<Vec<u8>>,
    /// The requesting peer, for the serve log line (none in tests).
    pub requester: Option<libp2p::PeerId>,
}

impl fmt::Debug for CommitQcSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CommitQcSource")
            .field("persisted_qc", &self.persisted_qc.as_ref().map(Vec::len))
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug)]
pub enum CommitQcResponse {
    Found(CommitQcPayload),
    Error {
        code: u8,
        message: String,
    },
    /// Server side: answered from `source` on a blocking thread.
    Lookup {
        request: CommitQcRequest,
        source: Box<CommitQcSource>,
    },
}

fn unavailable(message: &str) -> CommitQcResponse {
    CommitQcResponse::Error {
        code: result_code::RESOURCE_UNAVAILABLE,
        message: message.to_owned(),
    }
}

fn server_error(error: String) -> CommitQcResponse {
    CommitQcResponse::Error {
        code: result_code::SERVER_ERROR,
        message: error,
    }
}

/// One held CommitQC: its block's number and the QC.
struct HeldQc {
    number: u64,
    qc: H2QuorumCertificate,
}

/// Committed headers `from..=to`, decoded, stopping at the first gap or
/// parent-link break.
fn read_sync_headers(
    reader: &Gov5CanonicalBlockReader,
    from: u64,
    to: u64,
) -> Result<Vec<(SyncHeader, Vec<u8>)>, String> {
    if from > to {
        return Ok(Vec::new());
    }
    let mut headers: Vec<(SyncHeader, Vec<u8>)> = Vec::new();
    for raw in reader.header_rlps_by_range(from..=to)? {
        let Some(header) = decode_sync_header(&raw) else {
            break;
        };
        let expected = from + headers.len() as u64;
        let linked = headers
            .last()
            .is_none_or(|(previous, _)| header.parent == previous.hash);
        if header.number != expected || !linked {
            break;
        }
        headers.push((header, raw));
    }
    Ok(headers)
}

/// QCs held in `headers` (their `Extra`) and the persisted one, each paired
/// with the number of the block it names within `headers`.
fn held_qcs(
    headers: &[(SyncHeader, Vec<u8>)],
    persisted: Option<&H2QuorumCertificate>,
) -> Vec<HeldQc> {
    let number_of = |hash: B256| {
        headers
            .iter()
            .find(|(header, _)| header.hash == hash)
            .map(|(header, _)| header.number)
    };
    headers
        .iter()
        .filter_map(|(header, _)| header_extra_qc(&header.extra))
        .chain(persisted.cloned())
        .filter_map(|qc| {
            Some(HeldQc {
                number: number_of(qc.block_hash)?,
                qc,
            })
        })
        .collect()
}

fn found(qc: &H2QuorumCertificate, headers: Vec<Vec<u8>>) -> CommitQcResponse {
    match encode_h2_qc(qc) {
        Ok(qc_wire) if qc_wire.len() <= MAX_QC_WIRE_BYTES => {
            CommitQcResponse::Found(CommitQcPayload { qc_wire, headers })
        }
        _ => server_error("held QC does not encode".to_owned()),
    }
}

/// A held QC and, when read, its block's header.
type HighestQc = (H2QuorumCertificate, Option<Vec<u8>>);

/// The highest CommitQC held near the committed head, with its block's header.
fn highest_held(
    reader: &Gov5CanonicalBlockReader,
    head: u64,
    persisted: Option<&H2QuorumCertificate>,
) -> Result<Option<HighestQc>, String> {
    let lowest = head.saturating_sub(COMMIT_QC_HIGHEST_SCAN - 1);
    let mut headers = read_sync_headers(reader, lowest, head)?;
    if headers
        .last()
        .is_none_or(|(header, _)| header.number != head)
    {
        // Fewer headers are held below the head than the scan spans (a node
        // that joined at a pivot): walk down from the head instead.
        headers.clear();
        let mut number = head;
        loop {
            let Some(header) = reader
                .header_rlps_by_range(number..=number)?
                .into_iter()
                .next()
                .and_then(|raw| Some((decode_sync_header(&raw)?, raw)))
                .filter(|(header, _)| header.number == number)
            else {
                break;
            };
            if headers
                .last()
                .is_some_and(|(above, _): &(SyncHeader, Vec<u8>)| above.parent != header.0.hash)
            {
                break;
            }
            headers.push(header);
            if number == lowest {
                break;
            }
            number -= 1;
        }
    }
    let mut best: Option<H2QuorumCertificate> = None;
    for qc in headers
        .iter()
        .filter_map(|(header, _)| header_extra_qc(&header.extra))
        .chain(persisted.cloned())
    {
        if best.as_ref().is_none_or(|held| qc.view > held.view) {
            best = Some(qc);
        }
    }
    Ok(best.map(|qc| {
        let header = headers
            .iter()
            .find(|(header, _)| header.hash == qc.block_hash)
            .map(|(_, raw)| raw.clone());
        (qc, header)
    }))
}

/// §5.2 server: the lowest CommitQC held that names the requested block or a
/// descendant, with the headers from its block down to `number`; if that QC
/// is more than 256 headers away, the highest QC held with no headers.
/// "Held" is the persisted committed QC plus the QCs in the `Extra` of the
/// committed chain.
pub fn serve_commit_qc(source: &CommitQcSource, request: CommitQcRequest) -> CommitQcResponse {
    let reader = &source.reader;
    let persisted = source
        .persisted_qc
        .as_deref()
        .and_then(|wire| decode_h2_qc(wire).ok())
        .filter(|qc| qc.view != 0);
    let head = match reader.committed_head() {
        Ok(head) => head,
        Err(error) => return server_error(format!("canonical head lookup failed: {error}")),
    };
    if request.number > head {
        return unavailable("number is not committed");
    }
    if request.is_highest() {
        return match highest_held(reader, head, persisted.as_ref()) {
            Ok(Some((qc, header))) => found(&qc, header.into_iter().collect()),
            Ok(None) => unavailable("no commit QC held"),
            Err(error) => server_error(error),
        };
    }
    let headers = match read_sync_headers(
        reader,
        request.number,
        head.min(request.number.saturating_add(COMMIT_QC_SCAN)),
    ) {
        Ok(headers) => headers,
        Err(error) => return server_error(error),
    };
    let Some((first, _)) = headers.first() else {
        return unavailable("unknown number");
    };
    if request.hash != B256::ZERO && first.hash != request.hash {
        return unavailable("unknown hash");
    }
    // Lowest named block first; among QCs naming it, the first one found.
    let lowest = held_qcs(&headers, persisted.as_ref())
        .into_iter()
        .min_by_key(|held| held.number);
    if let Some(held) = lowest
        && held.number - request.number < MAX_COMMIT_QC_HEADERS as u64
    {
        let offset = (held.number - request.number) as usize;
        let chain = headers[..=offset]
            .iter()
            .rev()
            .map(|(_, raw)| raw.clone())
            .collect();
        return found(&held.qc, chain);
    }
    match highest_held(reader, head, persisted.as_ref()) {
        Ok(Some((qc, _))) => found(&qc, Vec::new()),
        Ok(None) => unavailable("no commit QC held"),
        Err(error) => server_error(error),
    }
}

/// What a checked commit_qc reply proves once the caller has verified `qc`
/// (§3.3): `qc` anchors `qc_block`, and `chain` (descending, from the QC's
/// block) links it down to the requested height.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitQcAnchor {
    pub qc: H2QuorumCertificate,
    /// `(number, hash)` of the QC's block, when the reply carries headers.
    pub qc_block: Option<(u64, B256)>,
    /// `(number, hash)` of every header served, from the QC's block down.
    pub chain: Vec<(u64, B256)>,
}

/// §5.2 client, structural part: the QC decodes, the headers hash-chain from
/// the QC's block down to `(number, hash)`. The signature check (§3.3) is the
/// caller's: it needs the validator set of the QC's epoch. An `Err` is a
/// provable fault of the server.
pub fn check_commit_qc_reply(
    request: &CommitQcRequest,
    payload: &CommitQcPayload,
) -> Result<CommitQcAnchor, &'static str> {
    let qc = decode_h2_qc(&payload.qc_wire).map_err(|_| "QC does not decode")?;
    if qc.view == 0 || qc.block_hash == B256::ZERO {
        return Err("QC is the genesis sentinel");
    }
    if payload.headers.len() > MAX_COMMIT_QC_HEADERS {
        return Err("more than 256 headers");
    }
    let mut chain: Vec<(u64, B256)> = Vec::with_capacity(payload.headers.len());
    let mut expected_hash = qc.block_hash;
    let mut previous_number = None;
    for raw in &payload.headers {
        let header = decode_sync_header(raw).ok_or("header does not decode")?;
        if header.hash != expected_hash {
            return Err("header chain does not link to the QC");
        }
        if previous_number.is_some_and(|number: u64| header.number + 1 != number) {
            return Err("header numbers are not contiguous");
        }
        previous_number = Some(header.number);
        expected_hash = header.parent;
        chain.push((header.number, header.hash));
    }
    if !request.is_highest() {
        // No headers: the anchor is further away than 256 headers (a bare QC).
        if let Some((number, hash)) = chain.last() {
            if *number != request.number {
                return Err("header chain does not reach the requested number");
            }
            if request.hash != B256::ZERO && *hash != request.hash {
                return Err("header chain names another block at the requested number");
            }
        }
    } else if chain.len() > 1 {
        return Err("a highest reply carries only the QC's block");
    }
    Ok(CommitQcAnchor {
        qc_block: chain.first().copied(),
        qc,
        chain,
    })
}

#[derive(Clone, Debug, Default)]
pub struct Gov5CommitQcCodec;

impl request_response::Codec for Gov5CommitQcCodec {
    type Protocol = StreamProtocol;
    type Request = CommitQcRequest;
    type Response = CommitQcResponse;

    async fn read_request<T>(&mut self, _: &Self::Protocol, io: &mut T) -> io::Result<Self::Request>
    where
        T: AsyncRead + Unpin + Send,
    {
        let encoded = read_framed_payload(io, MAX_REQUEST_WIRE, COMMIT_QC_REQUEST_LEN).await?;
        CommitQcRequest::decode(&encoded)
    }

    async fn read_response<T>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
    ) -> io::Result<Self::Response>
    where
        T: AsyncRead + Unpin + Send,
    {
        let mut code = [0u8; 1];
        io.read_exact(&mut code).await?;
        if code[0] == result_code::SUCCESS {
            let payload =
                read_framed_payload(io, MAX_COMMIT_QC_WIRE, MAX_COMMIT_QC_PAYLOAD).await?;
            return Ok(CommitQcResponse::Found(CommitQcPayload::decode(&payload)?));
        }
        let message = read_framed_payload(io, MAX_REQUEST_WIRE * 2, MAX_ERROR_MESSAGE)
            .await
            .map(|message| String::from_utf8_lossy(&message).into_owned())
            .unwrap_or_default();
        Ok(CommitQcResponse::Error {
            code: code[0],
            message,
        })
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
        io.write_all(&encode_framed_payload(
            &request.encode(),
            COMMIT_QC_REQUEST_LEN,
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
        let response = match response {
            CommitQcResponse::Lookup { request, source } => {
                let requester = source.requester;
                let served = run_blocking(move || serve_commit_qc(&source, request)).await?;
                log_commit_qc_served(requester, &request, &served);
                served
            }
            response => response,
        };
        match response {
            CommitQcResponse::Found(payload) => {
                let mut out = vec![result_code::SUCCESS];
                out.extend_from_slice(&encode_framed_payload(
                    &payload.encode()?,
                    MAX_COMMIT_QC_PAYLOAD,
                )?);
                io.write_all(&out).await?;
            }
            CommitQcResponse::Error { code, message } => {
                io.write_all(&encode_range_error(code, &message)?).await?;
            }
            CommitQcResponse::Lookup { .. } => unreachable!("resolved above"),
        }
        io.close().await
    }
}

/// One info line per served `commit_qc/1` request, as for range serving.
fn log_commit_qc_served(
    peer: Option<libp2p::PeerId>,
    request: &CommitQcRequest,
    response: &CommitQcResponse,
) {
    let peer = peer.map(|peer| peer.to_string()).unwrap_or_default();
    match response {
        CommitQcResponse::Found(payload) => tracing::info!(
            target: "n42::interop::range",
            %peer,
            number = request.number,
            hash = %request.hash,
            qc_view = decode_h2_qc(&payload.qc_wire).ok().map(|qc| qc.view),
            headers = payload.headers.len(),
            "serving commit_qc/1"
        ),
        CommitQcResponse::Error { code, message } => tracing::info!(
            target: "n42::interop::range",
            %peer,
            number = request.number,
            hash = %request.hash,
            code,
            %message,
            "commit_qc/1 refused"
        ),
        CommitQcResponse::Lookup { .. } => {}
    }
}

#[cfg(test)]
mod tests;
