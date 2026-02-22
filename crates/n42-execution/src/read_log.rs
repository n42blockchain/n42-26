//! Ordered state read log for deterministic replay.
//!
//! Records all first-time reads (basic, storage, block_hash) into an ordered log.
//! Replaying these in the same order on mobile via `StreamReplayDB` reconstructs
//! the exact same EVM execution without needing address/slot keys.
//!
//! Works because: same block + txs → deterministic `Database::basic()`,
//! `Database::storage()`, `Database::block_hash()` call order.
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
/// Uses `Arc<Mutex<Vec<ReadLogEntry>>>` for shared ownership after DB is consumed.
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

    #[error("invalid field length {len} at offset {offset} (max {max})")]
    InvalidFieldLen { len: usize, max: usize, offset: usize },

    #[error("entry count mismatch: expected {expected}, decoded {decoded}")]
    CountMismatch { expected: u32, decoded: u32 },

    #[error("entry count {0} exceeds maximum {1}")]
    EntryCountOverflow(u32, u32),
}

/// Maximum allowed entry count to prevent OOM from corrupted data.
/// A single Ethereum block with 30M gas can have at most ~1500 unique account reads
/// and ~30000 storage reads. 1M entries provides ample headroom.
const MAX_ENTRY_COUNT: u32 = 1_000_000;

/// Returns the number of significant bytes in a u64 value (0 for zero).
#[inline]
fn significant_bytes_u64(v: u64) -> usize {
    if v == 0 { 0 } else { (64 - v.leading_zeros() as usize + 7) / 8 }
}

/// Returns the number of significant bytes in a 32-byte big-endian value (0 if all zeros).
#[inline]
fn significant_bytes_be32(be: &[u8; 32]) -> usize {
    be.iter().position(|&b| b != 0).map(|i| 32 - i).unwrap_or(0)
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
    if entry_count > MAX_ENTRY_COUNT {
        return Err(DecodeError::EntryCountOverflow(entry_count, MAX_ENTRY_COUNT));
    }
    let mut entries = Vec::with_capacity(entry_count as usize);
    let mut pos: usize = 4;

    let ensure = |pos: usize, n: usize| {
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
                if balance_len > 32 {
                    return Err(DecodeError::InvalidFieldLen { len: balance_len, max: 32, offset: pos - 1 });
                }
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

    // ── Nonce boundary tests ──

    #[test]
    fn test_nonce_boundary_1_byte() {
        // nonce=1 → 1 byte, nonce=255 → 1 byte
        for nonce in [1u64, 127, 255] {
            let log = vec![ReadLogEntry::Account {
                nonce,
                balance: U256::ZERO,
                code_hash: KECCAK256_EMPTY,
            }];
            let encoded = encode_read_log(&log);
            let decoded = decode_read_log(&encoded).unwrap();
            assert_eq!(decoded, log, "nonce={nonce} roundtrip failed");
            // 4(count) + 1(header) + 1(nonce) = 6
            assert_eq!(encoded.len(), 6, "nonce={nonce} should be 1 byte");
        }
    }

    #[test]
    fn test_nonce_boundary_2_bytes() {
        // nonce=256 → 2 bytes, nonce=65535 → 2 bytes
        for nonce in [256u64, 0x1234, 65535] {
            let log = vec![ReadLogEntry::Account {
                nonce,
                balance: U256::ZERO,
                code_hash: KECCAK256_EMPTY,
            }];
            let encoded = encode_read_log(&log);
            let decoded = decode_read_log(&encoded).unwrap();
            assert_eq!(decoded, log, "nonce={nonce} roundtrip failed");
            // 4(count) + 1(header) + 2(nonce) = 7
            assert_eq!(encoded.len(), 7, "nonce={nonce} should be 2 bytes");
        }
    }

    #[test]
    fn test_nonce_boundary_each_byte_count() {
        // Test every byte count boundary: 3, 4, 5, 6, 7 bytes
        let cases: &[(u64, usize)] = &[
            (0x10000, 3),           // 3 bytes
            (0x1000000, 4),         // 4 bytes
            (0x100000000, 5),       // 5 bytes
            (0x10000000000, 6),     // 6 bytes
            (0x1000000000000, 7),   // 7 bytes
        ];
        for &(nonce, expected_nonce_bytes) in cases {
            let log = vec![ReadLogEntry::Account {
                nonce,
                balance: U256::ZERO,
                code_hash: KECCAK256_EMPTY,
            }];
            let encoded = encode_read_log(&log);
            let decoded = decode_read_log(&encoded).unwrap();
            assert_eq!(decoded, log, "nonce={nonce:#x} roundtrip failed");
            assert_eq!(
                encoded.len(),
                4 + 1 + expected_nonce_bytes,
                "nonce={nonce:#x} should be {expected_nonce_bytes} bytes"
            );
        }
    }

    #[test]
    fn test_nonce_7_byte_max() {
        // Largest 7-byte nonce: 0x00FFFFFFFFFFFFFF
        let nonce = 0x00FFFFFFFFFFFFFFu64;
        let log = vec![ReadLogEntry::Account {
            nonce,
            balance: U256::ZERO,
            code_hash: KECCAK256_EMPTY,
        }];
        let encoded = encode_read_log(&log);
        let decoded = decode_read_log(&encoded).unwrap();
        assert_eq!(decoded, log);
        // 4 + 1 + 7 = 12 (uses bits 1-3, not bit6)
        assert_eq!(encoded.len(), 12);
    }

    #[test]
    fn test_nonce_8_byte_boundary() {
        // Smallest 8-byte nonce: 0x0100000000000000
        let nonce = 0x0100000000000000u64;
        let log = vec![ReadLogEntry::Account {
            nonce,
            balance: U256::ZERO,
            code_hash: KECCAK256_EMPTY,
        }];
        let encoded = encode_read_log(&log);
        let decoded = decode_read_log(&encoded).unwrap();
        assert_eq!(decoded, log);
        // 4 + 1 + 8 = 13 (uses bit6)
        assert_eq!(encoded.len(), 13);
    }

    // ── Balance boundary tests ──

    #[test]
    fn test_balance_1_byte() {
        let log = vec![ReadLogEntry::Account {
            nonce: 0,
            balance: U256::from(1u64),
            code_hash: KECCAK256_EMPTY,
        }];
        let encoded = encode_read_log(&log);
        let decoded = decode_read_log(&encoded).unwrap();
        assert_eq!(decoded, log);
        // 4(count) + 1(header) + 1(balance_len=1) + 1(balance=1) = 7
        assert_eq!(encoded.len(), 7);
    }

    #[test]
    fn test_balance_max() {
        let log = vec![ReadLogEntry::Account {
            nonce: 0,
            balance: U256::MAX,
            code_hash: KECCAK256_EMPTY,
        }];
        let encoded = encode_read_log(&log);
        let decoded = decode_read_log(&encoded).unwrap();
        assert_eq!(decoded, log);
        // 4(count) + 1(header) + 1(balance_len=32) + 32(balance) = 38
        assert_eq!(encoded.len(), 38);
    }

    // ── Contract boundary tests ──

    #[test]
    fn test_contract_zero_nonce_zero_balance() {
        // Contract with zero nonce and zero balance (e.g., freshly deployed proxy)
        let code_hash = B256::from([0xAA; 32]);
        let log = vec![ReadLogEntry::Account {
            nonce: 0,
            balance: U256::ZERO,
            code_hash,
        }];
        let encoded = encode_read_log(&log);
        let decoded = decode_read_log(&encoded).unwrap();
        assert_eq!(decoded, log);
        // 4(count) + 1(header with code_hash bit) + 32(code_hash) = 37
        assert_eq!(encoded.len(), 37);
    }

    #[test]
    fn test_contract_all_max_values() {
        let code_hash = B256::from([0xFF; 32]);
        let log = vec![ReadLogEntry::Account {
            nonce: u64::MAX,
            balance: U256::MAX,
            code_hash,
        }];
        let encoded = encode_read_log(&log);
        let decoded = decode_read_log(&encoded).unwrap();
        assert_eq!(decoded, log);
        // 4 + 1(header) + 8(nonce) + 1(balance_len) + 32(balance) + 32(code_hash) = 78
        assert_eq!(encoded.len(), 78);
    }

    // ── Storage boundary tests ──

    #[test]
    fn test_storage_1_byte_value() {
        let log = vec![ReadLogEntry::Storage(U256::from(0xFFu64))];
        let encoded = encode_read_log(&log);
        let decoded = decode_read_log(&encoded).unwrap();
        assert_eq!(decoded, log);
        // 4 + 1(header=0x81) + 1(value) = 6
        assert_eq!(encoded.len(), 6);
    }

    #[test]
    fn test_storage_2_byte_boundary() {
        let log = vec![ReadLogEntry::Storage(U256::from(0x100u64))];
        let encoded = encode_read_log(&log);
        let decoded = decode_read_log(&encoded).unwrap();
        assert_eq!(decoded, log);
        // 4 + 1(header=0x82) + 2 = 7
        assert_eq!(encoded.len(), 7);
    }

    // ── Error handling tests ──

    #[test]
    fn test_decode_too_short() {
        // Less than 4 bytes
        assert!(matches!(decode_read_log(&[]), Err(DecodeError::UnexpectedEof(0))));
        assert!(matches!(decode_read_log(&[0x00]), Err(DecodeError::UnexpectedEof(0))));
        assert!(matches!(decode_read_log(&[0x00, 0x00, 0x00]), Err(DecodeError::UnexpectedEof(0))));
    }

    #[test]
    fn test_decode_corrupted_balance_len() {
        // Hand-craft: 1 entry, Account header with balance bit set, balance_len = 33 (invalid)
        let mut data = vec![0x01, 0x00, 0x00, 0x00]; // entry_count = 1
        let header = ACCT_EXISTS_BIT | ACCT_BALANCE_BIT; // exists + has balance
        data.push(header);
        data.push(33); // balance_len = 33 (exceeds 32)
        data.extend_from_slice(&[0xFF; 33]); // fake balance data

        let result = decode_read_log(&data);
        assert!(result.is_err(), "balance_len > 32 should be rejected");
        assert!(matches!(result, Err(DecodeError::InvalidFieldLen { len: 33, max: 32, .. })));
    }

    #[test]
    fn test_decode_entry_count_overflow() {
        // entry_count = MAX_ENTRY_COUNT + 1
        let count = (MAX_ENTRY_COUNT + 1).to_le_bytes();
        let result = decode_read_log(&count);
        assert!(matches!(result, Err(DecodeError::EntryCountOverflow(_, _))));
    }

    #[test]
    fn test_decode_invalid_tag_in_gap() {
        // Tags 0xA1..0xBF are invalid (between Storage max 0xA0 and BlockHash 0xC0)
        for tag in [0xA1u8, 0xB0, 0xBF] {
            let data = [0x01, 0x00, 0x00, 0x00, tag];
            let result = decode_read_log(&data);
            assert!(matches!(result, Err(DecodeError::InvalidTag(t, _)) if t == tag),
                "tag 0x{tag:02X} should be rejected");
        }
    }

    #[test]
    fn test_decode_invalid_tag_above_c0() {
        // Tags 0xC1..0xFF are invalid (only 0xC0 is BlockHash)
        for tag in [0xC1u8, 0xD0, 0xFF] {
            let data = [0x01, 0x00, 0x00, 0x00, tag];
            let result = decode_read_log(&data);
            assert!(matches!(result, Err(DecodeError::InvalidTag(t, _)) if t == tag),
                "tag 0x{tag:02X} should be rejected");
        }
    }

    #[test]
    fn test_decode_truncated_account_nonce() {
        // Account with nonce_len=3, but only 2 bytes of nonce data
        let mut data = vec![0x01, 0x00, 0x00, 0x00]; // entry_count = 1
        let header = ACCT_EXISTS_BIT | (3u8 << ACCT_NONCE_LEN_SHIFT);
        data.push(header);
        data.extend_from_slice(&[0x01, 0x02]); // only 2 bytes, need 3
        assert!(decode_read_log(&data).is_err());
    }

    #[test]
    fn test_decode_truncated_account_code_hash() {
        // Contract account, but code_hash truncated
        let mut data = vec![0x01, 0x00, 0x00, 0x00]; // entry_count = 1
        let header = ACCT_EXISTS_BIT | ACCT_CODE_HASH_BIT;
        data.push(header);
        data.extend_from_slice(&[0xCC; 20]); // only 20 bytes, need 32
        assert!(decode_read_log(&data).is_err());
    }

    #[test]
    fn test_decode_truncated_storage_value() {
        // Storage header says 8 bytes, but only 4 available
        let mut data = vec![0x01, 0x00, 0x00, 0x00]; // entry_count = 1
        data.push(HEADER_STORAGE_BASE + 8); // 8 bytes of value
        data.extend_from_slice(&[0xFF; 4]); // only 4 bytes
        assert!(decode_read_log(&data).is_err());
    }

    #[test]
    fn test_decode_truncated_block_hash() {
        // BlockHash header but not enough hash bytes
        let mut data = vec![0x01, 0x00, 0x00, 0x00]; // entry_count = 1
        data.push(HEADER_BLOCK_HASH);
        data.extend_from_slice(&[0xAB; 16]); // only 16 bytes, need 32
        assert!(decode_read_log(&data).is_err());
    }

    // ── Multiple entries of same type ──

    #[test]
    fn test_multiple_account_not_found() {
        let log = vec![
            ReadLogEntry::AccountNotFound,
            ReadLogEntry::AccountNotFound,
            ReadLogEntry::AccountNotFound,
        ];
        let encoded = encode_read_log(&log);
        let decoded = decode_read_log(&encoded).unwrap();
        assert_eq!(decoded, log);
        // 4 + 3 = 7 (3 × 1B header)
        assert_eq!(encoded.len(), 7);
    }

    // ── Block hash with zero hash ──

    #[test]
    fn test_block_hash_zero() {
        let log = vec![ReadLogEntry::BlockHash(B256::ZERO)];
        let encoded = encode_read_log(&log);
        let decoded = decode_read_log(&encoded).unwrap();
        assert_eq!(decoded, log);
    }

    // ── significant_bytes helpers ──

    #[test]
    fn test_significant_bytes_u64() {
        assert_eq!(significant_bytes_u64(0), 0);
        assert_eq!(significant_bytes_u64(1), 1);
        assert_eq!(significant_bytes_u64(255), 1);
        assert_eq!(significant_bytes_u64(256), 2);
        assert_eq!(significant_bytes_u64(0xFFFF), 2);
        assert_eq!(significant_bytes_u64(0x10000), 3);
        assert_eq!(significant_bytes_u64(0xFFFFFF), 3);
        assert_eq!(significant_bytes_u64(0x1000000), 4);
        assert_eq!(significant_bytes_u64(0x100000000), 5);
        assert_eq!(significant_bytes_u64(0x10000000000), 6);
        assert_eq!(significant_bytes_u64(0x1000000000000), 7);
        assert_eq!(significant_bytes_u64(0x100000000000000), 8);
        assert_eq!(significant_bytes_u64(u64::MAX), 8);
    }

    #[test]
    fn test_significant_bytes_be32() {
        let zero = [0u8; 32];
        assert_eq!(significant_bytes_be32(&zero), 0);

        let mut one = [0u8; 32];
        one[31] = 1;
        assert_eq!(significant_bytes_be32(&one), 1);

        let max = [0xFFu8; 32];
        assert_eq!(significant_bytes_be32(&max), 32);

        // Only first byte nonzero
        let mut first = [0u8; 32];
        first[0] = 0x01;
        assert_eq!(significant_bytes_be32(&first), 32);

        // Only middle byte nonzero
        let mut mid = [0u8; 32];
        mid[16] = 0x42;
        assert_eq!(significant_bytes_be32(&mid), 16);
    }

    // ── Trailing bytes tolerance ──

    #[test]
    fn test_decode_with_trailing_bytes() {
        // Empty log + trailing garbage — should decode successfully (forward compat)
        let mut data = vec![0x00, 0x00, 0x00, 0x00]; // entry_count = 0
        data.extend_from_slice(&[0xFF; 100]); // trailing bytes
        let decoded = decode_read_log(&data).unwrap();
        assert!(decoded.is_empty());
    }

    // ── ReadLogDatabase capture behavior ──

    #[test]
    fn test_read_log_database_capture() {
        use revm::{database::CacheDB, database_interface::Database};
        use alloy_primitives::Address;

        let mut cache_db = CacheDB::new(revm::database::EmptyDB::default());
        let addr = Address::with_last_byte(0x42);
        cache_db.insert_account_info(addr, AccountInfo {
            nonce: 5,
            balance: U256::from(1000u64),
            ..Default::default()
        });

        let mut logged_db = ReadLogDatabase::new(cache_db);

        // basic() should be logged
        let info = logged_db.basic(addr).unwrap().unwrap();
        assert_eq!(info.nonce, 5);

        // basic() on non-existent should log AccountNotFound
        let none = logged_db.basic(Address::ZERO).unwrap();
        assert!(none.is_none());

        // storage() should be logged
        let _val = logged_db.storage(addr, U256::ZERO.into()).unwrap();

        // block_hash() should be logged
        let _hash = logged_db.block_hash(0).unwrap();

        // Verify log contents
        let log = logged_db.log_handle();
        let entries = log.lock().unwrap();
        assert_eq!(entries.len(), 4);
        assert!(matches!(entries[0], ReadLogEntry::Account { nonce: 5, .. }));
        assert!(matches!(entries[1], ReadLogEntry::AccountNotFound));
        assert!(matches!(entries[2], ReadLogEntry::Storage(_)));
        assert!(matches!(entries[3], ReadLogEntry::BlockHash(_)));
    }

    #[test]
    fn test_read_log_database_code_not_logged() {
        use revm::{database::CacheDB, database_interface::Database};

        let cache_db = CacheDB::new(revm::database::EmptyDB::default());
        let mut logged_db = ReadLogDatabase::new(cache_db);

        let _ = logged_db.code_by_hash(KECCAK256_EMPTY);

        let handle = logged_db.log_handle();
        let entries = handle.lock().unwrap();
        assert!(entries.is_empty(), "code_by_hash should not produce log entries");
    }

    #[test]
    fn test_read_log_database_captures_bytecode() {
        use revm::{database::CacheDB, database_interface::Database, bytecode::Bytecode};
        use alloy_primitives::{Address, keccak256};

        let mut cache_db = CacheDB::new(revm::database::EmptyDB::default());
        let addr = Address::with_last_byte(0xC0);
        let code = alloy_primitives::Bytes::from(vec![0x60, 0x00, 0xF3]);
        let code_hash = keccak256(&code);

        cache_db.insert_account_info(addr, AccountInfo {
            nonce: 1,
            balance: U256::ZERO,
            code_hash,
            code: Some(Bytecode::new_raw(code.clone())),
            account_id: None,
        });

        let mut logged_db = ReadLogDatabase::new(cache_db);
        let _ = logged_db.basic(addr).unwrap();

        let codes = logged_db.codes_handle();
        let codes = codes.lock().unwrap();
        assert_eq!(codes.len(), 1, "should capture contract bytecode");
        assert!(codes.contains_key(&code_hash));
    }
}
