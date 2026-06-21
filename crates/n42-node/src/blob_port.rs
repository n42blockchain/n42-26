//! Node-side blob-store adapter. The `BlobStorePort` trait lives in
//! `n42-consensus-service`; this module provides the in-process adapter
//! [`DiskBlobStorePort`] over reth's `DiskFileBlobStore` (Caplin stage 6a-3 / 6).

use alloy_eips::eip7594::BlobTransactionSidecarVariant;
use alloy_primitives::B256;
use alloy_rlp::{Decodable, Encodable};
use reth_transaction_pool::blobstore::{BlobStore, DiskFileBlobStore};
use tracing::{debug, warn};

pub use n42_consensus_service::blob_port::BlobStorePort;

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
