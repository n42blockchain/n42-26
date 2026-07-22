use crate::{decode_compact_receipts, gov5_native_receipts_root};
use alloy_consensus::{Header as ConsensusHeader, TxEnvelope, proofs::calculate_transaction_root};
use alloy_eips::Decodable2718;
use alloy_primitives::{B256, keccak256};
use alloy_rlp::{Decodable, Header as RlpHeader};
use std::io::Read;

const MAGIC: &[u8; 8] = b"N42FRNG\x01";
pub const MAX_FINALIZED_RANGE_BLOCKS: u64 = 128;
const MAX_BLOB_SIZE: usize = 16 << 20;
pub const MAX_MATERIALIZED_FINALIZED_RANGE_BYTES: usize = 256 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalizedRangeVerification {
    pub chain_id: u64,
    pub genesis_hash: B256,
    pub from_block: u64,
    pub to_block: u64,
    pub block_count: u64,
    pub transaction_count: u64,
    pub first_parent_hash: B256,
    pub last_block_hash: B256,
    pub last_state_root: B256,
    pub last_receipts_root: B256,
}

/// Canonical bytes returned only after the complete finalized-range frame has
/// passed its trailing digest and all per-block checks.
#[derive(Debug, Clone)]
pub struct VerifiedFinalizedRangeEntry {
    pub number: u64,
    pub block_hash: B256,
    pub parent_hash: B256,
    pub state_root: B256,
    pub receipts_root: B256,
    pub transactions_root: B256,
    pub header: ConsensusHeader,
    pub header_rlp: Vec<u8>,
    pub block_rlp: Vec<u8>,
    pub transactions: Vec<TxEnvelope>,
    pub receipts: Vec<alloy_consensus::EthereumReceipt>,
}

/// Authenticated replay input. Construction is intentionally restricted to
/// [`decode_finalized_range_stream`], so callers cannot observe entries before
/// the whole-frame Blake3 digest succeeds.
#[derive(Debug, Clone)]
pub struct VerifiedFinalizedRange {
    pub verification: FinalizedRangeVerification,
    pub entries: Vec<VerifiedFinalizedRangeEntry>,
}

#[derive(Debug, thiserror::Error)]
pub enum FinalizedRangeError {
    #[error("finalized range I/O failed: {0}")]
    Io(String),
    #[error("unsupported finalized range version")]
    Version,
    #[error("finalized range chain identity mismatch")]
    ChainIdentity,
    #[error("invalid finalized range bounds")]
    Bounds,
    #[error("non-contiguous finalized block at {0}")]
    NonContiguous(u64),
    #[error("finalized block {0} parent lineage mismatch")]
    ParentMismatch(u64),
    #[error("finalized block {0} blob is invalid or oversized")]
    InvalidBlob(u64),
    #[error("finalized block {0} header hash mismatch")]
    HeaderHashMismatch(u64),
    #[error("finalized block {0} declared fields do not match its header")]
    HeaderFieldsMismatch(u64),
    #[error("finalized block {0} does not embed the declared header")]
    BlockHeaderMismatch(u64),
    #[error("finalized block {0} transactions are invalid or unsupported")]
    InvalidTransactions(u64),
    #[error("finalized block {0} transaction root mismatch")]
    TransactionRootMismatch(u64),
    #[error("finalized block {0} transaction and receipt counts differ")]
    ReceiptCountMismatch(u64),
    #[error("finalized block {0} compact receipts are invalid: {1}")]
    InvalidReceipts(u64, String),
    #[error("finalized block {0} receipt root mismatch")]
    ReceiptRootMismatch(u64),
    #[error("materialized finalized range exceeds {MAX_MATERIALIZED_FINALIZED_RANGE_BYTES} bytes")]
    MaterializedBytes,
    #[error("finalized range content hash mismatch")]
    ContentHashMismatch,
    #[error("finalized range has trailing bytes")]
    TrailingBytes,
}

pub fn verify_finalized_range_stream<R: Read>(
    reader: R,
    expected_chain_id: u64,
    expected_genesis_hash: B256,
) -> Result<FinalizedRangeVerification, FinalizedRangeError> {
    Ok(
        parse_finalized_range_stream(reader, expected_chain_id, expected_genesis_hash, false)?
            .verification,
    )
}

/// Verifies the entire frame before returning any canonical block bytes to a
/// potential execution importer. The aggregate retained-byte cap is separate
/// from the per-blob wire cap and prevents a bounded block count from becoming
/// an unbounded materialization request.
pub fn decode_finalized_range_stream<R: Read>(
    reader: R,
    expected_chain_id: u64,
    expected_genesis_hash: B256,
) -> Result<VerifiedFinalizedRange, FinalizedRangeError> {
    parse_finalized_range_stream(reader, expected_chain_id, expected_genesis_hash, true)
}

fn parse_finalized_range_stream<R: Read>(
    reader: R,
    expected_chain_id: u64,
    expected_genesis_hash: B256,
    materialize: bool,
) -> Result<VerifiedFinalizedRange, FinalizedRangeError> {
    let mut reader = HashingReader::new(reader);
    if reader.array::<8>()? != *MAGIC {
        return Err(FinalizedRangeError::Version);
    }
    let chain_id = u64::from_le_bytes(reader.array()?);
    let genesis_hash = B256::from(reader.array::<32>()?);
    if chain_id != expected_chain_id || genesis_hash != expected_genesis_hash {
        return Err(FinalizedRangeError::ChainIdentity);
    }
    let from_block = u64::from_le_bytes(reader.array()?);
    let to_block = u64::from_le_bytes(reader.array()?);
    let block_count = u64::from_le_bytes(reader.array()?);
    if block_count == 0
        || block_count > MAX_FINALIZED_RANGE_BLOCKS
        || from_block > to_block
        || to_block - from_block + 1 != block_count
    {
        return Err(FinalizedRangeError::Bounds);
    }

    let mut previous_hash = None;
    let mut first_parent_hash = B256::ZERO;
    let mut last = (B256::ZERO, B256::ZERO, B256::ZERO);
    let mut transaction_count = 0u64;
    let mut materialized_bytes = 0usize;
    let mut entries = Vec::with_capacity(if materialize { block_count as usize } else { 0 });
    for index in 0..block_count {
        let number = u64::from_le_bytes(reader.array()?);
        if number != from_block + index {
            return Err(FinalizedRangeError::NonContiguous(number));
        }
        let block_hash = B256::from(reader.array::<32>()?);
        let parent_hash = B256::from(reader.array::<32>()?);
        let state_root = B256::from(reader.array::<32>()?);
        let receipts_root = B256::from(reader.array::<32>()?);
        let tx_root = B256::from(reader.array::<32>()?);
        if index == 0 {
            first_parent_hash = parent_hash;
        } else if previous_hash != Some(parent_hash) {
            return Err(FinalizedRangeError::ParentMismatch(number));
        }
        let header = reader.blob(number)?;
        if keccak256(&header) != block_hash {
            return Err(FinalizedRangeError::HeaderHashMismatch(number));
        }
        let decoded_header =
            decode_header(&header).ok_or(FinalizedRangeError::HeaderFieldsMismatch(number))?;
        if decoded_header.number != number
            || decoded_header.parent_hash != parent_hash
            || decoded_header.state_root != state_root
            || decoded_header.transactions_root != tx_root
            || decoded_header.receipts_root != receipts_root
        {
            return Err(FinalizedRangeError::HeaderFieldsMismatch(number));
        }
        let block = reader.blob(number)?;
        if embedded_header(&block).is_none_or(|raw| raw != header) {
            return Err(FinalizedRangeError::BlockHeaderMismatch(number));
        }
        let transactions =
            block_transactions(&block).ok_or(FinalizedRangeError::InvalidTransactions(number))?;
        if calculate_transaction_root(&transactions) != tx_root {
            return Err(FinalizedRangeError::TransactionRootMismatch(number));
        }
        let receipts_bytes = reader.optional_blob(number)?;
        let receipts = decode_compact_receipts(&receipts_bytes)
            .map_err(|error| FinalizedRangeError::InvalidReceipts(number, error.to_string()))?;
        if transactions.len() != receipts.len() {
            return Err(FinalizedRangeError::ReceiptCountMismatch(number));
        }
        transaction_count = transaction_count
            .checked_add(receipts.len() as u64)
            .ok_or(FinalizedRangeError::Bounds)?;
        if gov5_native_receipts_root(&receipts) != receipts_root {
            return Err(FinalizedRangeError::ReceiptRootMismatch(number));
        }
        if materialize {
            materialized_bytes = materialized_bytes
                .checked_add(header.len())
                .and_then(|size| size.checked_add(block.len()))
                .and_then(|size| size.checked_add(receipts_bytes.len()))
                .filter(|size| *size <= MAX_MATERIALIZED_FINALIZED_RANGE_BYTES)
                .ok_or(FinalizedRangeError::MaterializedBytes)?;
            entries.push(VerifiedFinalizedRangeEntry {
                number,
                block_hash,
                parent_hash,
                state_root,
                receipts_root,
                transactions_root: tx_root,
                header: decoded_header,
                header_rlp: header,
                block_rlp: block,
                transactions,
                receipts,
            });
        }
        previous_hash = Some(block_hash);
        last = (block_hash, state_root, receipts_root);
    }
    reader.finish()?;
    Ok(VerifiedFinalizedRange {
        verification: FinalizedRangeVerification {
            chain_id,
            genesis_hash,
            from_block,
            to_block,
            block_count,
            transaction_count,
            first_parent_hash,
            last_block_hash: last.0,
            last_state_root: last.1,
            last_receipts_root: last.2,
        },
        entries,
    })
}

fn decode_header(encoded: &[u8]) -> Option<ConsensusHeader> {
    let mut cursor = encoded;
    let header = ConsensusHeader::decode(&mut cursor).ok()?;
    cursor.is_empty().then_some(header)
}

fn rlp_bytes<'a>(cursor: &mut &'a [u8]) -> Option<&'a [u8]> {
    let mut payload = *cursor;
    let header = RlpHeader::decode(&mut payload).ok()?;
    if header.list || header.payload_length > payload.len() {
        return None;
    }
    let value = payload.get(..header.payload_length)?;
    *cursor = payload.get(header.payload_length..)?;
    Some(value)
}

fn embedded_header(block: &[u8]) -> Option<&[u8]> {
    let mut outer_cursor = block;
    let outer = RlpHeader::decode(&mut outer_cursor).ok()?;
    if !outer.list || outer.payload_length != outer_cursor.len() {
        return None;
    }
    let payload = outer_cursor;
    let mut header_cursor = payload;
    let header = RlpHeader::decode(&mut header_cursor).ok()?;
    let prefix_len = payload.len() - header_cursor.len();
    let total_len = prefix_len.checked_add(header.payload_length)?;
    payload.get(..total_len)
}

fn block_transactions(block: &[u8]) -> Option<Vec<TxEnvelope>> {
    let mut payload = block;
    let outer = RlpHeader::decode(&mut payload).ok()?;
    if !outer.list || outer.payload_length != payload.len() {
        return None;
    }
    rlp_item(&mut payload)?;
    let mut transactions = payload;
    let transactions_header = RlpHeader::decode(&mut transactions).ok()?;
    if !transactions_header.list || transactions_header.payload_length > transactions.len() {
        return None;
    }
    let mut transactions = transactions.get(..transactions_header.payload_length)?;
    let mut decoded = Vec::new();
    while !transactions.is_empty() {
        let encoded = rlp_bytes(&mut transactions)?;
        let mut encoded_cursor = encoded;
        decoded.push(TxEnvelope::decode_2718(&mut encoded_cursor).ok()?);
        if !encoded_cursor.is_empty() {
            return None;
        }
    }
    Some(decoded)
}

fn rlp_item<'a>(cursor: &mut &'a [u8]) -> Option<&'a [u8]> {
    let original = *cursor;
    let mut payload = original;
    let header = RlpHeader::decode(&mut payload).ok()?;
    let prefix_len = original.len().checked_sub(payload.len())?;
    let total_len = prefix_len.checked_add(header.payload_length)?;
    let item = original.get(..total_len)?;
    *cursor = original.get(total_len..)?;
    Some(item)
}

struct HashingReader<R> {
    inner: R,
    hasher: blake3::Hasher,
}

impl<R: Read> HashingReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            hasher: blake3::Hasher::new(),
        }
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], FinalizedRangeError> {
        let mut bytes = [0; N];
        self.inner.read_exact(&mut bytes).map_err(io_error)?;
        self.hasher.update(&bytes);
        Ok(bytes)
    }

    fn blob(&mut self, number: u64) -> Result<Vec<u8>, FinalizedRangeError> {
        let size = u32::from_le_bytes(self.array()?) as usize;
        if size == 0 || size > MAX_BLOB_SIZE {
            return Err(FinalizedRangeError::InvalidBlob(number));
        }
        let mut bytes = vec![0; size];
        self.inner.read_exact(&mut bytes).map_err(io_error)?;
        self.hasher.update(&bytes);
        Ok(bytes)
    }

    fn optional_blob(&mut self, number: u64) -> Result<Vec<u8>, FinalizedRangeError> {
        let size = u32::from_le_bytes(self.array()?) as usize;
        if size > MAX_BLOB_SIZE {
            return Err(FinalizedRangeError::InvalidBlob(number));
        }
        let mut bytes = vec![0; size];
        self.inner.read_exact(&mut bytes).map_err(io_error)?;
        self.hasher.update(&bytes);
        Ok(bytes)
    }

    fn finish(mut self) -> Result<(), FinalizedRangeError> {
        let mut digest = [0; 32];
        self.inner.read_exact(&mut digest).map_err(io_error)?;
        if self.hasher.finalize().as_bytes() != &digest {
            return Err(FinalizedRangeError::ContentHashMismatch);
        }
        let mut trailing = [0; 1];
        match self.inner.read(&mut trailing).map_err(io_error)? {
            0 => Ok(()),
            _ => Err(FinalizedRangeError::TrailingBytes),
        }
    }
}

fn io_error(error: std::io::Error) -> FinalizedRangeError {
    FinalizedRangeError::Io(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{SignableTransaction, TxLegacy};
    use alloy_eips::Encodable2718;
    use alloy_primitives::{Address, Bytes, Signature, TxKind, U256};
    use alloy_rlp::Encodable;
    use std::io::Cursor;

    fn empty_receipt_root() -> B256 {
        gov5_native_receipts_root(&[])
    }

    fn empty_transaction_root() -> B256 {
        calculate_transaction_root::<TxEnvelope>(&[])
    }

    fn header_fixture() -> Vec<u8> {
        let header = ConsensusHeader {
            parent_hash: B256::repeat_byte(0x06),
            ommers_hash: B256::repeat_byte(0x22),
            state_root: B256::repeat_byte(0x33),
            transactions_root: empty_transaction_root(),
            receipts_root: empty_receipt_root(),
            number: 7,
            ..Default::default()
        };
        let mut encoded = Vec::new();
        header.encode(&mut encoded);
        encoded
    }

    fn fixture_with_transactions(transaction_data: Vec<Bytes>) -> Vec<u8> {
        let header = header_fixture();
        let block_payload_len = header.len() + transaction_data.length();
        let mut block = Vec::new();
        RlpHeader {
            list: true,
            payload_length: block_payload_len,
        }
        .encode(&mut block);
        block.extend_from_slice(&header);
        transaction_data.encode(&mut block);
        let block_hash = keccak256(&header);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&1143_u64.to_le_bytes());
        bytes.extend_from_slice(B256::repeat_byte(0x11).as_slice());
        bytes.extend_from_slice(&7_u64.to_le_bytes());
        bytes.extend_from_slice(&7_u64.to_le_bytes());
        bytes.extend_from_slice(&1_u64.to_le_bytes());
        bytes.extend_from_slice(&7_u64.to_le_bytes());
        bytes.extend_from_slice(block_hash.as_slice());
        bytes.extend_from_slice(B256::repeat_byte(0x06).as_slice());
        bytes.extend_from_slice(B256::repeat_byte(0x33).as_slice());
        bytes.extend_from_slice(empty_receipt_root().as_slice());
        bytes.extend_from_slice(empty_transaction_root().as_slice());
        for blob in [&header[..], &block[..], &[][..]] {
            bytes.extend_from_slice(&(blob.len() as u32).to_le_bytes());
            bytes.extend_from_slice(blob);
        }
        let digest = blake3::hash(&bytes);
        bytes.extend_from_slice(digest.as_bytes());
        bytes
    }

    fn fixture() -> Vec<u8> {
        fixture_with_transactions(Vec::new())
    }

    #[test]
    fn verifies_bounded_range_and_embedded_header() {
        let verified =
            verify_finalized_range_stream(Cursor::new(fixture()), 1143, B256::repeat_byte(0x11))
                .unwrap();
        assert_eq!(verified.from_block, 7);
        assert_eq!(verified.to_block, 7);
        assert_eq!(verified.transaction_count, 0);
        assert_eq!(verified.last_block_hash, keccak256(header_fixture()));
    }

    #[test]
    fn materializes_entries_only_after_whole_frame_authentication() {
        let encoded = fixture();
        let verified =
            decode_finalized_range_stream(Cursor::new(&encoded), 1143, B256::repeat_byte(0x11))
                .unwrap();
        assert_eq!(verified.entries.len(), 1);
        assert_eq!(verified.entries[0].number, 7);
        assert_eq!(verified.entries[0].header_rlp, header_fixture());
        assert!(verified.entries[0].transactions.is_empty());
        assert!(verified.entries[0].receipts.is_empty());

        let mut tampered = encoded;
        *tampered.last_mut().unwrap() ^= 1;
        assert!(matches!(
            decode_finalized_range_stream(Cursor::new(tampered), 1143, B256::repeat_byte(0x11)),
            Err(FinalizedRangeError::ContentHashMismatch)
        ));
    }

    #[test]
    fn rejects_tampered_content() {
        let mut bytes = fixture();
        let index = bytes.len() - 1;
        bytes[index] ^= 1;
        assert!(matches!(
            verify_finalized_range_stream(Cursor::new(bytes), 1143, B256::repeat_byte(0x11)),
            Err(FinalizedRangeError::ContentHashMismatch)
        ));
    }

    #[test]
    fn rejects_body_transactions_that_do_not_match_header_root() {
        let transaction = TxLegacy {
            nonce: 1,
            gas_price: 100,
            gas_limit: 21_000,
            to: TxKind::Call(Address::ZERO),
            value: U256::from(1_000),
            input: Default::default(),
            chain_id: Some(1143),
        }
        .into_signed(Signature::new(U256::from(1), U256::from(2), false));
        let envelope: TxEnvelope = transaction.into();
        let mut encoded = Vec::new();
        envelope.encode_2718(&mut encoded);
        let result = verify_finalized_range_stream(
            Cursor::new(fixture_with_transactions(vec![Bytes::from(encoded)])),
            1143,
            B256::repeat_byte(0x11),
        );
        assert!(
            matches!(result, Err(FinalizedRangeError::TransactionRootMismatch(7))),
            "{result:?}"
        );
    }

    #[test]
    fn rejects_rehashed_declared_root_that_is_not_in_header() {
        let mut bytes = fixture();
        bytes[144] ^= 1;
        let content_len = bytes.len() - 32;
        let digest = blake3::hash(&bytes[..content_len]);
        bytes[content_len..].copy_from_slice(digest.as_bytes());
        assert!(matches!(
            verify_finalized_range_stream(Cursor::new(bytes), 1143, B256::repeat_byte(0x11)),
            Err(FinalizedRangeError::HeaderFieldsMismatch(7))
        ));
    }

    #[test]
    fn rejects_receipts_without_matching_block_transactions() {
        let mut bytes = fixture();
        bytes.truncate(bytes.len() - 32);
        let receipt_length = bytes.len() - 4;
        bytes[receipt_length..].copy_from_slice(&4u32.to_le_bytes());
        bytes.extend_from_slice(&[0x14, 0x52, 0x08, 0x00]);
        let digest = blake3::hash(&bytes);
        bytes.extend_from_slice(digest.as_bytes());
        assert!(matches!(
            verify_finalized_range_stream(Cursor::new(bytes), 1143, B256::repeat_byte(0x11)),
            Err(FinalizedRangeError::ReceiptCountMismatch(7))
        ));
    }
}
