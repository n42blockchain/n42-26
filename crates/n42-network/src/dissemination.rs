use alloy_primitives::B256;
use serde::{Deserialize, Serialize};

/// Block announcement for header-first dissemination.
///
/// In header-first propagation, IDC nodes first announce the block header
/// (compact ~500 bytes) via GossipSub. Peers validate the header against
/// consensus rules, then fetch the full block body on-demand if needed.
///
/// This reduces gossip bandwidth significantly: only headers propagate
/// through the mesh, while block bodies use direct peer-to-peer transfers.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BlockAnnouncement {
    /// Hash of the announced block.
    pub block_hash: B256,
    /// Block number.
    pub block_number: u64,
    /// Parent block hash.
    pub parent_hash: B256,
    /// State root after executing this block.
    pub state_root: B256,
    /// Transactions root.
    pub transactions_root: B256,
    /// Number of transactions in the block.
    pub tx_count: u32,
    /// Block size in bytes (for peers to decide whether to fetch).
    pub block_size: u32,
}

/// Encodes a block announcement to bytes.
pub fn encode_block_announcement(ann: &BlockAnnouncement) -> Result<Vec<u8>, String> {
    bincode::serialize(ann).map_err(|e| e.to_string())
}

/// Decodes a block announcement from bytes.
pub fn decode_block_announcement(data: &[u8]) -> Result<BlockAnnouncement, String> {
    bincode::deserialize(data).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: create a BlockAnnouncement with deterministic test values.
    fn sample_block_announcement() -> BlockAnnouncement {
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
        let ann = sample_block_announcement();

        let encoded = encode_block_announcement(&ann)
            .expect("encoding should succeed");
        assert!(!encoded.is_empty(), "encoded bytes should not be empty");

        let decoded = decode_block_announcement(&encoded)
            .expect("decoding should succeed");

        assert_eq!(decoded.block_hash, ann.block_hash, "block_hash should match");
        assert_eq!(decoded.block_number, ann.block_number, "block_number should match");
        assert_eq!(decoded.parent_hash, ann.parent_hash, "parent_hash should match");
        assert_eq!(decoded.state_root, ann.state_root, "state_root should match");
        assert_eq!(decoded.transactions_root, ann.transactions_root, "transactions_root should match");
        assert_eq!(decoded.tx_count, ann.tx_count, "tx_count should match");
        assert_eq!(decoded.block_size, ann.block_size, "block_size should match");
    }

    #[test]
    fn test_block_announcement_fields() {
        // Use unique non-trivial values for each field to ensure no field is swapped.
        let ann = BlockAnnouncement {
            block_hash: B256::repeat_byte(0xAA),
            block_number: 999_999,
            parent_hash: B256::repeat_byte(0xBB),
            state_root: B256::repeat_byte(0xCC),
            transactions_root: B256::repeat_byte(0xDD),
            tx_count: 128,
            block_size: 65536,
        };

        let encoded = encode_block_announcement(&ann).unwrap();
        let decoded = decode_block_announcement(&encoded).unwrap();

        // Every field must survive the round-trip with its unique value intact.
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
        let garbage = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let result = decode_block_announcement(&garbage);
        assert!(result.is_err(), "garbage data should fail to decode");
    }

    #[test]
    fn test_decode_empty_data_fails() {
        let result = decode_block_announcement(&[]);
        assert!(result.is_err(), "empty data should fail to decode");
    }
}
