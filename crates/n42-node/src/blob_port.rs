//! Blob-store port — a byte-oriented trait over reth's `DiskFileBlobStore` so the
//! consensus orchestrator (and the future `n42-consensus-service` crate) handle
//! only RLP bytes, never reth blob types. The `BlobTransactionSidecarVariant`
//! RLP coding + `DiskFileBlobStore` live in the node-side adapter here. Part of
//! the Caplin EL-seam refactor (stage 6a-3); see
//! `docs/task-caplin-stage6-clean-extraction.md`.

use alloy_eips::eip7594::BlobTransactionSidecarVariant;
use alloy_primitives::B256;
use alloy_rlp::{Decodable, Encodable};
use reth_transaction_pool::blobstore::{BlobStore, DiskFileBlobStore};
use tracing::{debug, warn};

/// Stores / retrieves EIP-4844 blob sidecars as RLP bytes. Callers exchange
/// `(tx_hash, sidecar_rlp)` only; the reth blob type + RLP coding stay in the
/// adapter.
pub trait BlobStorePort: Send + Sync {
    /// Decode an RLP-encoded sidecar and insert it under `tx_hash`. Logging on
    /// decode / insert failure is identical to the previous inline path.
    fn insert_rlp(&self, tx_hash: B256, sidecar_rlp: &[u8]);

    /// Fetch the sidecars for `tx_hashes`, RLP-encoding each into `(tx_hash, rlp)`.
    /// `Err` on a store error (caller logs it with the block hash).
    fn get_all_encoded(&self, tx_hashes: Vec<B256>) -> Result<Vec<(B256, Vec<u8>)>, String>;
}

/// In-process adapter over reth's `DiskFileBlobStore`.
pub struct DiskBlobStorePort(pub DiskFileBlobStore);

impl BlobStorePort for DiskBlobStorePort {
    fn insert_rlp(&self, tx_hash: B256, sidecar_rlp: &[u8]) {
        match <BlobTransactionSidecarVariant as Decodable>::decode(&mut &sidecar_rlp[..]) {
            Ok(sidecar) => {
                if let Err(e) = self.0.insert(tx_hash, sidecar) {
                    debug!(target: "n42::cl::exec_bridge", %tx_hash, error = %e, "failed to insert blob sidecar");
                }
            }
            Err(e) => {
                warn!(target: "n42::cl::exec_bridge", %tx_hash, error = %e, "failed to decode blob sidecar RLP");
            }
        }
    }

    fn get_all_encoded(&self, tx_hashes: Vec<B256>) -> Result<Vec<(B256, Vec<u8>)>, String> {
        let sidecars = self.0.get_all(tx_hashes).map_err(|e| e.to_string())?;
        Ok(sidecars
            .into_iter()
            .map(|(tx_hash, sidecar)| {
                let mut buf = Vec::new();
                sidecar.encode(&mut buf);
                (tx_hash, buf)
            })
            .collect())
    }
}
