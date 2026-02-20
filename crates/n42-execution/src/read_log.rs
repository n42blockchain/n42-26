//! Ordered state read log for deterministic replay.
//!
//! `ReadLogDatabase` wraps any revm `Database` and records all first-time reads
//! (basic, storage, block_hash) into an ordered log. The log entries, when replayed
//! in the same order on the mobile side via `StreamReplayDB`, reconstruct the exact
//! same EVM execution without needing address/slot keys.
//!
//! This works because: same block + same transactions → revm `Database::basic()`,
//! `Database::storage()`, `Database::block_hash()` calls happen in a 100% deterministic
//! order. JournaledState caches first reads, so the log only contains unique first-time
//! values.

use alloy_primitives::{Bytes, B256, U256};
use revm::{
    bytecode::Bytecode,
    database_interface::{DBErrorMarker, Database},
    primitives::StorageKey,
    state::AccountInfo,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// A single entry in the ordered state read log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadLogEntry {
    /// Account does not exist at the queried address.
    AccountNotFound,
    /// Account exists with the given state.
    Account {
        nonce: u64,
        balance: U256,
        code_hash: B256,
    },
    /// Storage value at a slot (key is implicit from call order).
    Storage(U256),
    /// Block hash for a block number (number is implicit from call order).
    BlockHash(B256),
}

/// Wraps a database and captures all first-time reads into an ordered log.
///
/// Uses `Arc<Mutex<Vec<ReadLogEntry>>>` because reth's executor moves/consumes
/// the DB. After execution completes, the Arc allows extracting the log.
/// Single-threaded execution means no contention on the Mutex.
#[derive(Debug)]
pub struct ReadLogDatabase<DB> {
    inner: DB,
    log: Arc<Mutex<Vec<ReadLogEntry>>>,
    captured_codes: Arc<Mutex<HashMap<B256, Bytes>>>,
}

impl<DB> ReadLogDatabase<DB> {
    /// Creates a new read-log wrapper around the given database.
    pub fn new(inner: DB) -> Self {
        Self {
            inner,
            log: Arc::new(Mutex::new(Vec::new())),
            captured_codes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Returns a clone of the Arc holding the read log.
    ///
    /// Call this before passing the DB to the executor (which consumes self).
    pub fn log_handle(&self) -> Arc<Mutex<Vec<ReadLogEntry>>> {
        Arc::clone(&self.log)
    }

    /// Returns a clone of the Arc holding captured bytecodes.
    pub fn codes_handle(&self) -> Arc<Mutex<HashMap<B256, Bytes>>> {
        Arc::clone(&self.captured_codes)
    }
}

impl<DB: Database> Database for ReadLogDatabase<DB>
where
    DB::Error: DBErrorMarker,
{
    type Error = DB::Error;

    fn basic(&mut self, address: alloy_primitives::Address) -> Result<Option<AccountInfo>, Self::Error> {
        let result = self.inner.basic(address)?;
        let mut log = self.log.lock().unwrap();
        match &result {
            None => {
                log.push(ReadLogEntry::AccountNotFound);
            }
            Some(info) => {
                // Capture bytecode if present (for the bytecodes section).
                if let Some(ref code) = info.code {
                    if info.code_hash != alloy_primitives::KECCAK256_EMPTY {
                        let mut codes = self.captured_codes.lock().unwrap();
                        codes.entry(info.code_hash).or_insert_with(|| {
                            Bytes::copy_from_slice(code.original_byte_slice())
                        });
                    }
                }
                log.push(ReadLogEntry::Account {
                    nonce: info.nonce,
                    balance: info.balance,
                    code_hash: info.code_hash,
                });
            }
        }
        Ok(result)
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        // code_by_hash is NOT logged — bytecodes travel via a separate channel.
        self.inner.code_by_hash(code_hash)
    }

    fn storage(
        &mut self,
        address: alloy_primitives::Address,
        index: StorageKey,
    ) -> Result<U256, Self::Error> {
        let value = self.inner.storage(address, index)?;
        let mut log = self.log.lock().unwrap();
        log.push(ReadLogEntry::Storage(value));
        Ok(value)
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        let hash = self.inner.block_hash(number)?;
        let mut log = self.log.lock().unwrap();
        log.push(ReadLogEntry::BlockHash(hash));
        Ok(hash)
    }
}

// ─── Binary encoding/decoding ───────────────────────────────────────────────

/// Tag bytes for read log entry types.
const TAG_ACCOUNT_NOT_FOUND: u8 = 0x00;
const TAG_ACCOUNT_EOA: u8 = 0x01;
const TAG_ACCOUNT_CONTRACT: u8 = 0x02;
const TAG_STORAGE: u8 = 0x03;
const TAG_BLOCK_HASH: u8 = 0x04;

/// Account flags for conditional field encoding.
const ACCT_FLAG_NONCE_NONZERO: u8 = 0x01;
const ACCT_FLAG_BALANCE_NONZERO: u8 = 0x02;

/// Errors during read log decoding.
#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("unexpected end of data at offset {0}")]
    UnexpectedEof(usize),

    #[error("invalid tag 0x{0:02X} at offset {1}")]
    InvalidTag(u8, usize),

    #[error("entry count mismatch: expected {expected}, decoded {decoded}")]
    CountMismatch { expected: u32, decoded: u32 },
}

/// Encodes a read log into compact binary format.
///
/// Format:
/// ```text
/// entry_count: u32 LE
/// for each entry:
///   tag: u8
///   data_len: u16 LE
///   data: [u8; data_len]
/// ```
///
/// Entry data formats:
/// - AccountNotFound: data_len=0
/// - AccountEOA: flags(1B) + conditional nonce(8B LE) + conditional balance(32B BE)
/// - AccountContract: flags(1B) + conditional nonce/balance + code_hash(32B)
/// - Storage: value(32B BE)
/// - BlockHash: hash(32B)
pub fn encode_read_log(log: &[ReadLogEntry]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(log.len() * 40);
    buf.extend_from_slice(&(log.len() as u32).to_le_bytes());

    for entry in log {
        match entry {
            ReadLogEntry::AccountNotFound => {
                buf.push(TAG_ACCOUNT_NOT_FOUND);
                buf.extend_from_slice(&0u16.to_le_bytes());
            }
            ReadLogEntry::Account { nonce, balance, code_hash } => {
                let is_contract = *code_hash != alloy_primitives::KECCAK256_EMPTY;
                let tag = if is_contract { TAG_ACCOUNT_CONTRACT } else { TAG_ACCOUNT_EOA };

                let mut flags: u8 = 0;
                if *nonce != 0 { flags |= ACCT_FLAG_NONCE_NONZERO; }
                if *balance != U256::ZERO { flags |= ACCT_FLAG_BALANCE_NONZERO; }

                // Calculate data length
                let mut data_len: usize = 1; // flags byte
                if *nonce != 0 { data_len += 8; }
                if *balance != U256::ZERO { data_len += 32; }
                if is_contract { data_len += 32; } // code_hash

                buf.push(tag);
                buf.extend_from_slice(&(data_len as u16).to_le_bytes());
                buf.push(flags);
                if *nonce != 0 {
                    buf.extend_from_slice(&nonce.to_le_bytes());
                }
                if *balance != U256::ZERO {
                    buf.extend_from_slice(&balance.to_be_bytes::<32>());
                }
                if is_contract {
                    buf.extend_from_slice(code_hash.as_slice());
                }
            }
            ReadLogEntry::Storage(value) => {
                buf.push(TAG_STORAGE);
                buf.extend_from_slice(&32u16.to_le_bytes());
                buf.extend_from_slice(&value.to_be_bytes::<32>());
            }
            ReadLogEntry::BlockHash(hash) => {
                buf.push(TAG_BLOCK_HASH);
                buf.extend_from_slice(&32u16.to_le_bytes());
                buf.extend_from_slice(hash.as_slice());
            }
        }
    }

    buf
}

/// Decodes a read log from compact binary format.
pub fn decode_read_log(data: &[u8]) -> Result<Vec<ReadLogEntry>, DecodeError> {
    if data.len() < 4 {
        return Err(DecodeError::UnexpectedEof(0));
    }

    let entry_count = u32::from_le_bytes(data[0..4].try_into().unwrap());
    let mut entries = Vec::with_capacity(entry_count as usize);
    let mut pos: usize = 4;

    for _ in 0..entry_count {
        // Read tag
        if pos >= data.len() {
            return Err(DecodeError::UnexpectedEof(pos));
        }
        let tag = data[pos];
        pos += 1;

        // Read data_len
        if pos + 2 > data.len() {
            return Err(DecodeError::UnexpectedEof(pos));
        }
        let data_len = u16::from_le_bytes(data[pos..pos + 2].try_into().unwrap()) as usize;
        pos += 2;

        if pos + data_len > data.len() {
            return Err(DecodeError::UnexpectedEof(pos));
        }

        let entry_data = &data[pos..pos + data_len];
        pos += data_len;

        let entry = match tag {
            TAG_ACCOUNT_NOT_FOUND => ReadLogEntry::AccountNotFound,
            TAG_ACCOUNT_EOA | TAG_ACCOUNT_CONTRACT => {
                if entry_data.is_empty() {
                    return Err(DecodeError::UnexpectedEof(pos));
                }
                let flags = entry_data[0];
                let mut dpos: usize = 1;

                let nonce = if flags & ACCT_FLAG_NONCE_NONZERO != 0 {
                    if dpos + 8 > entry_data.len() {
                        return Err(DecodeError::UnexpectedEof(pos));
                    }
                    let n = u64::from_le_bytes(entry_data[dpos..dpos + 8].try_into().unwrap());
                    dpos += 8;
                    n
                } else {
                    0
                };

                let balance = if flags & ACCT_FLAG_BALANCE_NONZERO != 0 {
                    if dpos + 32 > entry_data.len() {
                        return Err(DecodeError::UnexpectedEof(pos));
                    }
                    let b = U256::from_be_slice(&entry_data[dpos..dpos + 32]);
                    dpos += 32;
                    b
                } else {
                    U256::ZERO
                };

                let code_hash = if tag == TAG_ACCOUNT_CONTRACT {
                    if dpos + 32 > entry_data.len() {
                        return Err(DecodeError::UnexpectedEof(pos));
                    }
                    B256::from_slice(&entry_data[dpos..dpos + 32])
                } else {
                    alloy_primitives::KECCAK256_EMPTY
                };

                ReadLogEntry::Account { nonce, balance, code_hash }
            }
            TAG_STORAGE => {
                if entry_data.len() < 32 {
                    return Err(DecodeError::UnexpectedEof(pos));
                }
                ReadLogEntry::Storage(U256::from_be_slice(&entry_data[..32]))
            }
            TAG_BLOCK_HASH => {
                if entry_data.len() < 32 {
                    return Err(DecodeError::UnexpectedEof(pos));
                }
                ReadLogEntry::BlockHash(B256::from_slice(&entry_data[..32]))
            }
            _ => return Err(DecodeError::InvalidTag(tag, pos - data_len)),
        };

        entries.push(entry);
    }

    if entries.len() != entry_count as usize {
        return Err(DecodeError::CountMismatch {
            expected: entry_count,
            decoded: entries.len() as u32,
        });
    }

    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{KECCAK256_EMPTY, U256, B256};

    #[test]
    fn test_encode_decode_empty_log() {
        let log: Vec<ReadLogEntry> = vec![];
        let encoded = encode_read_log(&log);
        let decoded = decode_read_log(&encoded).unwrap();
        assert!(decoded.is_empty());
    }

    #[test]
    fn test_encode_decode_account_not_found() {
        let log = vec![ReadLogEntry::AccountNotFound];
        let encoded = encode_read_log(&log);
        let decoded = decode_read_log(&encoded).unwrap();
        assert_eq!(decoded, log);
    }

    #[test]
    fn test_encode_decode_eoa_zero_values() {
        let log = vec![ReadLogEntry::Account {
            nonce: 0,
            balance: U256::ZERO,
            code_hash: KECCAK256_EMPTY,
        }];
        let encoded = encode_read_log(&log);
        let decoded = decode_read_log(&encoded).unwrap();
        assert_eq!(decoded, log);
        // Minimal encoding: 4(count) + 1(tag) + 2(len) + 1(flags) = 8 bytes
        assert_eq!(encoded.len(), 8);
    }

    #[test]
    fn test_encode_decode_eoa_with_values() {
        let log = vec![ReadLogEntry::Account {
            nonce: 42,
            balance: U256::from(1_000_000u64),
            code_hash: KECCAK256_EMPTY,
        }];
        let encoded = encode_read_log(&log);
        let decoded = decode_read_log(&encoded).unwrap();
        assert_eq!(decoded, log);
    }

    #[test]
    fn test_encode_decode_contract() {
        let code_hash = B256::from([0xCC; 32]);
        let log = vec![ReadLogEntry::Account {
            nonce: 1,
            balance: U256::from(500u64),
            code_hash,
        }];
        let encoded = encode_read_log(&log);
        let decoded = decode_read_log(&encoded).unwrap();
        assert_eq!(decoded, log);
    }

    #[test]
    fn test_encode_decode_storage() {
        let log = vec![ReadLogEntry::Storage(U256::from(0xDEADBEEFu64))];
        let encoded = encode_read_log(&log);
        let decoded = decode_read_log(&encoded).unwrap();
        assert_eq!(decoded, log);
    }

    #[test]
    fn test_encode_decode_block_hash() {
        let hash = B256::from([0xAB; 32]);
        let log = vec![ReadLogEntry::BlockHash(hash)];
        let encoded = encode_read_log(&log);
        let decoded = decode_read_log(&encoded).unwrap();
        assert_eq!(decoded, log);
    }

    #[test]
    fn test_encode_decode_mixed_log() {
        let log = vec![
            ReadLogEntry::Account {
                nonce: 10,
                balance: U256::from(999u64),
                code_hash: KECCAK256_EMPTY,
            },
            ReadLogEntry::AccountNotFound,
            ReadLogEntry::Account {
                nonce: 0,
                balance: U256::ZERO,
                code_hash: B256::from([0xDD; 32]),
            },
            ReadLogEntry::Storage(U256::from(42u64)),
            ReadLogEntry::Storage(U256::MAX),
            ReadLogEntry::BlockHash(B256::from([0x11; 32])),
            ReadLogEntry::AccountNotFound,
        ];
        let encoded = encode_read_log(&log);
        let decoded = decode_read_log(&encoded).unwrap();
        assert_eq!(decoded, log);
    }

    #[test]
    fn test_decode_truncated_data() {
        let result = decode_read_log(&[0x01, 0x00, 0x00]);
        assert!(result.is_err());
    }

    #[test]
    fn test_decode_invalid_tag() {
        // 1 entry, tag 0xFF, data_len 0
        let data = [0x01, 0x00, 0x00, 0x00, 0xFF, 0x00, 0x00];
        let result = decode_read_log(&data);
        assert!(matches!(result, Err(DecodeError::InvalidTag(0xFF, _))));
    }

    #[test]
    fn test_compact_encoding_saves_space() {
        // EOA with zero nonce and zero balance → only 1 flag byte
        let eoa_zero = vec![ReadLogEntry::Account {
            nonce: 0,
            balance: U256::ZERO,
            code_hash: KECCAK256_EMPTY,
        }];
        let encoded_zero = encode_read_log(&eoa_zero);

        // EOA with nonzero nonce and balance → flags + 8 + 32 bytes
        let eoa_full = vec![ReadLogEntry::Account {
            nonce: 100,
            balance: U256::from(1_000_000u64),
            code_hash: KECCAK256_EMPTY,
        }];
        let encoded_full = encode_read_log(&eoa_full);

        assert!(encoded_zero.len() < encoded_full.len(),
            "zero-value EOA ({} bytes) should be smaller than full EOA ({} bytes)",
            encoded_zero.len(), encoded_full.len());
    }
}
