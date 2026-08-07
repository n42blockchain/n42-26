use alloy_consensus::{EthereumReceipt, TxType};
use alloy_primitives::{Address, B256, Bytes, Log};
pub use n42_consensus::gov5_native_receipts_root;

pub const MAX_RECEIPTS_PER_BLOCK: usize = 250_000;
const MAX_LOGS_PER_BLOCK: usize = 1_000_000;

#[derive(Debug, thiserror::Error)]
pub enum CompactReceiptError {
    #[error("compact receipt bytes are truncated")]
    Truncated,
    #[error("compact receipt flags use reserved bits")]
    ReservedFlags,
    #[error("unsupported compact receipt transaction type {0}")]
    UnsupportedType(u8),
    #[error("compact receipt gas length {0} exceeds eight bytes")]
    InvalidGasLength(usize),
    #[error("compact receipt varint is invalid or non-canonical")]
    InvalidVarint,
    #[error("compact receipt log has {0} topics, max four")]
    TooManyTopics(usize),
    #[error("compact receipt count exceeds {MAX_RECEIPTS_PER_BLOCK}")]
    TooManyReceipts,
    #[error("compact receipt log count exceeds {MAX_LOGS_PER_BLOCK}")]
    TooManyLogs,
}

pub fn decode_compact_receipts(bytes: &[u8]) -> Result<Vec<EthereumReceipt>, CompactReceiptError> {
    let mut cursor = Cursor::new(bytes);
    let mut receipts = Vec::new();
    let mut total_logs = 0usize;
    while !cursor.is_empty() {
        if receipts.len() == MAX_RECEIPTS_PER_BLOCK {
            return Err(CompactReceiptError::TooManyReceipts);
        }
        let flags = cursor.byte()?;
        if flags & 0x80 != 0 {
            return Err(CompactReceiptError::ReservedFlags);
        }
        let encoded_type = flags & 0x03;
        let tx_type = if encoded_type == 3 {
            cursor.byte()?
        } else {
            encoded_type
        };
        let tx_type =
            TxType::try_from(tx_type).map_err(|_| CompactReceiptError::UnsupportedType(tx_type))?;

        let gas_len = usize::from((flags >> 3) & 0x0f);
        if gas_len > 8 {
            return Err(CompactReceiptError::InvalidGasLength(gas_len));
        }
        let mut gas = [0; 8];
        gas[8 - gas_len..].copy_from_slice(cursor.take(gas_len)?);

        let log_count =
            usize::try_from(cursor.varint()?).map_err(|_| CompactReceiptError::TooManyLogs)?;
        total_logs = total_logs
            .checked_add(log_count)
            .filter(|count| *count <= MAX_LOGS_PER_BLOCK)
            .ok_or(CompactReceiptError::TooManyLogs)?;
        if log_count > cursor.remaining() / 22 {
            return Err(CompactReceiptError::Truncated);
        }
        let mut logs = Vec::with_capacity(log_count);
        for _ in 0..log_count {
            let address = Address::from_slice(cursor.take(20)?);
            let topic_count = usize::from(cursor.byte()?);
            if topic_count > 4 {
                return Err(CompactReceiptError::TooManyTopics(topic_count));
            }
            let mut topics = Vec::with_capacity(topic_count);
            for _ in 0..topic_count {
                topics.push(B256::from_slice(cursor.take(32)?));
            }
            let data_len =
                usize::try_from(cursor.varint()?).map_err(|_| CompactReceiptError::Truncated)?;
            let data = Bytes::copy_from_slice(cursor.take(data_len)?);
            logs.push(Log::new_unchecked(address, topics, data));
        }
        receipts.push(EthereumReceipt {
            tx_type,
            success: flags & 0x04 != 0,
            cumulative_gas_used: u64::from_be_bytes(gas),
            logs,
        });
    }
    Ok(receipts)
}

struct Cursor<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Cursor<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn is_empty(&self) -> bool {
        self.position == self.bytes.len()
    }

    fn remaining(&self) -> usize {
        self.bytes.len() - self.position
    }

    fn byte(&mut self) -> Result<u8, CompactReceiptError> {
        Ok(self.take(1)?[0])
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], CompactReceiptError> {
        let end = self
            .position
            .checked_add(len)
            .filter(|end| *end <= self.bytes.len())
            .ok_or(CompactReceiptError::Truncated)?;
        let value = &self.bytes[self.position..end];
        self.position = end;
        Ok(value)
    }

    fn varint(&mut self) -> Result<u64, CompactReceiptError> {
        let mut value = 0u64;
        for index in 0..10 {
            let byte = self.byte()?;
            if index == 9 && byte > 1 {
                return Err(CompactReceiptError::InvalidVarint);
            }
            value |= u64::from(byte & 0x7f) << (index * 7);
            if byte & 0x80 == 0 {
                if index > 0 && value < (1u64 << (index * 7)) {
                    return Err(CompactReceiptError::InvalidVarint);
                }
                return Ok(value);
            }
        }
        Err(CompactReceiptError::InvalidVarint)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_gov5_legacy_success_receipt() {
        let receipts = decode_compact_receipts(&[0x14, 0x52, 0x08, 0x00]).unwrap();
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].tx_type, TxType::Legacy);
        assert!(receipts[0].success);
        assert_eq!(receipts[0].cumulative_gas_used, 21_000);
        assert!(receipts[0].logs.is_empty());
        assert_eq!(
            gov5_native_receipts_root(&receipts),
            "0x9ec602b25fc63e86a5feb8943d52cf66b24ed8e8021f3f74f077271ffae88c75"
                .parse::<B256>()
                .unwrap()
        );
    }

    #[test]
    fn decodes_extended_type_and_log() {
        let mut bytes = vec![0x0f, 0x04, 0x01, 0x01];
        bytes.extend_from_slice(&[0x11; 20]);
        bytes.push(1);
        bytes.extend_from_slice(&[0x22; 32]);
        bytes.push(3);
        bytes.extend_from_slice(&[1, 2, 3]);
        let receipts = decode_compact_receipts(&bytes).unwrap();
        assert_eq!(receipts[0].tx_type, TxType::Eip7702);
        assert_eq!(receipts[0].logs[0].data.data.as_ref(), &[1, 2, 3]);
    }

    #[test]
    fn rejects_noncanonical_varint_and_too_many_topics() {
        assert!(matches!(
            decode_compact_receipts(&[0x04, 0x80, 0x00]),
            Err(CompactReceiptError::InvalidVarint)
        ));
        let mut too_many_topics = vec![0x04, 0x01];
        too_many_topics.extend_from_slice(&[0; 20]);
        too_many_topics.push(5);
        too_many_topics.push(0);
        assert!(matches!(
            decode_compact_receipts(&too_many_topics),
            Err(CompactReceiptError::TooManyTopics(5))
        ));
    }
}
