use alloy_primitives::B256;
use serde::{Deserialize, Serialize};

/// Block announcement for header-first dissemination.
///
/// In header-first propagation, IDC nodes announce the block header (~500 bytes)
/// via GossipSub. Peers validate the header, then fetch the full body on-demand.
/// This reduces gossip bandwidth: only headers propagate through the mesh.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BlockAnnouncement {
    pub block_hash: B256,
    pub block_number: u64,
    pub parent_hash: B256,
    pub state_root: B256,
    pub transactions_root: B256,
    /// Number of transactions (for peers to decide whether to fetch).
    pub tx_count: u32,
    /// Block size in bytes (for peers to decide whether to fetch).
    pub block_size: u32,
}

pub fn encode_block_announcement(ann: &BlockAnnouncement) -> Result<Vec<u8>, String> {
    bincode::serialize(ann).map_err(|e| e.to_string())
}

pub fn decode_block_announcement(data: &[u8]) -> Result<BlockAnnouncement, String> {
    bincode::deserialize(data).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_announcement() -> BlockAnnouncement {
        BlockAnnouncement {
            block_hash: B256::repeat_byte(0x11),
            block_number: 42,
            parent_hash: B256::repeat_byte(0x22),
            state_root: B256::repeat_byte(0x33),
            transactions_root: B256::repeat_byte(0x44),
            tx_count: 7,
            block_size: 1024,
        }
    }

    #[test]
    fn test_block_announcement_encode_decode() {
        let ann = sample_announcement();
        let encoded = encode_block_announcement(&ann).expect("encoding should succeed");
        assert!(!encoded.is_empty());

        let decoded = decode_block_announcement(&encoded).expect("decoding should succeed");
        assert_eq!(decoded.block_hash, ann.block_hash);
        assert_eq!(decoded.block_number, ann.block_number);
        assert_eq!(decoded.parent_hash, ann.parent_hash);
        assert_eq!(decoded.state_root, ann.state_root);
        assert_eq!(decoded.transactions_root, ann.transactions_root);
        assert_eq!(decoded.tx_count, ann.tx_count);
        assert_eq!(decoded.block_size, ann.block_size);
    }

    #[test]
    fn test_block_announcement_fields() {
        // Unique non-trivial values ensure no field is swapped.
        let ann = BlockAnnouncement {
            block_hash: B256::repeat_byte(0xAA),
            block_number: 999_999,
            parent_hash: B256::repeat_byte(0xBB),
            state_root: B256::repeat_byte(0xCC),
            transactions_root: B256::repeat_byte(0xDD),
            tx_count: 128,
            block_size: 65536,
        };

        let decoded = decode_block_announcement(&encode_block_announcement(&ann).unwrap()).unwrap();
        assert_eq!(decoded.block_hash, B256::repeat_byte(0xAA));
        assert_eq!(decoded.block_number, 999_999);
        assert_eq!(decoded.parent_hash, B256::repeat_byte(0xBB));
        assert_eq!(decoded.state_root, B256::repeat_byte(0xCC));
        assert_eq!(decoded.transactions_root, B256::repeat_byte(0xDD));
        assert_eq!(decoded.tx_count, 128);
        assert_eq!(decoded.block_size, 65536);
    }

    #[test]
    fn test_decode_garbage_data_fails() {
        assert!(decode_block_announcement(&[0xDE, 0xAD, 0xBE, 0xEF]).is_err());
    }

    #[test]
    fn test_decode_empty_data_fails() {
        assert!(decode_block_announcement(&[]).is_err());
    }
}
