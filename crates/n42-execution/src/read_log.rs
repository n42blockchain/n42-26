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
//!
//! ## Compact encoding format
//!
//! Each entry starts with a single header byte that encodes both the type and metadata,
//! eliminating separate tag/data_len fields:
//!
//! ```text
//! 0x00        = AccountNotFound                         (1 byte total)
//! 0x01-0x7F   = Account exists, flags packed:
//!   bit0      = 1 (exists marker)
//!   bit1-3    = nonce byte count (0-7)
//!   bit4      = balance nonzero
//!   bit5      = has_code_hash (contract)
//!   bit6      = nonce uses 8 bytes (overrides bit1-3)
//! 0x80-0xA0   = Storage, value_len = header - 0x80      (1 + value_len bytes)
//! 0xC0        = BlockHash                                (1 + 32 bytes)
//! ```
//!
//! Variable-length fields strip leading zeros (big-endian). This is strictly
//! more compact than pevm's logbin format (which uses 1B length prefix per entry +
//! reth Compact encoding), especially for common cases like zero-value storage
//! and empty/zero-balance accounts.

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

// ─── Compact binary encoding/decoding ───────────────────────────────────────

/// Header byte ranges for the compact encoding.
///
/// `0x00`       = AccountNotFound
/// `0x01..0x7F` = Account exists (flags packed in the byte)
/// `0x80..0xA0` = Storage, value byte count = header − 0x80
/// `0xC0`       = BlockHash (32-byte hash follows)
pub const HEADER_ACCOUNT_NOT_FOUND: u8 = 0x00;
pub const HEADER_STORAGE_BASE: u8 = 0x80;
pub const HEADER_BLOCK_HASH: u8 = 0xC0;

/// Bit masks within an Account header byte (0x01..0x7F).
pub const ACCT_EXISTS_BIT: u8 = 0x01;
pub const ACCT_NONCE_LEN_SHIFT: u8 = 1;
pub const ACCT_NONCE_LEN_MASK: u8 = 0x0E; // bits 1-3
pub const ACCT_BALANCE_BIT: u8 = 0x10;     // bit 4
pub const ACCT_CODE_HASH_BIT: u8 = 0x20;   // bit 5
pub const ACCT_NONCE_8B_BIT: u8 = 0x40;    // bit 6

/// Errors during read log decoding.
#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("unexpected end of data at offset {0}")]
    UnexpectedEof(usize),

    #[error("invalid header byte 0x{0:02X} at offset {1}")]
    InvalidTag(u8, usize),

    #[error("entry count mismatch: expected {expected}, decoded {decoded}")]
    CountMismatch { expected: u32, decoded: u32 },
}

/// Returns the number of significant bytes needed to represent a u64 value
/// in big-endian form (0 for zero).
#[inline]
fn significant_bytes_u64(v: u64) -> usize {
    if v == 0 {
        return 0;
    }
    let bits = 64 - v.leading_zeros() as usize;
    (bits + 7) / 8
}

/// Returns the number of significant bytes in a 32-byte big-endian value
/// (0 if all zeros).
#[inline]
fn significant_bytes_be32(be: &[u8; 32]) -> usize {
    for (i, &b) in be.iter().enumerate() {
        if b != 0 {
            return 32 - i;
        }
    }
    0
}

/// Encodes a read log into compact binary format.
///
/// Format: `entry_count(4B LE) + entries` where each entry is a self-describing
/// compact byte sequence (see module-level doc for the header byte layout).
pub fn encode_read_log(log: &[ReadLogEntry]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(4 + log.len() * 16);
    buf.extend_from_slice(&(log.len() as u32).to_le_bytes());

    for entry in log {
        match entry {
            ReadLogEntry::AccountNotFound => {
                buf.push(HEADER_ACCOUNT_NOT_FOUND);
            }
            ReadLogEntry::Account { nonce, balance, code_hash } => {
                let is_contract = *code_hash != alloy_primitives::KECCAK256_EMPTY;
                let nonce_len = significant_bytes_u64(*nonce);
                let has_balance = *balance != U256::ZERO;

                let mut header: u8 = ACCT_EXISTS_BIT;
                if nonce_len <= 7 {
                    header |= (nonce_len as u8) << ACCT_NONCE_LEN_SHIFT;
                } else {
                    header |= ACCT_NONCE_8B_BIT;
                }
                if has_balance {
                    header |= ACCT_BALANCE_BIT;
                }
                if is_contract {
                    header |= ACCT_CODE_HASH_BIT;
                }
                buf.push(header);

                // Nonce: big-endian, leading zeros stripped
                if nonce_len > 0 {
                    let actual_len = if header & ACCT_NONCE_8B_BIT != 0 { 8 } else { nonce_len };
                    let nonce_be = nonce.to_be_bytes();
                    buf.extend_from_slice(&nonce_be[8 - actual_len..]);
                }

                // Balance: length-prefixed, big-endian, leading zeros stripped
                if has_balance {
                    let balance_be = balance.to_be_bytes::<32>();
                    let balance_len = significant_bytes_be32(&balance_be);
                    buf.push(balance_len as u8);
                    buf.extend_from_slice(&balance_be[32 - balance_len..]);
                }

                // Code hash (32B, only for contracts)
                if is_contract {
                    buf.extend_from_slice(code_hash.as_slice());
                }
            }
            ReadLogEntry::Storage(value) => {
                let value_be = value.to_be_bytes::<32>();
                let value_len = significant_bytes_be32(&value_be);
                buf.push(HEADER_STORAGE_BASE + value_len as u8);
                if value_len > 0 {
                    buf.extend_from_slice(&value_be[32 - value_len..]);
                }
            }
            ReadLogEntry::BlockHash(hash) => {
                buf.push(HEADER_BLOCK_HASH);
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

    let ensure = |pos: usize, n: usize| -> Result<(), DecodeError> {
        if pos + n > data.len() {
            Err(DecodeError::UnexpectedEof(pos))
        } else {
            Ok(())
        }
    };

    for _ in 0..entry_count {
        ensure(pos, 1)?;
        let header = data[pos];
        pos += 1;

        if header == HEADER_ACCOUNT_NOT_FOUND {
            entries.push(ReadLogEntry::AccountNotFound);
        } else if header < HEADER_STORAGE_BASE {
            // Account exists (0x01..0x7F)
            let nonce_len = if header & ACCT_NONCE_8B_BIT != 0 {
                8
            } else {
                ((header & ACCT_NONCE_LEN_MASK) >> ACCT_NONCE_LEN_SHIFT) as usize
            };
            let has_balance = header & ACCT_BALANCE_BIT != 0;
            let has_code_hash = header & ACCT_CODE_HASH_BIT != 0;

            let nonce = if nonce_len > 0 {
                ensure(pos, nonce_len)?;
                let mut buf = [0u8; 8];
                buf[8 - nonce_len..].copy_from_slice(&data[pos..pos + nonce_len]);
                pos += nonce_len;
                u64::from_be_bytes(buf)
            } else {
                0
            };

            let balance = if has_balance {
                ensure(pos, 1)?;
                let balance_len = data[pos] as usize;
                pos += 1;
                ensure(pos, balance_len)?;
                let mut buf = [0u8; 32];
                buf[32 - balance_len..].copy_from_slice(&data[pos..pos + balance_len]);
                pos += balance_len;
                U256::from_be_bytes(buf)
            } else {
                U256::ZERO
            };

            let code_hash = if has_code_hash {
                ensure(pos, 32)?;
                let h = B256::from_slice(&data[pos..pos + 32]);
                pos += 32;
                h
            } else {
                alloy_primitives::KECCAK256_EMPTY
            };

            entries.push(ReadLogEntry::Account { nonce, balance, code_hash });
        } else if header <= HEADER_STORAGE_BASE + 32 {
            // Storage (0x80..0xA0)
            let value_len = (header - HEADER_STORAGE_BASE) as usize;
            let value = if value_len > 0 {
                ensure(pos, value_len)?;
                let mut buf = [0u8; 32];
                buf[32 - value_len..].copy_from_slice(&data[pos..pos + value_len]);
                pos += value_len;
                U256::from_be_bytes(buf)
            } else {
                U256::ZERO
            };
            entries.push(ReadLogEntry::Storage(value));
        } else if header == HEADER_BLOCK_HASH {
            // BlockHash (0xC0)
            ensure(pos, 32)?;
            let h = B256::from_slice(&data[pos..pos + 32]);
            pos += 32;
            entries.push(ReadLogEntry::BlockHash(h));
        } else {
            return Err(DecodeError::InvalidTag(header, pos - 1));
        }
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
        assert_eq!(encoded.len(), 4); // just the 4B count
    }

    #[test]
    fn test_encode_decode_account_not_found() {
        let log = vec![ReadLogEntry::AccountNotFound];
        let encoded = encode_read_log(&log);
        let decoded = decode_read_log(&encoded).unwrap();
        assert_eq!(decoded, log);
        // 4(count) + 1(header=0x00) = 5 bytes
        assert_eq!(encoded.len(), 5);
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
        // 4(count) + 1(header=0x01) = 5 bytes — beats logbin's 2B per entry
        assert_eq!(encoded.len(), 5);
    }

    #[test]
    fn test_encode_decode_eoa_with_nonce() {
        let log = vec![ReadLogEntry::Account {
            nonce: 5,
            balance: U256::ZERO,
            code_hash: KECCAK256_EMPTY,
        }];
        let encoded = encode_read_log(&log);
        let decoded = decode_read_log(&encoded).unwrap();
        assert_eq!(decoded, log);
        // 4(count) + 1(header) + 1(nonce=5) = 6 bytes
        assert_eq!(encoded.len(), 6);
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
        // 4(count) + 1(header) + 1(nonce=42) + 1(balance_len=3) + 3(balance=0x0F4240)
        assert_eq!(encoded.len(), 10);
    }

    #[test]
    fn test_encode_decode_eoa_large_nonce() {
        // nonce that requires 8 bytes (uses bit6)
        let log = vec![ReadLogEntry::Account {
            nonce: u64::MAX,
            balance: U256::ZERO,
            code_hash: KECCAK256_EMPTY,
        }];
        let encoded = encode_read_log(&log);
        let decoded = decode_read_log(&encoded).unwrap();
        assert_eq!(decoded, log);
        // 4(count) + 1(header with bit6) + 8(nonce) = 13 bytes
        assert_eq!(encoded.len(), 13);
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
        // 4(count) + 1(header) + 1(nonce=1) + 1(balance_len=2) + 2(balance=500) + 32(code_hash) = 41 bytes
        assert_eq!(encoded.len(), 41);
    }

    #[test]
    fn test_encode_decode_storage_zero() {
        let log = vec![ReadLogEntry::Storage(U256::ZERO)];
        let encoded = encode_read_log(&log);
        let decoded = decode_read_log(&encoded).unwrap();
        assert_eq!(decoded, log);
        // 4(count) + 1(header=0x80) = 5 bytes — beats logbin's ~2B per entry
        assert_eq!(encoded.len(), 5);
    }

    #[test]
    fn test_encode_decode_storage() {
        let log = vec![ReadLogEntry::Storage(U256::from(0xDEADBEEFu64))];
        let encoded = encode_read_log(&log);
        let decoded = decode_read_log(&encoded).unwrap();
        assert_eq!(decoded, log);
        // 4(count) + 1(header=0x84) + 4(value=0xDEADBEEF) = 9 bytes
        assert_eq!(encoded.len(), 9);
    }

    #[test]
    fn test_encode_decode_storage_max() {
        let log = vec![ReadLogEntry::Storage(U256::MAX)];
        let encoded = encode_read_log(&log);
        let decoded = decode_read_log(&encoded).unwrap();
        assert_eq!(decoded, log);
        // 4(count) + 1(header=0xA0) + 32(value) = 37 bytes
        assert_eq!(encoded.len(), 37);
    }

    #[test]
    fn test_encode_decode_block_hash() {
        let hash = B256::from([0xAB; 32]);
        let log = vec![ReadLogEntry::BlockHash(hash)];
        let encoded = encode_read_log(&log);
        let decoded = decode_read_log(&encoded).unwrap();
        assert_eq!(decoded, log);
        // 4(count) + 1(header=0xC0) + 32(hash) = 37 bytes
        assert_eq!(encoded.len(), 37);
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
        // entry_count = 1, but no data for the entry
        let result = decode_read_log(&[0x01, 0x00, 0x00, 0x00]);
        assert!(result.is_err());
    }

    #[test]
    fn test_decode_invalid_tag() {
        // 1 entry, invalid header byte 0xD0
        let data = [0x01, 0x00, 0x00, 0x00, 0xD0];
        let result = decode_read_log(&data);
        assert!(matches!(result, Err(DecodeError::InvalidTag(0xD0, _))));
    }

    #[test]
    fn test_compact_encoding_saves_space() {
        // EOA with zero nonce and zero balance → just 1 header byte
        let eoa_zero = vec![ReadLogEntry::Account {
            nonce: 0,
            balance: U256::ZERO,
            code_hash: KECCAK256_EMPTY,
        }];
        let encoded_zero = encode_read_log(&eoa_zero);

        // EOA with nonzero nonce and balance → header + nonce + len + balance
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

    #[test]
    fn test_variable_length_storage_encoding() {
        // Zero storage: just 1B header
        let zero = encode_read_log(&[ReadLogEntry::Storage(U256::ZERO)]);
        // Small value (3 bytes): 1B header + 3B
        let small = encode_read_log(&[ReadLogEntry::Storage(U256::from(0x0F4240u64))]);
        // Max value: 1B header + 32B
        let max = encode_read_log(&[ReadLogEntry::Storage(U256::MAX)]);

        // All include 4B count prefix, so subtract for per-entry comparison
        assert_eq!(zero.len() - 4, 1, "zero storage should be 1B");
        assert_eq!(small.len() - 4, 4, "3-byte value storage should be 4B");
        assert_eq!(max.len() - 4, 33, "max storage should be 33B");
    }
}
