use crate::{decode_compact_receipts, gov5_native_receipts_root};
use alloy_primitives::{B256, keccak256};
use alloy_rlp::Header as RlpHeader;
use std::io::Read;

const MAGIC: &[u8; 8] = b"N42FRNG\x01";
pub const MAX_FINALIZED_RANGE_BLOCKS: u64 = 128;
const MAX_BLOB_SIZE: usize = 16 << 20;

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
    #[error("finalized block {0} transaction and receipt counts differ")]
    ReceiptCountMismatch(u64),
    #[error("finalized block {0} compact receipts are invalid: {1}")]
    InvalidReceipts(u64, String),
    #[error("finalized block {0} receipt root mismatch")]
    ReceiptRootMismatch(u64),
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
        let fields =
            header_fields(&header).ok_or(FinalizedRangeError::HeaderFieldsMismatch(number))?;
        if fields.number != number
            || fields.parent_hash != parent_hash
            || fields.state_root != state_root
            || fields.transactions_root != tx_root
            || fields.receipts_root != receipts_root
        {
            return Err(FinalizedRangeError::HeaderFieldsMismatch(number));
        }
        let block = reader.blob(number)?;
        if embedded_header(&block).is_none_or(|raw| raw != header) {
            return Err(FinalizedRangeError::BlockHeaderMismatch(number));
        }
        let receipts_bytes = reader.optional_blob(number)?;
        let receipts = decode_compact_receipts(&receipts_bytes)
            .map_err(|error| FinalizedRangeError::InvalidReceipts(number, error.to_string()))?;
        if block_transaction_count(&block) != Some(receipts.len()) {
            return Err(FinalizedRangeError::ReceiptCountMismatch(number));
        }
        transaction_count = transaction_count
            .checked_add(receipts.len() as u64)
            .ok_or(FinalizedRangeError::Bounds)?;
        if gov5_native_receipts_root(&receipts) != receipts_root {
            return Err(FinalizedRangeError::ReceiptRootMismatch(number));
        }
        previous_hash = Some(block_hash);
        last = (block_hash, state_root, receipts_root);
    }
    reader.finish()?;
    Ok(FinalizedRangeVerification {
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
    })
}

struct HeaderFields {
    parent_hash: B256,
    state_root: B256,
    transactions_root: B256,
    receipts_root: B256,
    number: u64,
}

fn header_fields(header: &[u8]) -> Option<HeaderFields> {
    let mut payload = header;
    let outer = RlpHeader::decode(&mut payload).ok()?;
    if !outer.list || outer.payload_length != payload.len() {
        return None;
    }

    let parent_hash = b256(rlp_bytes(&mut payload)?)?;
    rlp_bytes(&mut payload)?; // ommers hash
    rlp_bytes(&mut payload)?; // beneficiary
    let state_root = b256(rlp_bytes(&mut payload)?)?;
    let transactions_root = b256(rlp_bytes(&mut payload)?)?;
    let receipts_root = b256(rlp_bytes(&mut payload)?)?;
    rlp_bytes(&mut payload)?; // logs bloom
    rlp_bytes(&mut payload)?; // difficulty
    let number = rlp_u64(rlp_bytes(&mut payload)?)?;
    Some(HeaderFields {
        parent_hash,
        state_root,
        transactions_root,
        receipts_root,
        number,
    })
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

fn b256(value: &[u8]) -> Option<B256> {
    (value.len() == 32).then(|| B256::from_slice(value))
}

fn rlp_u64(value: &[u8]) -> Option<u64> {
    if value.len() > 8 || value.first() == Some(&0) {
        return None;
    }
    let mut bytes = [0; 8];
    bytes[8 - value.len()..].copy_from_slice(value);
    Some(u64::from_be_bytes(bytes))
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

fn block_transaction_count(block: &[u8]) -> Option<usize> {
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
    let mut count = 0usize;
    while !transactions.is_empty() {
        rlp_item(&mut transactions)?;
        count = count.checked_add(1)?;
    }
    Some(count)
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
    use std::io::Cursor;

    fn empty_receipt_root() -> B256 {
        gov5_native_receipts_root(&[])
    }

    fn header_fixture() -> Vec<u8> {
        let mut payload = Vec::new();
        for value in [B256::repeat_byte(0x06), B256::repeat_byte(0x22)] {
            payload.push(0xa0);
            payload.extend_from_slice(value.as_slice());
        }
        payload.push(0x94);
        payload.extend_from_slice(&[0; 20]);
        for value in [
            B256::repeat_byte(0x33),
            B256::repeat_byte(0x55),
            empty_receipt_root(),
        ] {
            payload.push(0xa0);
            payload.extend_from_slice(value.as_slice());
        }
        payload.extend_from_slice(&[0xb9, 0x01, 0x00]);
        payload.extend_from_slice(&[0; 256]);
        payload.push(0x80);
        payload.push(0x07);

        let mut header = vec![0xf9, (payload.len() >> 8) as u8, payload.len() as u8];
        header.extend_from_slice(&payload);
        header
    }

    fn fixture() -> Vec<u8> {
        let header = header_fixture();
        let block_payload_len = header.len() + 1;
        let mut block = vec![
            0xf9,
            (block_payload_len >> 8) as u8,
            block_payload_len as u8,
        ];
        block.extend_from_slice(&header);
        block.push(0xc0);
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
        bytes.extend_from_slice(B256::repeat_byte(0x55).as_slice());
        for blob in [&header[..], &block[..], &[][..]] {
            bytes.extend_from_slice(&(blob.len() as u32).to_le_bytes());
            bytes.extend_from_slice(blob);
        }
        let digest = blake3::hash(&bytes);
        bytes.extend_from_slice(digest.as_bytes());
        bytes
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
