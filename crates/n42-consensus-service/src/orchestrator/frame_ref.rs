//! Frame-referenced form of the binary execution payload.
//!
//! A leader whose transactions came from ingest frames sends, instead of the
//! transaction bytes, the frames' roots and which positions of each frame the
//! block takes, in block order. A follower that holds those frames
//! (`n42_tx_types::FrameStore`) rebuilds the binary-v1 payload byte for byte.
//! The block hash still commits to the transactions, so a wrong reference list
//! yields a payload the engine refuses; it cannot change what the block is.
//!
//! ```text
//! "N42EPF01" | u32 header_len | u32 tx_count | header (as in binary-v1)
//! | u32 frames | frames × 32-byte frame root
//! | u32 runs   | runs × (u32 frame | u32 start | u32 len)
//! | loose transactions, each u32 len | bytes
//! ```
//! A run with `frame == u32::MAX` takes its `len` transactions from the loose
//! list (transactions the leader found in no frame); `start` is zero there.
//! All integers are little-endian.

use super::{EXECUTION_PAYLOAD_BINARY_HEADER_BYTES, EXECUTION_PAYLOAD_BINARY_V1_MAGIC};
use alloy_primitives::{B256, map::B256HashMap};
use n42_tx_types::FrameStore;
use rayon::prelude::*;

/// First bytes of a frame-referenced payload.
pub const EXECUTION_PAYLOAD_FRAME_REF_MAGIC: [u8; 8] = *b"N42EPF01";

const LOOSE: u32 = u32::MAX;
/// Upper bound on a block's transaction count accepted from the wire.
const MAX_TRANSACTIONS: u32 = 8_000_000;

/// Why a frame-referenced payload could not be put back together.
#[derive(Debug, PartialEq, Eq)]
pub enum FrameRefError {
    /// The bytes are not a well-formed frame-referenced payload.
    Malformed(&'static str),
    /// This node does not hold `missing` of the `frames` frames named.
    Missing { missing: usize, frames: usize },
}

impl std::fmt::Display for FrameRefError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(reason) => write!(f, "malformed frame-referenced payload: {reason}"),
            Self::Missing { missing, frames } => {
                write!(f, "{missing} of {frames} referenced frames are not held")
            }
        }
    }
}

/// Whether `payload` is in the frame-referenced form.
pub fn is_frame_ref_payload(payload: &[u8]) -> bool {
    payload.starts_with(&EXECUTION_PAYLOAD_FRAME_REF_MAGIC)
}

/// What [`encode_frame_ref_payload`] produced.
pub struct FrameRefPayload {
    pub bytes: Vec<u8>,
    pub frames: usize,
    pub runs: usize,
    pub referenced: usize,
    pub loose: usize,
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8], FrameRefError> {
        if self.0.len() < len {
            return Err(FrameRefError::Malformed("truncated"));
        }
        let (head, tail) = self.0.split_at(len);
        self.0 = tail;
        Ok(head)
    }

    fn u32(&mut self) -> Result<u32, FrameRefError> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }
}

/// Rewrites a binary-v1 payload as frame references against `store`. `None`
/// when the payload is not binary-v1 or none of its transactions is in a
/// frame this node holds.
pub fn encode_frame_ref_payload(binary_v1: &[u8], store: &FrameStore) -> Option<FrameRefPayload> {
    if !binary_v1.starts_with(&EXECUTION_PAYLOAD_BINARY_V1_MAGIC) {
        return None;
    }
    let mut reader = Reader(&binary_v1[EXECUTION_PAYLOAD_BINARY_V1_MAGIC.len()..]);
    let header_len = reader.u32().ok()?;
    let tx_count = reader.u32().ok()?;
    let header = reader.take(header_len as usize).ok()?;
    let mut transactions = Vec::with_capacity(tx_count as usize);
    for _ in 0..tx_count {
        let len = reader.u32().ok()?;
        transactions.push(reader.take(len as usize).ok()?);
    }
    let hashes: Vec<B256> = transactions
        .par_iter()
        .map(alloy_primitives::keccak256)
        .collect();
    let located = store.locate_all(&hashes);

    let mut roots: Vec<B256> = Vec::new();
    let mut root_index: B256HashMap<u32> = B256HashMap::default();
    // (frame, start, len)
    let mut runs: Vec<(u32, u32, u32)> = Vec::new();
    let mut loose_bytes = 0usize;
    let mut loose = 0usize;
    for (transaction, place) in transactions.iter().zip(&located) {
        let (frame, position) = match place {
            Some((root, position)) => {
                let frame = *root_index.entry(*root).or_insert_with(|| {
                    roots.push(*root);
                    (roots.len() - 1) as u32
                });
                (frame, *position)
            }
            None => {
                loose += 1;
                loose_bytes += 4 + transaction.len();
                (LOOSE, 0)
            }
        };
        match runs.last_mut() {
            Some((last_frame, _, len)) if *last_frame == LOOSE && frame == LOOSE => *len += 1,
            Some((last_frame, start, len))
                if *last_frame == frame && frame != LOOSE && *start + *len == position =>
            {
                *len += 1
            }
            _ => runs.push((frame, position, 1)),
        }
    }
    let referenced = transactions.len() - loose;
    if referenced == 0 {
        return None;
    }

    let mut bytes = Vec::with_capacity(
        EXECUTION_PAYLOAD_BINARY_HEADER_BYTES
            + header.len()
            + 8
            + roots.len() * 32
            + runs.len() * 12
            + loose_bytes,
    );
    bytes.extend_from_slice(&EXECUTION_PAYLOAD_FRAME_REF_MAGIC);
    bytes.extend_from_slice(&header_len.to_le_bytes());
    bytes.extend_from_slice(&tx_count.to_le_bytes());
    bytes.extend_from_slice(header);
    bytes.extend_from_slice(&(roots.len() as u32).to_le_bytes());
    for root in &roots {
        bytes.extend_from_slice(root.as_slice());
    }
    bytes.extend_from_slice(&(runs.len() as u32).to_le_bytes());
    for (frame, start, len) in &runs {
        bytes.extend_from_slice(&frame.to_le_bytes());
        bytes.extend_from_slice(&start.to_le_bytes());
        bytes.extend_from_slice(&len.to_le_bytes());
    }
    for (transaction, place) in transactions.iter().zip(&located) {
        if place.is_none() {
            bytes.extend_from_slice(&(transaction.len() as u32).to_le_bytes());
            bytes.extend_from_slice(transaction);
        }
    }
    Some(FrameRefPayload {
        bytes,
        frames: roots.len(),
        runs: runs.len(),
        referenced,
        loose,
    })
}

/// Rebuilds the binary-v1 payload a frame-referenced payload stands for.
pub fn expand_frame_ref_payload(payload: &[u8], store: &FrameStore) -> Result<Vec<u8>, FrameRefError> {
    if !is_frame_ref_payload(payload) {
        return Err(FrameRefError::Malformed("wrong magic"));
    }
    let mut reader = Reader(&payload[EXECUTION_PAYLOAD_FRAME_REF_MAGIC.len()..]);
    let header_len = reader.u32()?;
    let tx_count = reader.u32()?;
    if tx_count > MAX_TRANSACTIONS {
        return Err(FrameRefError::Malformed("transaction count too large"));
    }
    let header = reader.take(header_len as usize)?;
    let frame_count = reader.u32()?;
    if frame_count > tx_count {
        return Err(FrameRefError::Malformed("more frames than transactions"));
    }
    let roots: Vec<B256> = reader
        .take(frame_count as usize * 32)?
        .chunks_exact(32)
        .map(B256::from_slice)
        .collect();
    let run_count = reader.u32()?;
    if run_count > tx_count {
        return Err(FrameRefError::Malformed("more runs than transactions"));
    }
    let mut runs = Vec::with_capacity(run_count as usize);
    for _ in 0..run_count {
        runs.push((reader.u32()?, reader.u32()?, reader.u32()?));
    }

    let frames = store.get_all(&roots);
    let missing = frames.iter().filter(|frame| frame.is_none()).count();
    if missing > 0 {
        return Err(FrameRefError::Missing {
            missing,
            frames: roots.len(),
        });
    }
    let frames: Vec<_> = frames.into_iter().flatten().collect();

    // Check every run and size the output before writing anything.
    let mut total = 0u64;
    let mut referenced_bytes = 0usize;
    let mut loose_count = 0u64;
    for &(frame, start, len) in &runs {
        if len == 0 {
            return Err(FrameRefError::Malformed("empty run"));
        }
        total += u64::from(len);
        if frame == LOOSE {
            loose_count += u64::from(len);
            continue;
        }
        let stored = frames
            .get(frame as usize)
            .ok_or(FrameRefError::Malformed("run names an unknown frame"))?;
        let end = start
            .checked_add(len)
            .filter(|end| *end as usize <= stored.transactions.len())
            .ok_or(FrameRefError::Malformed("run leaves its frame"))?;
        referenced_bytes += stored.transactions[start as usize..end as usize]
            .iter()
            .map(|transaction| 4 + transaction.len())
            .sum::<usize>();
    }
    if total != u64::from(tx_count) {
        return Err(FrameRefError::Malformed("runs do not cover the block"));
    }
    if loose_count > reader.0.len() as u64 / 4 {
        return Err(FrameRefError::Malformed("loose transactions truncated"));
    }

    let mut out = Vec::with_capacity(
        EXECUTION_PAYLOAD_BINARY_HEADER_BYTES + header.len() + referenced_bytes + reader.0.len(),
    );
    out.extend_from_slice(&EXECUTION_PAYLOAD_BINARY_V1_MAGIC);
    out.extend_from_slice(&header_len.to_le_bytes());
    out.extend_from_slice(&tx_count.to_le_bytes());
    out.extend_from_slice(header);
    for &(frame, start, len) in &runs {
        if frame == LOOSE {
            for _ in 0..len {
                let transaction_len = reader.u32()?;
                out.extend_from_slice(&transaction_len.to_le_bytes());
                out.extend_from_slice(reader.take(transaction_len as usize)?);
            }
        } else {
            let stored = &frames[frame as usize];
            for transaction in &stored.transactions[start as usize..(start + len) as usize] {
                out.extend_from_slice(&(transaction.len() as u32).to_le_bytes());
                out.extend_from_slice(transaction);
            }
        }
    }
    if !reader.0.is_empty() {
        return Err(FrameRefError::Malformed("trailing bytes"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Bytes;

    fn frame(seed: u8, count: u8) -> (B256, Vec<Bytes>, Vec<B256>) {
        let transactions: Vec<Bytes> = (0..count)
            .map(|i| Bytes::from(vec![0x50, seed, i, seed ^ i]))
            .collect();
        let hashes: Vec<B256> = transactions
            .iter()
            .map(alloy_primitives::keccak256)
            .collect();
        (n42_tx_types::frame_root(&hashes), transactions, hashes)
    }

    fn binary_v1(header: &[u8], transactions: &[Bytes]) -> Vec<u8> {
        let mut bytes = EXECUTION_PAYLOAD_BINARY_V1_MAGIC.to_vec();
        bytes.extend_from_slice(&(header.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&(transactions.len() as u32).to_le_bytes());
        bytes.extend_from_slice(header);
        for transaction in transactions {
            bytes.extend_from_slice(&(transaction.len() as u32).to_le_bytes());
            bytes.extend_from_slice(transaction);
        }
        bytes
    }

    #[test]
    fn a_block_cut_across_frames_with_loose_transactions_round_trips() {
        let leader = FrameStore::new(8);
        let follower = FrameStore::new(8);
        let (root_a, txs_a, hashes_a) = frame(1, 5);
        let (root_b, txs_b, hashes_b) = frame(2, 4);
        for store in [&leader, &follower] {
            store.insert(root_a, txs_a.clone(), hashes_a.clone());
            store.insert(root_b, txs_b.clone(), hashes_b.clone());
        }
        let loose = Bytes::from_static(&[0x02, 0xaa, 0xbb]);
        // The tail of frame A (its head went into an earlier block), a loose
        // transaction, frame B out of order, then frame A's first entry.
        let mut body = txs_a[2..].to_vec();
        body.push(loose.clone());
        body.extend([txs_b[1].clone(), txs_b[0].clone(), txs_b[2].clone(), txs_b[3].clone()]);
        body.push(txs_a[0].clone());
        let full = binary_v1(br#"{"header":1}"#, &body);

        let encoded = encode_frame_ref_payload(&full, &leader).unwrap();
        assert_eq!((encoded.frames, encoded.loose, encoded.referenced), (2, 1, 8));
        // A[2..5], loose, B[1], B[0], B[2..4], A[0]
        assert_eq!(encoded.runs, 6);
        assert!(is_frame_ref_payload(&encoded.bytes));
        assert!(encoded.bytes.len() < full.len() + 100);
        assert_eq!(expand_frame_ref_payload(&encoded.bytes, &follower).unwrap(), full);

        // A follower without one frame reports it and builds nothing.
        let partial = FrameStore::new(8);
        partial.insert(root_a, txs_a, hashes_a);
        assert_eq!(
            expand_frame_ref_payload(&encoded.bytes, &partial),
            Err(FrameRefError::Missing { missing: 1, frames: 2 })
        );
    }

    #[test]
    fn a_payload_with_no_known_frame_is_left_alone_and_damage_is_refused() {
        let store = FrameStore::new(8);
        let (root, txs, hashes) = frame(3, 3);
        let full = binary_v1(b"{}", &txs);
        assert!(encode_frame_ref_payload(&full, &store).is_none());
        assert!(encode_frame_ref_payload(b"{\"json\":true}", &store).is_none());

        store.insert(root, txs, hashes);
        let encoded = encode_frame_ref_payload(&full, &store).unwrap().bytes;
        assert_eq!(expand_frame_ref_payload(&encoded, &store).unwrap(), full);
        for cut in 0..encoded.len() {
            assert!(expand_frame_ref_payload(&encoded[..cut], &store).is_err());
        }
        // A run reaching past its frame, and runs that do not add up.
        let run_len = encoded.len() - 4;
        let mut long = encoded.clone();
        long[run_len..].copy_from_slice(&4u32.to_le_bytes());
        assert!(matches!(
            expand_frame_ref_payload(&long, &store),
            Err(FrameRefError::Malformed(_))
        ));
        let mut short = encoded.clone();
        short[run_len..].copy_from_slice(&2u32.to_le_bytes());
        assert_eq!(
            expand_frame_ref_payload(&short, &store),
            Err(FrameRefError::Malformed("runs do not cover the block"))
        );
        let mut trailing = encoded;
        trailing.push(0);
        assert!(expand_frame_ref_payload(&trailing, &store).is_err());
    }
}
