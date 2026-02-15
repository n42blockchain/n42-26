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
