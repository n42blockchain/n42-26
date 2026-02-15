use alloy_primitives::Bytes;
use n42_primitives::QuorumCertificate;
use reth_consensus::ConsensusError;

/// Magic prefix identifying N42 QC data in header extra_data.
/// ASCII "N42Q" = [0x4E, 0x34, 0x32, 0x51]
const QC_MAGIC: &[u8; 4] = b"N42Q";

/// Extracts a QuorumCertificate from a block header's extra_data field.
///
/// ## Format
///
/// ```text
/// extra_data = [4B magic "N42Q"] + [bincode-encoded QuorumCertificate]
/// ```
///
/// Returns `None` if the extra_data doesn't contain a QC (e.g., genesis
/// block or blocks during initial sync before consensus engine starts).
/// Returns `Err` if the magic is present but the QC data is malformed.
pub fn extract_qc_from_extra_data(
    extra_data: &Bytes,
) -> Result<Option<QuorumCertificate>, ConsensusError> {
    // No extra_data or too short for magic prefix
    if extra_data.len() < QC_MAGIC.len() {
        return Ok(None);
    }

    // Check magic prefix
    if &extra_data[..4] != QC_MAGIC {
        return Ok(None);
    }

    // Decode the QC from the remaining bytes
    let qc_bytes = &extra_data[4..];
    bincode::deserialize(qc_bytes).map(Some).map_err(|e| {
        ConsensusError::Other(format!("malformed QC in extra_data: {e}"))
    })
}

/// Encodes a QuorumCertificate into bytes suitable for header extra_data.
///
/// Prepends the 4-byte magic prefix "N42Q" before the bincode-encoded QC.
pub fn encode_qc_to_extra_data(qc: &QuorumCertificate) -> Result<Bytes, ConsensusError> {
    let qc_bytes = bincode::serialize(qc).map_err(|e| {
        ConsensusError::Other(format!("QC serialization failed: {e}"))
    })?;

    let mut data = Vec::with_capacity(4 + qc_bytes.len());
    data.extend_from_slice(QC_MAGIC);
    data.extend_from_slice(&qc_bytes);

    Ok(Bytes::from(data))
}

/// Errors specific to QC extra_data encoding/decoding.
#[derive(Debug, thiserror::Error)]
pub enum ExtraDataError {
    /// The extra_data has the QC magic prefix but the data is malformed.
    #[error("malformed QC in extra_data: {0}")]
    MalformedQC(String),

    /// Failed to serialize QC to bytes.
    #[error("QC serialization failed: {0}")]
    SerializationFailed(String),
}
