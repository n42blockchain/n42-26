//! Blob-store port — a byte-oriented trait over reth's `DiskFileBlobStore` so
//! the consensus orchestrator handles only RLP bytes, never reth blob types. The
//! adapter (`DiskBlobStorePort`) lives node-side. Caplin EL-seam refactor
//! (stage 6a-3 / 6c).

use alloy_primitives::B256;

/// Stores / retrieves EIP-4844 blob sidecars as RLP bytes. Callers exchange
/// `(tx_hash, sidecar_rlp)` only; the reth blob type + RLP coding stay in the
/// node-side adapter.
pub trait BlobStorePort: Send + Sync {
    /// Decode an RLP-encoded sidecar and insert it under `tx_hash`. Logging on
    /// decode / insert failure is identical to the previous inline path.
    fn insert_rlp(&self, tx_hash: B256, sidecar_rlp: &[u8]);

    /// Fetch the sidecars for `tx_hashes`, RLP-encoding each into `(tx_hash, rlp)`.
    /// `Err` on a store error (caller logs it with the block hash).
    fn get_all_encoded(&self, tx_hashes: Vec<B256>) -> Result<Vec<(B256, Vec<u8>)>, String>;
}
