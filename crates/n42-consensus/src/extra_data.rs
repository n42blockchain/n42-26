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
    if extra_data.len() < QC_MAGIC.len() || &extra_data[..4] != QC_MAGIC {
        return Ok(None);
    }

    bincode::deserialize(&extra_data[4..])
        .map(Some)
        .map_err(|e| ConsensusError::Other(format!("malformed QC in extra_data: {e}")))
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

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::B256;
    use bitvec::prelude::*;
    use n42_primitives::BlsSecretKey;

    /// Helper: create a valid QuorumCertificate with a real BLS signature.
    fn make_test_qc() -> QuorumCertificate {
        let sk = BlsSecretKey::random().unwrap();
        let sig = sk.sign(b"test");
        QuorumCertificate {
            view: 42,
            block_hash: B256::repeat_byte(0xAA),
            aggregate_signature: sig,
            signers: bitvec![u8, Msb0; 1, 1, 0, 1],
        }
    }

    #[test]
    fn test_encode_decode_roundtrip() {
        let qc = make_test_qc();

        let encoded = encode_qc_to_extra_data(&qc).expect("encoding should succeed");
        assert_eq!(&encoded[..4], b"N42Q");

        let decoded = extract_qc_from_extra_data(&encoded)
            .expect("decoding should succeed")
            .expect("should contain a QC");

        assert_eq!(decoded.view, qc.view);
        assert_eq!(decoded.block_hash, qc.block_hash);
        assert_eq!(decoded.aggregate_signature, qc.aggregate_signature);
        assert_eq!(decoded.signers, qc.signers);
    }

    #[test]
    fn test_extract_no_magic() {
        let data = Bytes::from_static(b"ABCDEFGHIJKLMNOP");
        let result = extract_qc_from_extra_data(&data);
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[test]
    fn test_extract_empty() {
        let data = Bytes::new();
        let result = extract_qc_from_extra_data(&data);
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[test]
    fn test_extract_short() {
        let data = Bytes::from_static(b"N42");
        let result = extract_qc_from_extra_data(&data);
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[test]
    fn test_extract_malformed() {
        let mut data = Vec::from(b"N42Q" as &[u8]);
        data.extend_from_slice(&[0xFF, 0xFE, 0x00, 0x01, 0x02]);
        let data = Bytes::from(data);

        let result = extract_qc_from_extra_data(&data);
        assert!(result.is_err());
    }
}
