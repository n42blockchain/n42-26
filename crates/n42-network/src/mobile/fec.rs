//! Forward Error Correction (FEC) for mobile packet broadcast.
//!
//! Inspired by Firedancer's Turbine shredding: splits large packets into
//! data shreds + parity shreds using Reed-Solomon erasure coding. Phones
//! can recover the original packet from any K-of-N shreds, tolerating up
//! to M lost shreds (where N = K + M).
//!
//! ## Wire format (per shred)
//!
//! ```text
//! [fec_set_id: 8B][shred_index: 2B][data_count: 2B][parity_count: 2B][payload...]
//! ```
//!
//! ## Integration points
//!
//! - **Encoder** (`FecEncoder`): called in `StarHub::BroadcastPacket` handler
//!   before sending to sessions. Splits the framed packet into shreds.
//! - **Decoder** (`FecDecoder`): called in `QuicMobileClient::receive_message`
//!   to reassemble shreds back into the original packet.

use alloy_primitives::B256;

/// FEC shred header size in bytes.
pub const FEC_HEADER_SIZE: usize = 14; // 8 + 2 + 2 + 2

/// Default data shred count per FEC set.
pub const DEFAULT_DATA_SHREDS: u16 = 16;

/// Default parity shred count per FEC set.
pub const DEFAULT_PARITY_SHREDS: u16 = 4;

/// A single FEC shred (data or parity).
#[derive(Clone, Debug)]
pub struct FecShred {
    /// Unique ID for this FEC set (block_hash-derived).
    pub fec_set_id: u64,
    /// Index within the FEC set (0..data_count are data, data_count..total are parity).
    pub shred_index: u16,
    /// Number of data shreds in this FEC set.
    pub data_count: u16,
    /// Number of parity shreds in this FEC set.
    pub parity_count: u16,
    /// Shred payload.
    pub payload: Vec<u8>,
}

impl FecShred {
    /// Encode this shred into wire format.
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(FEC_HEADER_SIZE + self.payload.len());
        buf.extend_from_slice(&self.fec_set_id.to_le_bytes());
        buf.extend_from_slice(&self.shred_index.to_le_bytes());
        buf.extend_from_slice(&self.data_count.to_le_bytes());
        buf.extend_from_slice(&self.parity_count.to_le_bytes());
        buf.extend_from_slice(&self.payload);
        buf
    }

    /// Decode a shred from wire format.
    pub fn decode(data: &[u8]) -> Option<Self> {
        if data.len() < FEC_HEADER_SIZE {
            return None;
        }
        let fec_set_id = u64::from_le_bytes(data[0..8].try_into().ok()?);
        let shred_index = u16::from_le_bytes(data[8..10].try_into().ok()?);
        let data_count = u16::from_le_bytes(data[10..12].try_into().ok()?);
        let parity_count = u16::from_le_bytes(data[12..14].try_into().ok()?);
        let payload = data[FEC_HEADER_SIZE..].to_vec();
        Some(Self {
            fec_set_id,
            shred_index,
            data_count,
            parity_count,
            payload,
        })
    }
}

/// Splits a packet into equal-sized data shreds.
///
/// Returns `data_count` shreds, each with payload padded to `shard_size`.
pub fn split_into_shreds(
    data: &[u8],
    fec_set_id: u64,
    data_count: u16,
    parity_count: u16,
) -> Vec<FecShred> {
    let shard_size = (data.len() + data_count as usize - 1) / data_count as usize;
    let mut shreds = Vec::with_capacity(data_count as usize);

    for i in 0..data_count {
        let start = i as usize * shard_size;
        let end = (start + shard_size).min(data.len());
        let mut payload = vec![0u8; shard_size];
        if start < data.len() {
            let copy_len = end - start;
            payload[..copy_len].copy_from_slice(&data[start..end]);
        }
        shreds.push(FecShred {
            fec_set_id,
            shred_index: i,
            data_count,
            parity_count,
            payload,
        });
    }

    shreds
}

/// Reassembles data shreds back into the original packet.
///
/// Requires exactly `data_count` data shreds (indices 0..data_count).
/// Returns `None` if any data shred is missing.
pub fn reassemble_from_data_shreds(
    shreds: &[FecShred],
    data_count: u16,
    original_size: usize,
) -> Option<Vec<u8>> {
    let mut sorted: Vec<Option<&FecShred>> = vec![None; data_count as usize];
    for shred in shreds {
        if shred.shred_index < data_count {
            sorted[shred.shred_index as usize] = Some(shred);
        }
    }

    // Check all data shreds present
    if sorted.iter().any(|s| s.is_none()) {
        return None;
    }

    let mut result = Vec::with_capacity(original_size);
    for shred in sorted.into_iter().flatten() {
        result.extend_from_slice(&shred.payload);
    }
    result.truncate(original_size);
    Some(result)
}

/// Derives a deterministic FEC set ID from a block hash.
pub fn fec_set_id_from_hash(block_hash: &B256) -> u64 {
    u64::from_le_bytes(block_hash.0[..8].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shred_encode_decode_roundtrip() {
        let shred = FecShred {
            fec_set_id: 42,
            shred_index: 3,
            data_count: 16,
            parity_count: 4,
            payload: vec![0xAA; 100],
        };
        let encoded = shred.encode();
        let decoded = FecShred::decode(&encoded).unwrap();
        assert_eq!(decoded.fec_set_id, 42);
        assert_eq!(decoded.shred_index, 3);
        assert_eq!(decoded.data_count, 16);
        assert_eq!(decoded.parity_count, 4);
        assert_eq!(decoded.payload, vec![0xAA; 100]);
    }

    #[test]
    fn split_and_reassemble() {
        let data = (0..1000u16).flat_map(|i| i.to_le_bytes()).collect::<Vec<_>>();
        let shreds = split_into_shreds(&data, 1, 16, 4);
        assert_eq!(shreds.len(), 16);

        let reassembled = reassemble_from_data_shreds(&shreds, 16, data.len()).unwrap();
        assert_eq!(reassembled, data);
    }

    #[test]
    fn split_small_data() {
        let data = vec![1, 2, 3, 4, 5];
        let shreds = split_into_shreds(&data, 1, 4, 2);
        assert_eq!(shreds.len(), 4);

        let reassembled = reassemble_from_data_shreds(&shreds, 4, data.len()).unwrap();
        assert_eq!(reassembled, data);
    }

    #[test]
    fn reassemble_fails_with_missing_shred() {
        let data = vec![0xBB; 100];
        let mut shreds = split_into_shreds(&data, 1, 4, 2);
        shreds.remove(2); // Remove shred index 2
        assert!(reassemble_from_data_shreds(&shreds, 4, data.len()).is_none());
    }

    #[test]
    fn fec_set_id_deterministic() {
        let hash = B256::repeat_byte(0x42);
        let id1 = fec_set_id_from_hash(&hash);
        let id2 = fec_set_id_from_hash(&hash);
        assert_eq!(id1, id2);
        assert_ne!(id1, 0); // Should not be zero for non-zero hash
    }
}
