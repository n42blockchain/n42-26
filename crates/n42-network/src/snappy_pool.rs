//! Pooled Snappy codecs.
//!
//! `snap::raw::Encoder::new()` allocates its hash table and
//! `snap::write::FrameEncoder::new()` / `snap::read::FrameDecoder::new()`
//! additionally allocate two block-sized scratch buffers (~140 KiB) per call.
//! The gov5 range path builds one frame per served block and the gossip path
//! one raw block per message, so every call paid that allocation. The raw
//! encoder and decoder are kept per thread here, and the frame format is
//! written and read directly on top of them.
//!
//! The frame writer is byte-identical to `snap::write::FrameEncoder` fed one
//! `write_all` followed by `into_inner` (see `frame_encode_matches_snap`), so
//! peers running either implementation interoperate unchanged.

use std::cell::RefCell;
use std::io;

/// Snappy frame stream identifier chunk.
const STREAM_IDENTIFIER: &[u8] = b"\xFF\x06\x00\x00sNaPpY";
const STREAM_BODY: &[u8] = b"sNaPpY";
/// Largest uncompressed block a frame chunk may carry.
const MAX_BLOCK_SIZE: usize = 1 << 16;
/// `max_compress_len(MAX_BLOCK_SIZE)`: largest compressed chunk body.
const MAX_COMPRESS_BLOCK_SIZE: usize = 76_490;

thread_local! {
    static RAW_ENCODER: RefCell<Option<snap::raw::Encoder>> = const { RefCell::new(None) };
    static RAW_DECODER: RefCell<Option<snap::raw::Decoder>> = const { RefCell::new(None) };
}

fn with_encoder<T>(f: impl FnOnce(&mut snap::raw::Encoder) -> T) -> T {
    RAW_ENCODER.with(|slot| {
        let mut slot = slot.borrow_mut();
        f(slot.get_or_insert_with(snap::raw::Encoder::new))
    })
}

fn with_decoder<T>(f: impl FnOnce(&mut snap::raw::Decoder) -> T) -> T {
    RAW_DECODER.with(|slot| {
        let mut slot = slot.borrow_mut();
        f(slot.get_or_insert_with(snap::raw::Decoder::new))
    })
}

/// Raw (unframed) Snappy compression with a per-thread encoder.
pub fn raw_compress(input: &[u8]) -> snap::Result<Vec<u8>> {
    with_encoder(|encoder| encoder.compress_vec(input))
}

/// Raw (unframed) Snappy decompression with a per-thread decoder. Callers
/// bound the output with `snap::raw::decompress_len` first, as before.
pub fn raw_decompress(input: &[u8]) -> snap::Result<Vec<u8>> {
    with_decoder(|decoder| decoder.decompress_vec(input))
}

/// Encode `payload` in the Snappy frame format, byte-identical to
/// `snap::write::FrameEncoder`. An empty payload produces an empty frame,
/// exactly as the `FrameEncoder` writes nothing for it.
pub fn frame_encode(payload: &[u8]) -> io::Result<Vec<u8>> {
    if payload.is_empty() {
        return Ok(Vec::new());
    }
    let block_count = payload.len().div_ceil(MAX_BLOCK_SIZE);
    let mut out =
        Vec::with_capacity(STREAM_IDENTIFIER.len() + payload.len() + 8 * block_count + 32);
    out.extend_from_slice(STREAM_IDENTIFIER);
    with_encoder(|encoder| {
        for block in payload.chunks(MAX_BLOCK_SIZE) {
            let checksum = crc32c_masked(block);
            let header_at = out.len();
            out.extend_from_slice(&[0u8; 8]);
            let body_at = out.len();
            out.resize(body_at + snap::raw::max_compress_len(block.len()), 0);
            let compressed_len = encoder
                .compress(block, &mut out[body_at..])
                .map_err(io::Error::other)?;
            // Same trade-off as snap: keep the block raw unless compression
            // saved at least an eighth of it.
            let (chunk_type, chunk_len) = if compressed_len >= block.len() - block.len() / 8 {
                out.truncate(body_at);
                out.extend_from_slice(block);
                (0x01u8, 4 + block.len())
            } else {
                out.truncate(body_at + compressed_len);
                (0x00u8, 4 + compressed_len)
            };
            out[header_at] = chunk_type;
            out[header_at + 1..header_at + 4]
                .copy_from_slice(&(chunk_len as u32).to_le_bytes()[..3]);
            out[header_at + 4..header_at + 8].copy_from_slice(&checksum.to_le_bytes());
        }
        Ok::<(), io::Error>(())
    })?;
    Ok(out)
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// Decode a complete Snappy frame into at most `max_decoded` bytes.
///
/// Mirrors `snap::read::FrameDecoder` (stream identifier first, per-chunk
/// CRC-32C, chunk size limits, skippable and reserved chunk types) and fails
/// as soon as the decoded length would exceed `max_decoded`, the way the
/// former `.take(max + 1)` readers did, without decoding the excess first.
pub fn frame_decode(frame: &[u8], max_decoded: usize) -> io::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(max_decoded.min(frame.len().saturating_mul(4)));
    let mut position = 0usize;
    let mut saw_stream_identifier = false;
    with_decoder(|decoder| {
        while position < frame.len() {
            let header = frame
                .get(position..position + 4)
                .ok_or_else(|| invalid("truncated Snappy chunk header"))?;
            let chunk_type = header[0];
            let chunk_len = usize::from(header[1])
                | (usize::from(header[2]) << 8)
                | (usize::from(header[3]) << 16);
            let body = frame
                .get(position + 4..position + 4 + chunk_len)
                .ok_or_else(|| invalid("truncated Snappy chunk body"))?;
            position += 4 + chunk_len;
            match chunk_type {
                0xff => {
                    if body != STREAM_BODY {
                        return Err(invalid("invalid Snappy stream identifier"));
                    }
                    saw_stream_identifier = true;
                }
                _ if !saw_stream_identifier => {
                    return Err(invalid("Snappy stream does not start with an identifier"));
                }
                0x00 => {
                    if !(4..=MAX_COMPRESS_BLOCK_SIZE + 4).contains(&chunk_len) {
                        return Err(invalid("invalid compressed Snappy chunk length"));
                    }
                    let checksum = u32::from_le_bytes(body[..4].try_into().expect("four bytes"));
                    let compressed = &body[4..];
                    let decoded_len =
                        snap::raw::decompress_len(compressed).map_err(io::Error::other)?;
                    if decoded_len > MAX_BLOCK_SIZE {
                        return Err(invalid("Snappy chunk decodes beyond the block size"));
                    }
                    if out.len() + decoded_len > max_decoded {
                        return Err(invalid("Snappy payload exceeds decoded size limit"));
                    }
                    let start = out.len();
                    out.resize(start + decoded_len, 0);
                    let written = decoder
                        .decompress(compressed, &mut out[start..])
                        .map_err(io::Error::other)?;
                    if written != decoded_len {
                        return Err(invalid("Snappy chunk decoded length mismatch"));
                    }
                    if crc32c_masked(&out[start..]) != checksum {
                        return Err(invalid("Snappy chunk checksum mismatch"));
                    }
                }
                0x01 => {
                    if !(4..=MAX_BLOCK_SIZE + 4).contains(&chunk_len) {
                        return Err(invalid("invalid uncompressed Snappy chunk length"));
                    }
                    let checksum = u32::from_le_bytes(body[..4].try_into().expect("four bytes"));
                    let data = &body[4..];
                    if out.len() + data.len() > max_decoded {
                        return Err(invalid("Snappy payload exceeds decoded size limit"));
                    }
                    if crc32c_masked(data) != checksum {
                        return Err(invalid("Snappy chunk checksum mismatch"));
                    }
                    out.extend_from_slice(data);
                }
                0x02..=0x7f => {
                    return Err(invalid("reserved unskippable Snappy chunk"));
                }
                0x80..=0xfe => {}
            }
        }
        Ok::<(), io::Error>(())
    })?;
    Ok(out)
}

/// Masked CRC-32C as the Snappy frame format defines it.
fn crc32c_masked(buf: &[u8]) -> u32 {
    let sum = crc32c(buf);
    (sum.wrapping_shr(15) | sum.wrapping_shl(17)).wrapping_add(0xA282_EAD8)
}

fn crc32c(buf: &[u8]) -> u32 {
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("sse4.2") {
            // SAFETY: the feature check above guarantees SSE4.2 is available.
            return unsafe { crc32c_sse42(buf) };
        }
    }
    crc32c_table(buf)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse4.2")]
unsafe fn crc32c_sse42(buf: &[u8]) -> u32 {
    use std::arch::x86_64::{_mm_crc32_u8, _mm_crc32_u64};
    let mut crc = !0u32;
    let (words, rest) = buf.as_chunks::<8>();
    for word in words {
        crc = _mm_crc32_u64(u64::from(crc), u64::from_le_bytes(*word)) as u32;
    }
    for &byte in rest {
        crc = _mm_crc32_u8(crc, byte);
    }
    !crc
}

const fn crc32c_table_init() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut index = 0;
    while index < 256 {
        let mut crc = index as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0x82F6_3B78
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[index] = crc;
        index += 1;
    }
    table
}

static CRC32C_TABLE: [u32; 256] = crc32c_table_init();

fn crc32c_table(buf: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in buf {
        crc = CRC32C_TABLE[((crc ^ u32::from(byte)) & 0xff) as usize] ^ (crc >> 8);
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    /// Deterministic block-like bytes: runs of repeated values mixed with
    /// noise, so frames contain both compressed and stored chunks.
    pub(crate) fn sample(len: usize, seed: u64) -> Vec<u8> {
        let mut state = seed ^ 0x9E37_79B9_7F4A_7C15;
        let mut out = Vec::with_capacity(len);
        while out.len() < len {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let run = (state % 64) as usize + 1;
            let byte = (state >> 32) as u8;
            if state & 1 == 0 {
                out.extend(std::iter::repeat_n(byte, run.min(len - out.len())));
            } else {
                for offset in 0..run.min(len - out.len()) {
                    out.push(
                        byte.wrapping_mul(31)
                            .wrapping_add(offset as u8 ^ (state as u8)),
                    );
                }
            }
        }
        out
    }

    fn noise(len: usize, seed: u64) -> Vec<u8> {
        let mut state = seed | 1;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 24) as u8
            })
            .collect()
    }

    fn snap_frame(payload: &[u8]) -> Vec<u8> {
        let mut encoder = snap::write::FrameEncoder::new(Vec::new());
        encoder.write_all(payload).unwrap();
        encoder.into_inner().unwrap()
    }

    fn snap_unframe(frame: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        snap::read::FrameDecoder::new(frame)
            .read_to_end(&mut out)
            .unwrap();
        out
    }

    #[test]
    fn crc32c_matches_known_vectors() {
        // RFC 3720 / Castagnoli check value for "123456789".
        assert_eq!(crc32c_table(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c(&[]), 0);
        for len in [0usize, 1, 7, 8, 9, 15, 16, 17, 1000, 65_536, 65_537] {
            let data = noise(len, len as u64 + 1);
            assert_eq!(crc32c(&data), crc32c_table(&data), "len {len}");
        }
    }

    #[test]
    fn frame_encode_matches_snap() {
        for len in [
            0usize,
            1,
            2,
            100,
            4_095,
            65_535,
            65_536,
            65_537,
            131_072,
            200_000,
            1 << 20,
            3 << 20,
        ] {
            for (label, payload) in [
                ("sample", sample(len, len as u64)),
                ("noise", noise(len, len as u64)),
                ("zeros", vec![0u8; len]),
            ] {
                let ours = frame_encode(&payload).unwrap();
                let theirs = snap_frame(&payload);
                assert!(
                    ours == theirs,
                    "{label} len {len}: frame differs ({} vs {} bytes)",
                    ours.len(),
                    theirs.len()
                );
                assert_eq!(
                    frame_decode(&ours, len).unwrap(),
                    payload,
                    "{label} len {len}"
                );
                if len > 0 {
                    assert_eq!(snap_unframe(&ours), payload, "{label} len {len}");
                }
            }
        }
    }

    #[test]
    fn frame_encode_handles_the_twelve_mebibyte_range_chunk() {
        let payload = sample(12 << 20, 12);
        let ours = frame_encode(&payload).unwrap();
        assert_eq!(ours, snap_frame(&payload));
        assert_eq!(frame_decode(&ours, payload.len()).unwrap(), payload);
    }

    #[test]
    fn frame_decode_rejects_corruption_and_over_length() {
        let payload = sample(200_000, 7);
        let frame = frame_encode(&payload).unwrap();

        assert!(frame_decode(&frame, payload.len() - 1).is_err());
        assert_eq!(frame_decode(&frame, payload.len()).unwrap(), payload);

        let mut corrupt = frame.clone();
        // Flip a byte inside the first chunk body (after the 10-byte stream
        // identifier and the 8-byte chunk header).
        corrupt[24] ^= 0x55;
        assert!(frame_decode(&corrupt, payload.len()).is_err());

        let mut checksum_corrupt = frame.clone();
        checksum_corrupt[14] ^= 0x01;
        assert!(frame_decode(&checksum_corrupt, payload.len()).is_err());

        let without_identifier = &frame[10..];
        assert!(frame_decode(without_identifier, payload.len()).is_err());

        let truncated = &frame[..frame.len() - 1];
        assert!(frame_decode(truncated, payload.len()).is_err());

        let mut reserved = frame.clone();
        reserved[10] = 0x02;
        assert!(frame_decode(&reserved, payload.len()).is_err());
    }

    #[test]
    fn raw_pool_round_trips() {
        let payload = sample(100_000, 3);
        let compressed = raw_compress(&payload).unwrap();
        assert_eq!(
            compressed,
            snap::raw::Encoder::new().compress_vec(&payload).unwrap()
        );
        assert_eq!(raw_decompress(&compressed).unwrap(), payload);
    }

    /// Timing only. `cargo test -p n42-network snappy_pool_bench --release -- --ignored --nocapture`
    #[test]
    #[ignore = "measurement, not a correctness gate"]
    fn snappy_pool_bench() {
        for (label, len) in [
            ("1 KiB", 1usize << 10),
            ("16 KiB", 16usize << 10),
            ("1 MiB", 1usize << 20),
            ("12 MiB", 12usize << 20),
        ] {
            let payload = sample(len, len as u64);
            let iterations = if len > (4 << 20) {
                10
            } else if len >= (1 << 20) {
                50
            } else {
                2_000
            };
            let frame = snap_frame(&payload);

            let started = std::time::Instant::now();
            for _ in 0..iterations {
                std::hint::black_box(snap_frame(&payload));
            }
            let old_encode = started.elapsed() / iterations;
            let started = std::time::Instant::now();
            for _ in 0..iterations {
                std::hint::black_box(frame_encode(&payload).unwrap());
            }
            let new_encode = started.elapsed() / iterations;

            let started = std::time::Instant::now();
            for _ in 0..iterations {
                let mut out = Vec::with_capacity(len);
                snap::read::FrameDecoder::new(frame.as_slice())
                    .take(len as u64 + 1)
                    .read_to_end(&mut out)
                    .unwrap();
                std::hint::black_box(out);
            }
            let old_decode = started.elapsed() / iterations;
            let started = std::time::Instant::now();
            for _ in 0..iterations {
                std::hint::black_box(frame_decode(&frame, len).unwrap());
            }
            let new_decode = started.elapsed() / iterations;

            let raw = snap::raw::Encoder::new().compress_vec(&payload).unwrap();
            let started = std::time::Instant::now();
            for _ in 0..iterations {
                std::hint::black_box(snap::raw::Encoder::new().compress_vec(&payload).unwrap());
            }
            let old_raw_encode = started.elapsed() / iterations;
            let started = std::time::Instant::now();
            for _ in 0..iterations {
                std::hint::black_box(raw_compress(&payload).unwrap());
            }
            let new_raw_encode = started.elapsed() / iterations;
            let started = std::time::Instant::now();
            for _ in 0..iterations {
                std::hint::black_box(snap::raw::Decoder::new().decompress_vec(&raw).unwrap());
            }
            let old_raw_decode = started.elapsed() / iterations;
            let started = std::time::Instant::now();
            for _ in 0..iterations {
                std::hint::black_box(raw_decompress(&raw).unwrap());
            }
            let new_raw_decode = started.elapsed() / iterations;

            println!(
                "snappy {label} ({} -> {} framed bytes, {iterations} iterations):\n  frame encode {old_encode:?} -> {new_encode:?}\n  frame decode {old_decode:?} -> {new_decode:?}\n  raw encode   {old_raw_encode:?} -> {new_raw_encode:?}\n  raw decode   {old_raw_decode:?} -> {new_raw_decode:?}",
                payload.len(),
                frame.len()
            );
        }
    }
}
