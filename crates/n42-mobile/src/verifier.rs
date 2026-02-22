use crate::{
    code_cache::CodeCache,
    packet::{StreamPacket, VerificationPacket},
};
use alloy_consensus::Header;
use alloy_eips::Decodable2718;
use alloy_primitives::{Address, B256, KECCAK256_EMPTY, U256};
use alloy_rlp::Decodable;
use n42_execution::N42EvmConfig;
use reth_chainspec::ChainSpec;
use reth_ethereum_primitives::{EthPrimitives, Receipt};
use reth_evm::{
    execute::{BlockExecutionError, Executor},
    ConfigureEvm,
};
use reth_primitives_traits::{NodePrimitives, RecoveredBlock, SealedBlock, SealedHeader};
use reth_revm::cached::CachedReads;
use revm::{
    bytecode::Bytecode,
    database_interface::{DBErrorMarker, EmptyDB},
    state::AccountInfo,
};
use std::collections::HashMap;
use std::sync::Arc;

/// The signed transaction type used by Ethereum primitives.
type EthTx = <EthPrimitives as NodePrimitives>::SignedTx;

/// Block verification result.
#[derive(Debug, Clone)]
pub struct VerificationResult {
    /// Whether the computed receipts root matches the expected value.
    pub receipts_root_match: bool,
    /// The computed receipts root from re-execution.
    pub computed_receipts_root: B256,
}

/// Errors during block verification.
#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    #[error("header RLP decode failed: {0}")]
    HeaderDecode(#[from] alloy_rlp::Error),
    #[error("header hash mismatch: expected {expected}, got {actual}")]
    HeaderHashMismatch { expected: B256, actual: B256 },
    #[error("transaction decode failed: {0}")]
    TxDecode(alloy_eips::eip2718::Eip2718Error),
    #[error("sender recovery failed")]
    SenderRecovery,
    #[error("block execution failed: {0}")]
    Execution(#[from] BlockExecutionError),
    #[error("missing bytecode for code_hash {0}")]
    MissingBytecode(B256),
    #[error("stream read log exhausted at cursor {0} (expected more entries)")]
    StreamExhausted(usize),
    #[error("unexpected entry type at cursor {cursor}: expected {expected}, got {actual}")]
    UnexpectedEntryType {
        cursor: usize,
        expected: &'static str,
        actual: &'static str,
    },
}

/// Decodes the header RLP and verifies the hash matches `expected_hash`.
fn decode_and_verify_header(
    header_rlp: &[u8],
    expected_hash: B256,
) -> Result<SealedHeader, VerifyError> {
    let header = Header::decode(&mut &header_rlp[..])?;
    let sealed = SealedHeader::seal_slow(header);
    if sealed.hash() != expected_hash {
        return Err(VerifyError::HeaderHashMismatch {
            expected: expected_hash,
            actual: sealed.hash(),
        });
    }
    Ok(sealed)
}

/// Decodes EIP-2718 transactions and assembles a recovered block.
fn decode_and_recover(
    sealed_header: SealedHeader,
    transactions: &[alloy_primitives::Bytes],
) -> Result<RecoveredBlock<reth_ethereum_primitives::Block>, VerifyError> {
    let txs: Vec<EthTx> = transactions
        .iter()
        .map(|rlp| EthTx::decode_2718(&mut &rlp[..]))
        .collect::<Result<_, _>>()
        .map_err(VerifyError::TxDecode)?;

    let body = alloy_consensus::BlockBody {
        transactions: txs,
        ommers: vec![],
        withdrawals: None,
    };
    let block = SealedBlock::from_sealed_parts(sealed_header, body);
    RecoveredBlock::try_recover_sealed(block).map_err(|_| VerifyError::SenderRecovery)
}

/// Re-executes a block using the V1 `VerificationPacket` and verifies the receipts root.
pub fn verify_block(
    packet: &VerificationPacket,
    code_cache: &mut CodeCache,
    chain_spec: Arc<ChainSpec>,
) -> Result<VerificationResult, VerifyError> {
    let sealed_header = decode_and_verify_header(&packet.header_rlp, packet.block_hash)?;
    let recovered = decode_and_recover(sealed_header, &packet.transactions)?;
    let mut cached_reads = build_cached_reads(packet, code_cache)?;

    let evm_config = N42EvmConfig::new(chain_spec);
    let db = cached_reads.as_db_mut(EmptyDB::default());
    let result = evm_config.executor(db).execute_one(&recovered)?;
    let computed = Receipt::calculate_receipt_root_no_memo(&result.receipts);

    Ok(VerificationResult {
        receipts_root_match: computed == packet.receipts_root,
        computed_receipts_root: computed,
    })
}

/// Builds a `CachedReads` database from the V1 packet's witness data.
fn build_cached_reads(
    packet: &VerificationPacket,
    code_cache: &mut CodeCache,
) -> Result<CachedReads, VerifyError> {
    let mut cached = CachedReads::default();

    let uncached_map: HashMap<&B256, &alloy_primitives::Bytes> =
        packet.uncached_bytecodes.iter().map(|(h, b)| (h, b)).collect();

    for wa in &packet.witness_accounts {
        let bytecode = if wa.code_hash == KECCAK256_EMPTY {
            None
        } else {
            let code = code_cache
                .get(&wa.code_hash)
                .cloned()
                .or_else(|| uncached_map.get(&wa.code_hash).map(|b| (*b).clone()));
            match code {
                Some(b) => Some(Bytecode::new_raw(b)),
                None => return Err(VerifyError::MissingBytecode(wa.code_hash)),
            }
        };

        let info = AccountInfo {
            balance: wa.balance,
            nonce: wa.nonce,
            code_hash: wa.code_hash,
            account_id: None,
            code: bytecode,
        };
        cached.insert_account(wa.address, info, wa.storage.iter().cloned().collect());
    }

    for (num, hash) in &packet.block_hashes {
        cached.block_hashes.insert(*num, *hash);
    }

    Ok(cached)
}

/// Updates the code cache with new bytecodes from a V1 packet after successful verification.
pub fn update_cache_after_verify(packet: &VerificationPacket, code_cache: &mut CodeCache) {
    for (hash, code) in &packet.uncached_bytecodes {
        code_cache.insert(*hash, code.clone());
    }
}

/// Updates the code cache with bytecodes from a V2 `StreamPacket`.
pub fn update_cache_after_stream_verify(packet: &StreamPacket, code_cache: &mut CodeCache) {
    for (hash, code) in &packet.bytecodes {
        code_cache.insert(*hash, code.clone());
    }
}

// ─── StreamReplayDB ─────────────────────────────────────────────────────────

/// Error type for `StreamReplayDB`.
#[derive(Debug, Clone, thiserror::Error)]
pub enum StreamDbError {
    #[error("stream exhausted at cursor {0}")]
    Exhausted(usize),
    #[error("unexpected entry at cursor {cursor}: expected {expected}, got {actual}")]
    UnexpectedEntry {
        cursor: usize,
        expected: &'static str,
        actual: &'static str,
    },
    #[error("missing bytecode for hash {0}")]
    MissingCode(B256),
}

impl DBErrorMarker for StreamDbError {}

/// Sequential replay database for mobile verification — zero-allocation cursor on raw bytes.
///
/// Parses read log entries directly from pre-encoded bytes on each `Database` call,
/// advancing a byte cursor. No intermediate `Vec<ReadLogEntry>` is ever allocated.
///
/// This is safe because same block + same txs → same revm `Database` call sequence
/// (deterministic). If execution order diverges → wrong values → receipts_root mismatch.
///
/// `code_by_hash` does NOT consume the cursor — bytecodes are in a separate lookup map.
/// It first checks `bytecodes` (from the current packet), then falls back to `code_cache`.
pub struct StreamReplayDB<'a> {
    data: Vec<u8>,
    pos: usize,
    entries_consumed: u32,
    entry_count: u32,
    bytecodes: HashMap<B256, Bytecode>,
    code_cache: Option<&'a mut CodeCache>,
}

impl std::fmt::Debug for StreamReplayDB<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamReplayDB")
            .field("data_len", &self.data.len())
            .field("pos", &self.pos)
            .field("entries_consumed", &self.entries_consumed)
            .field("entry_count", &self.entry_count)
            .field("bytecodes_count", &self.bytecodes.len())
            .field("has_code_cache", &self.code_cache.is_some())
            .finish()
    }
}

fn parse_stream_header(data: &[u8]) -> (u32, usize) {
    if data.len() >= 4 {
        (u32::from_le_bytes(data[0..4].try_into().unwrap()), 4)
    } else {
        (0, data.len())
    }
}

impl<'a> StreamReplayDB<'a> {
    /// Creates a new replay DB with a fallback code cache for lazy resolution.
    ///
    /// `data` is the raw output of `encode_read_log()` — starts with entry_count (4B) header.
    pub fn new(
        data: Vec<u8>,
        bytecodes: HashMap<B256, Bytecode>,
        code_cache: &'a mut CodeCache,
    ) -> Self {
        let (entry_count, pos) = parse_stream_header(&data);
        Self { data, pos, entries_consumed: 0, entry_count, bytecodes, code_cache: Some(code_cache) }
    }

    /// Creates a replay DB without a code cache fallback.
    ///
    /// Use when all needed bytecodes are already in the `bytecodes` map.
    pub fn new_without_cache(data: Vec<u8>, bytecodes: HashMap<B256, Bytecode>) -> Self {
        let (entry_count, pos) = parse_stream_header(&data);
        Self { data, pos, entries_consumed: 0, entry_count, bytecodes, code_cache: None }
    }

    /// Returns the number of entries consumed so far.
    pub fn cursor(&self) -> usize {
        self.entries_consumed as usize
    }

    /// Returns true if all entries have been consumed.
    pub fn is_exhausted(&self) -> bool {
        self.entries_consumed >= self.entry_count
    }
}

impl revm::database_interface::Database for StreamReplayDB<'_> {
    type Error = StreamDbError;

    fn basic(&mut self, _address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        use n42_execution::read_log::{
            ACCT_BALANCE_BIT, ACCT_CODE_HASH_BIT, ACCT_NONCE_8B_BIT, ACCT_NONCE_LEN_MASK,
            ACCT_NONCE_LEN_SHIFT, HEADER_ACCOUNT_NOT_FOUND, HEADER_STORAGE_BASE,
        };

        let cursor = self.entries_consumed as usize;
        if self.pos >= self.data.len() {
            return Err(StreamDbError::Exhausted(cursor));
        }
        let header = self.data[self.pos];
        self.pos += 1;
        self.entries_consumed += 1;

        if header == HEADER_ACCOUNT_NOT_FOUND {
            return Ok(None);
        }

        if header >= HEADER_STORAGE_BASE {
            return Err(StreamDbError::UnexpectedEntry {
                cursor,
                expected: "Account",
                actual: if header <= HEADER_STORAGE_BASE + 32 { "Storage" } else { "BlockHash" },
            });
        }

        let nonce_len = if header & ACCT_NONCE_8B_BIT != 0 {
            8
        } else {
            ((header & ACCT_NONCE_LEN_MASK) >> ACCT_NONCE_LEN_SHIFT) as usize
        };
        let has_balance = header & ACCT_BALANCE_BIT != 0;
        let has_code_hash = header & ACCT_CODE_HASH_BIT != 0;

        let nonce = if nonce_len > 0 {
            if self.pos + nonce_len > self.data.len() {
                return Err(StreamDbError::Exhausted(cursor));
            }
            let mut buf = [0u8; 8];
            buf[8 - nonce_len..].copy_from_slice(&self.data[self.pos..self.pos + nonce_len]);
            self.pos += nonce_len;
            u64::from_be_bytes(buf)
        } else {
            0
        };

        let balance = if has_balance {
            if self.pos >= self.data.len() {
                return Err(StreamDbError::Exhausted(cursor));
            }
            let balance_len = self.data[self.pos] as usize;
            self.pos += 1;
            if balance_len > 32 {
                return Err(StreamDbError::UnexpectedEntry {
                    cursor,
                    expected: "Account balance_len <= 32",
                    actual: "corrupted balance_len > 32",
                });
            }
            if self.pos + balance_len > self.data.len() {
                return Err(StreamDbError::Exhausted(cursor));
            }
            let mut buf = [0u8; 32];
            buf[32 - balance_len..].copy_from_slice(&self.data[self.pos..self.pos + balance_len]);
            self.pos += balance_len;
            U256::from_be_bytes(buf)
        } else {
            U256::ZERO
        };

        let code_hash = if has_code_hash {
            if self.pos + 32 > self.data.len() {
                return Err(StreamDbError::Exhausted(cursor));
            }
            let h = B256::from_slice(&self.data[self.pos..self.pos + 32]);
            self.pos += 32;
            h
        } else {
            KECCAK256_EMPTY
        };

        Ok(Some(AccountInfo { nonce, balance, code_hash, account_id: None, code: None }))
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        if let Some(code) = self.bytecodes.get(&code_hash) {
            return Ok(code.clone());
        }
        if let Some(cache) = self.code_cache.as_mut() {
            if let Some(code) = cache.get(&code_hash) {
                let bytecode = Bytecode::new_raw(code.clone());
                self.bytecodes.insert(code_hash, bytecode.clone());
                return Ok(bytecode);
            }
        }
        Err(StreamDbError::MissingCode(code_hash))
    }

    fn storage(&mut self, _address: Address, _index: U256) -> Result<U256, Self::Error> {
        use n42_execution::read_log::HEADER_STORAGE_BASE;

        let cursor = self.entries_consumed as usize;
        if self.pos >= self.data.len() {
            return Err(StreamDbError::Exhausted(cursor));
        }
        let header = self.data[self.pos];
        self.pos += 1;
        self.entries_consumed += 1;

        if !(HEADER_STORAGE_BASE..=HEADER_STORAGE_BASE + 32).contains(&header) {
            return Err(StreamDbError::UnexpectedEntry {
                cursor,
                expected: "Storage",
                actual: if header < HEADER_STORAGE_BASE { "Account" } else { "BlockHash" },
            });
        }

        let value_len = (header - HEADER_STORAGE_BASE) as usize;
        if value_len == 0 {
            return Ok(U256::ZERO);
        }

        if self.pos + value_len > self.data.len() {
            return Err(StreamDbError::Exhausted(cursor));
        }
        let mut buf = [0u8; 32];
        buf[32 - value_len..].copy_from_slice(&self.data[self.pos..self.pos + value_len]);
        self.pos += value_len;
        Ok(U256::from_be_bytes(buf))
    }

    fn block_hash(&mut self, _number: u64) -> Result<B256, Self::Error> {
        use n42_execution::read_log::HEADER_BLOCK_HASH;

        let cursor = self.entries_consumed as usize;
        if self.pos >= self.data.len() {
            return Err(StreamDbError::Exhausted(cursor));
        }
        let header = self.data[self.pos];
        self.pos += 1;
        self.entries_consumed += 1;

        if header != HEADER_BLOCK_HASH {
            return Err(StreamDbError::UnexpectedEntry {
                cursor,
                expected: "BlockHash",
                actual: if header < 0x80 { "Account" } else { "Storage" },
            });
        }

        if self.pos + 32 > self.data.len() {
            return Err(StreamDbError::Exhausted(cursor));
        }
        let h = B256::from_slice(&self.data[self.pos..self.pos + 32]);
        self.pos += 32;
        Ok(h)
    }
}

/// Re-executes a block using the V2 `StreamPacket` and verifies the receipts root.
///
/// Uses `StreamReplayDB` to replay state reads in the exact order they occurred
/// during IDC execution — no intermediate `Vec<ReadLogEntry>` allocation.
pub fn verify_block_stream(
    packet: &StreamPacket,
    code_cache: &mut CodeCache,
    chain_spec: Arc<ChainSpec>,
) -> Result<VerificationResult, VerifyError> {
    let sealed_header = decode_and_verify_header(&packet.header_rlp, packet.block_hash)?;
    let expected_receipts_root = {
        use alloy_consensus::BlockHeader;
        sealed_header.receipts_root()
    };
    let recovered = decode_and_recover(sealed_header, &packet.transactions)?;

    let bytecodes_map: HashMap<B256, Bytecode> = packet
        .bytecodes
        .iter()
        .map(|(hash, code)| (*hash, Bytecode::new_raw(code.clone())))
        .collect();

    let db = StreamReplayDB::new(packet.read_log_data.clone(), bytecodes_map, code_cache);
    let result = N42EvmConfig::new(chain_spec).executor(db).execute_one(&recovered)?;
    let computed = Receipt::calculate_receipt_root_no_memo(&result.receipts);

    Ok(VerificationResult {
        receipts_root_match: computed == expected_receipts_root,
        computed_receipts_root: computed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::WitnessAccount;
    use alloy_consensus::Header;
    use alloy_primitives::{keccak256, Address, Bytes, U256, KECCAK256_EMPTY};
    use alloy_rlp::Encodable;

    fn make_sealed_header() -> (Header, Bytes, B256) {
        let header = Header::default();
        let mut buf = Vec::new();
        header.encode(&mut buf);
        let header_rlp = Bytes::from(buf);
        let sealed = reth_primitives_traits::SealedHeader::seal_slow(header.clone());
        let hash = sealed.hash();
        (header, header_rlp, hash)
    }

    fn make_empty_block_packet() -> VerificationPacket {
        let (header, header_rlp, block_hash) = make_sealed_header();
        use alloy_consensus::BlockHeader;
        VerificationPacket {
            block_hash,
            block_number: header.number(),
            parent_hash: header.parent_hash(),
            state_root: header.state_root(),
            transactions_root: header.transactions_root(),
            receipts_root: header.receipts_root(),
            timestamp: header.timestamp(),
            gas_limit: header.gas_limit(),
            beneficiary: header.beneficiary(),
            header_rlp,
            transactions: vec![],
            witness_accounts: vec![],
            uncached_bytecodes: vec![],
            lowest_block_number: None,
            block_hashes: vec![],
        }
    }

    #[test]
    fn test_build_cached_reads_empty() {
        let packet = make_empty_block_packet();
        let mut cache = CodeCache::new(10);
        assert!(build_cached_reads(&packet, &mut cache).is_ok());
    }

    #[test]
    fn test_build_cached_reads_eoa_account() {
        let mut packet = make_empty_block_packet();
        packet.witness_accounts.push(WitnessAccount {
            address: Address::with_last_byte(0x01),
            nonce: 1,
            balance: U256::from(1000),
            code_hash: KECCAK256_EMPTY,
            storage: vec![],
        });
        let mut cache = CodeCache::new(10);
        assert!(build_cached_reads(&packet, &mut cache).is_ok());
    }

    #[test]
    fn test_build_cached_reads_contract_from_uncached() {
        let code = Bytes::from(vec![0x60, 0x00, 0x60, 0x00, 0xf3]);
        let code_hash = keccak256(&code);
        let mut packet = make_empty_block_packet();
        packet.witness_accounts.push(WitnessAccount {
            address: Address::with_last_byte(0x02),
            nonce: 0,
            balance: U256::ZERO,
            code_hash,
            storage: vec![],
        });
        packet.uncached_bytecodes.push((code_hash, code));
        let mut cache = CodeCache::new(10);
        assert!(build_cached_reads(&packet, &mut cache).is_ok());
    }

    #[test]
    fn test_build_cached_reads_contract_from_cache() {
        let code = Bytes::from(vec![0x60, 0x01, 0x60, 0x00, 0xf3]);
        let code_hash = keccak256(&code);
        let mut packet = make_empty_block_packet();
        packet.witness_accounts.push(WitnessAccount {
            address: Address::with_last_byte(0x03),
            nonce: 0,
            balance: U256::ZERO,
            code_hash,
            storage: vec![],
        });
        let mut cache = CodeCache::new(10);
        cache.insert(code_hash, code);
        assert!(build_cached_reads(&packet, &mut cache).is_ok());
    }

    #[test]
    fn test_build_cached_reads_missing_bytecode_error() {
        let code_hash = B256::with_last_byte(0xFF);
        let mut packet = make_empty_block_packet();
        packet.witness_accounts.push(WitnessAccount {
            address: Address::with_last_byte(0x04),
            nonce: 0,
            balance: U256::ZERO,
            code_hash,
            storage: vec![],
        });
        let mut cache = CodeCache::new(10);
        let result = build_cached_reads(&packet, &mut cache);
        assert!(matches!(result, Err(VerifyError::MissingBytecode(h)) if h == code_hash));
    }

    #[test]
    fn test_build_cached_reads_block_hashes() {
        let mut packet = make_empty_block_packet();
        let hash_100 = B256::with_last_byte(0x64);
        let hash_101 = B256::with_last_byte(0x65);
        packet.block_hashes = vec![(100, hash_100), (101, hash_101)];
        let mut cache = CodeCache::new(10);
        let cached = build_cached_reads(&packet, &mut cache).unwrap();
        assert_eq!(cached.block_hashes.get(&100), Some(&hash_100));
        assert_eq!(cached.block_hashes.get(&101), Some(&hash_101));
    }

    #[test]
    fn test_verify_block_header_hash_mismatch() {
        let mut packet = make_empty_block_packet();
        packet.block_hash = B256::with_last_byte(0xFF);
        let mut cache = CodeCache::new(10);
        let chain_spec = n42_chainspec::n42_dev_chainspec();
        let result = verify_block(&packet, &mut cache, chain_spec);
        assert!(matches!(result, Err(VerifyError::HeaderHashMismatch { .. })));
    }

    #[test]
    fn test_verify_block_bad_header_rlp() {
        let mut packet = make_empty_block_packet();
        packet.header_rlp = Bytes::from(vec![0xFF, 0xFE, 0xFD]);
        let mut cache = CodeCache::new(10);
        let chain_spec = n42_chainspec::n42_dev_chainspec();
        let result = verify_block(&packet, &mut cache, chain_spec);
        assert!(matches!(result, Err(VerifyError::HeaderDecode(_))));
    }

    #[test]
    fn test_verify_block_bad_transaction() {
        let mut packet = make_empty_block_packet();
        packet.transactions.push(Bytes::from(vec![0xFF, 0xFE]));
        let mut cache = CodeCache::new(10);
        let chain_spec = n42_chainspec::n42_dev_chainspec();
        let result = verify_block(&packet, &mut cache, chain_spec);
        assert!(matches!(result, Err(VerifyError::TxDecode(_))));
    }

    #[test]
    fn test_update_cache_after_verify() {
        let code_a = Bytes::from(vec![0xAA; 100]);
        let hash_a = keccak256(&code_a);
        let code_b = Bytes::from(vec![0xBB; 200]);
        let hash_b = keccak256(&code_b);
        let mut packet = make_empty_block_packet();
        packet.uncached_bytecodes = vec![(hash_a, code_a.clone()), (hash_b, code_b.clone())];
        let mut cache = CodeCache::new(10);
        update_cache_after_verify(&packet, &mut cache);
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.get(&hash_a).unwrap(), &code_a);
        assert_eq!(cache.get(&hash_b).unwrap(), &code_b);
    }

    fn encode_entries(entries: &[n42_execution::read_log::ReadLogEntry]) -> Vec<u8> {
        n42_execution::read_log::encode_read_log(entries)
    }

    #[test]
    fn test_stream_replay_db_basic_account() {
        use n42_execution::read_log::ReadLogEntry;
        use revm::database_interface::Database;

        let data = encode_entries(&[ReadLogEntry::Account {
            nonce: 10,
            balance: U256::from(5000u64),
            code_hash: KECCAK256_EMPTY,
        }]);
        let mut db = StreamReplayDB::new_without_cache(data, HashMap::new());
        let info = db.basic(Address::ZERO).unwrap().expect("should return Some");
        assert_eq!(info.nonce, 10);
        assert_eq!(info.balance, U256::from(5000u64));
        assert_eq!(info.code_hash, KECCAK256_EMPTY);
        assert!(info.code.is_none());
        assert!(db.is_exhausted());
    }

    #[test]
    fn test_stream_replay_db_account_not_found() {
        use n42_execution::read_log::ReadLogEntry;
        use revm::database_interface::Database;

        let data = encode_entries(&[ReadLogEntry::AccountNotFound]);
        let mut db = StreamReplayDB::new_without_cache(data, HashMap::new());
        assert!(db.basic(Address::ZERO).unwrap().is_none());
        assert!(db.is_exhausted());
    }

    #[test]
    fn test_stream_replay_db_storage() {
        use n42_execution::read_log::ReadLogEntry;
        use revm::database_interface::Database;

        let data = encode_entries(&[ReadLogEntry::Storage(U256::from(42u64))]);
        let mut db = StreamReplayDB::new_without_cache(data, HashMap::new());
        assert_eq!(db.storage(Address::ZERO, U256::ZERO).unwrap(), U256::from(42u64));
        assert!(db.is_exhausted());
    }

    #[test]
    fn test_stream_replay_db_block_hash() {
        use n42_execution::read_log::ReadLogEntry;
        use revm::database_interface::Database;

        let hash = B256::from([0xAB; 32]);
        let data = encode_entries(&[ReadLogEntry::BlockHash(hash)]);
        let mut db = StreamReplayDB::new_without_cache(data, HashMap::new());
        assert_eq!(db.block_hash(100).unwrap(), hash);
        assert!(db.is_exhausted());
    }

    #[test]
    fn test_stream_replay_db_exhausted_error() {
        use n42_execution::read_log::encode_read_log;
        use revm::database_interface::Database;

        let data = encode_read_log(&[]);
        let mut db = StreamReplayDB::new_without_cache(data, HashMap::new());
        assert!(matches!(db.basic(Address::ZERO), Err(StreamDbError::Exhausted(0))));
    }

    #[test]
    fn test_stream_replay_db_unexpected_entry_type() {
        use n42_execution::read_log::ReadLogEntry;
        use revm::database_interface::Database;

        let data = encode_entries(&[ReadLogEntry::Storage(U256::ZERO)]);
        let mut db = StreamReplayDB::new_without_cache(data, HashMap::new());
        assert!(matches!(db.basic(Address::ZERO), Err(StreamDbError::UnexpectedEntry { .. })));
    }

    #[test]
    fn test_stream_replay_db_code_by_hash() {
        use n42_execution::read_log::encode_read_log;
        use revm::database_interface::Database;

        let code_hash = B256::from([0xCC; 32]);
        let bytecode = Bytecode::new_raw(Bytes::from(vec![0x60, 0x00]));
        let mut codes = HashMap::new();
        codes.insert(code_hash, bytecode.clone());
        let data = encode_read_log(&[]);
        let mut db = StreamReplayDB::new_without_cache(data, codes);
        let result = db.code_by_hash(code_hash).unwrap();
        assert_eq!(result.original_byte_slice(), bytecode.original_byte_slice());
        assert_eq!(db.cursor(), 0);
    }

    #[test]
    fn test_stream_replay_db_mixed_sequence() {
        use n42_execution::read_log::ReadLogEntry;
        use revm::database_interface::Database;

        let data = encode_entries(&[
            ReadLogEntry::Account { nonce: 1, balance: U256::from(100u64), code_hash: KECCAK256_EMPTY },
            ReadLogEntry::Storage(U256::from(42u64)),
            ReadLogEntry::AccountNotFound,
            ReadLogEntry::BlockHash(B256::from([0xBB; 32])),
        ]);
        let mut db = StreamReplayDB::new_without_cache(data, HashMap::new());

        assert_eq!(db.basic(Address::ZERO).unwrap().unwrap().nonce, 1);
        assert_eq!(db.storage(Address::ZERO, U256::ZERO).unwrap(), U256::from(42u64));
        assert!(db.basic(Address::ZERO).unwrap().is_none());
        assert_eq!(db.block_hash(0).unwrap(), B256::from([0xBB; 32]));
        assert!(db.is_exhausted());
    }

    #[test]
    fn test_verify_block_stream_header_hash_mismatch() {
        use n42_execution::read_log::encode_read_log;
        let (_, header_rlp, _) = make_sealed_header();
        let packet = StreamPacket {
            block_hash: B256::with_last_byte(0xFF),
            header_rlp,
            transactions: vec![],
            read_log_data: encode_read_log(&[]),
            bytecodes: vec![],
        };
        let mut cache = CodeCache::new(10);
        let chain_spec = n42_chainspec::n42_dev_chainspec();
        let result = verify_block_stream(&packet, &mut cache, chain_spec);
        assert!(matches!(result, Err(VerifyError::HeaderHashMismatch { .. })));
    }

    #[test]
    fn test_verify_block_stream_bad_header_rlp() {
        use n42_execution::read_log::encode_read_log;
        let packet = StreamPacket {
            block_hash: B256::ZERO,
            header_rlp: Bytes::from(vec![0xFF, 0xFE, 0xFD]),
            transactions: vec![],
            read_log_data: encode_read_log(&[]),
            bytecodes: vec![],
        };
        let mut cache = CodeCache::new(10);
        let chain_spec = n42_chainspec::n42_dev_chainspec();
        let result = verify_block_stream(&packet, &mut cache, chain_spec);
        assert!(matches!(result, Err(VerifyError::HeaderDecode(_))));
    }

    #[test]
    fn test_update_cache_after_stream_verify() {
        use n42_execution::read_log::encode_read_log;
        let (_, header_rlp, block_hash) = make_sealed_header();
        let code = Bytes::from(vec![0xAA; 100]);
        let code_hash = keccak256(&code);
        let packet = StreamPacket {
            block_hash,
            header_rlp,
            transactions: vec![],
            read_log_data: encode_read_log(&[]),
            bytecodes: vec![(code_hash, code.clone())],
        };
        let mut cache = CodeCache::new(10);
        update_cache_after_stream_verify(&packet, &mut cache);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.get(&code_hash).unwrap(), &code);
    }

    #[test]
    fn test_stream_replay_db_storage_zero() {
        use n42_execution::read_log::ReadLogEntry;
        use revm::database_interface::Database;

        let data = encode_entries(&[ReadLogEntry::Storage(U256::ZERO)]);
        let mut db = StreamReplayDB::new_without_cache(data, HashMap::new());
        assert_eq!(db.storage(Address::ZERO, U256::ZERO).unwrap(), U256::ZERO);
        assert!(db.is_exhausted());
    }

    #[test]
    fn test_stream_replay_db_storage_max() {
        use n42_execution::read_log::ReadLogEntry;
        use revm::database_interface::Database;

        let data = encode_entries(&[ReadLogEntry::Storage(U256::MAX)]);
        let mut db = StreamReplayDB::new_without_cache(data, HashMap::new());
        assert_eq!(db.storage(Address::ZERO, U256::ZERO).unwrap(), U256::MAX);
    }

    #[test]
    fn test_stream_replay_db_contract_account() {
        use n42_execution::read_log::ReadLogEntry;
        use revm::database_interface::Database;

        let code_hash = B256::from([0xCC; 32]);
        let data = encode_entries(&[ReadLogEntry::Account {
            nonce: 1,
            balance: U256::from(5000u64),
            code_hash,
        }]);
        let mut db = StreamReplayDB::new_without_cache(data, HashMap::new());
        let info = db.basic(Address::ZERO).unwrap().expect("should return Some");
        assert_eq!(info.nonce, 1);
        assert_eq!(info.balance, U256::from(5000u64));
        assert_eq!(info.code_hash, code_hash);
        assert!(info.code.is_none());
    }

    #[test]
    fn test_stream_replay_db_large_nonce_balance() {
        use n42_execution::read_log::ReadLogEntry;
        use revm::database_interface::Database;

        let data = encode_entries(&[ReadLogEntry::Account {
            nonce: u64::MAX,
            balance: U256::MAX,
            code_hash: KECCAK256_EMPTY,
        }]);
        let mut db = StreamReplayDB::new_without_cache(data, HashMap::new());
        let info = db.basic(Address::ZERO).unwrap().unwrap();
        assert_eq!(info.nonce, u64::MAX);
        assert_eq!(info.balance, U256::MAX);
    }

    #[test]
    fn test_stream_replay_db_missing_code_error() {
        use n42_execution::read_log::encode_read_log;
        use revm::database_interface::Database;

        let missing_hash = B256::from([0xDD; 32]);
        let data = encode_read_log(&[]);
        let mut db = StreamReplayDB::new_without_cache(data, HashMap::new());
        assert!(matches!(db.code_by_hash(missing_hash), Err(StreamDbError::MissingCode(h)) if h == missing_hash));
    }

    #[test]
    fn test_stream_replay_db_code_by_hash_no_cursor_advance() {
        use n42_execution::read_log::ReadLogEntry;
        use revm::database_interface::Database;

        let code_hash = B256::from([0xCC; 32]);
        let bytecode = Bytecode::new_raw(Bytes::from(vec![0x60, 0x00, 0xF3]));
        let mut codes = HashMap::new();
        codes.insert(code_hash, bytecode);

        let data = encode_entries(&[ReadLogEntry::Account {
            nonce: 1,
            balance: U256::ZERO,
            code_hash: KECCAK256_EMPTY,
        }]);
        let mut db = StreamReplayDB::new_without_cache(data, codes);

        let _ = db.code_by_hash(code_hash).unwrap();
        let _ = db.code_by_hash(code_hash).unwrap();
        let _ = db.code_by_hash(code_hash).unwrap();
        assert_eq!(db.cursor(), 0);

        let info = db.basic(Address::ZERO).unwrap().unwrap();
        assert_eq!(info.nonce, 1);
        assert_eq!(db.cursor(), 1);
    }

    #[test]
    fn test_stream_replay_db_expect_storage_get_account() {
        use n42_execution::read_log::ReadLogEntry;
        use revm::database_interface::Database;

        let data = encode_entries(&[ReadLogEntry::Account {
            nonce: 0,
            balance: U256::ZERO,
            code_hash: KECCAK256_EMPTY,
        }]);
        let mut db = StreamReplayDB::new_without_cache(data, HashMap::new());
        assert!(matches!(
            db.storage(Address::ZERO, U256::ZERO),
            Err(StreamDbError::UnexpectedEntry { expected: "Storage", actual: "Account", .. })
        ));
    }

    #[test]
    fn test_stream_replay_db_expect_storage_get_blockhash() {
        use n42_execution::read_log::ReadLogEntry;
        use revm::database_interface::Database;

        let data = encode_entries(&[ReadLogEntry::BlockHash(B256::ZERO)]);
        let mut db = StreamReplayDB::new_without_cache(data, HashMap::new());
        assert!(matches!(
            db.storage(Address::ZERO, U256::ZERO),
            Err(StreamDbError::UnexpectedEntry { expected: "Storage", actual: "BlockHash", .. })
        ));
    }

    #[test]
    fn test_stream_replay_db_expect_blockhash_get_account() {
        use n42_execution::read_log::ReadLogEntry;
        use revm::database_interface::Database;

        let data = encode_entries(&[ReadLogEntry::AccountNotFound]);
        let mut db = StreamReplayDB::new_without_cache(data, HashMap::new());
        assert!(matches!(
            db.block_hash(0),
            Err(StreamDbError::UnexpectedEntry { expected: "BlockHash", actual: "Account", .. })
        ));
    }

    #[test]
    fn test_stream_replay_db_expect_blockhash_get_storage() {
        use n42_execution::read_log::ReadLogEntry;
        use revm::database_interface::Database;

        let data = encode_entries(&[ReadLogEntry::Storage(U256::from(42u64))]);
        let mut db = StreamReplayDB::new_without_cache(data, HashMap::new());
        assert!(matches!(
            db.block_hash(0),
            Err(StreamDbError::UnexpectedEntry { expected: "BlockHash", actual: "Storage", .. })
        ));
    }

    #[test]
    fn test_stream_replay_db_empty_data() {
        use revm::database_interface::Database;

        let mut db = StreamReplayDB::new_without_cache(vec![], HashMap::new());
        assert!(db.is_exhausted());
        assert!(db.basic(Address::ZERO).is_err());
    }

    #[test]
    fn test_stream_replay_db_corrupted_balance_len() {
        use revm::database_interface::Database;

        let mut data = vec![0x01, 0x00, 0x00, 0x00]; // entry_count = 1
        let header = n42_execution::read_log::ACCT_EXISTS_BIT
            | n42_execution::read_log::ACCT_BALANCE_BIT;
        data.push(header);
        data.push(33); // balance_len = 33 (invalid)
        data.extend_from_slice(&[0xFF; 33]);
        let mut db = StreamReplayDB::new_without_cache(data, HashMap::new());
        assert!(db.basic(Address::ZERO).is_err());
    }

    #[test]
    fn test_stream_replay_db_cursor_tracking() {
        use n42_execution::read_log::ReadLogEntry;
        use revm::database_interface::Database;

        let data = encode_entries(&[
            ReadLogEntry::AccountNotFound,
            ReadLogEntry::Storage(U256::from(1u64)),
            ReadLogEntry::BlockHash(B256::ZERO),
        ]);
        let mut db = StreamReplayDB::new_without_cache(data, HashMap::new());

        assert_eq!(db.cursor(), 0);
        assert!(!db.is_exhausted());
        db.basic(Address::ZERO).unwrap();
        assert_eq!(db.cursor(), 1);
        db.storage(Address::ZERO, U256::ZERO).unwrap();
        assert_eq!(db.cursor(), 2);
        db.block_hash(0).unwrap();
        assert_eq!(db.cursor(), 3);
        assert!(db.is_exhausted());
    }

    #[test]
    fn test_stream_replay_db_block_hash_zero() {
        use n42_execution::read_log::ReadLogEntry;
        use revm::database_interface::Database;

        let data = encode_entries(&[ReadLogEntry::BlockHash(B256::ZERO)]);
        let mut db = StreamReplayDB::new_without_cache(data, HashMap::new());
        assert_eq!(db.block_hash(0).unwrap(), B256::ZERO);
    }

    #[test]
    fn test_stream_replay_db_lazy_code_cache_fallback() {
        use n42_execution::read_log::ReadLogEntry;
        use revm::database_interface::Database;

        let code_hash = B256::from([0xCC; 32]);
        let code = Bytes::from(vec![0x60, 0x00, 0xF3]);
        let mut cache = CodeCache::new(10);
        cache.insert(code_hash, code.clone());

        let data = encode_entries(&[ReadLogEntry::Account {
            nonce: 1,
            balance: U256::ZERO,
            code_hash,
        }]);
        let mut db = StreamReplayDB::new(data, HashMap::new(), &mut cache);

        let info = db.basic(Address::ZERO).unwrap().unwrap();
        assert_eq!(info.code_hash, code_hash);
        assert!(info.code.is_none());

        let resolved = db.code_by_hash(code_hash).unwrap();
        assert_eq!(resolved.original_byte_slice(), &code[..]);
        assert_eq!(db.cursor(), 1);

        let resolved2 = db.code_by_hash(code_hash).unwrap();
        assert_eq!(resolved2.original_byte_slice(), &code[..]);
    }

    #[test]
    fn test_stream_replay_db_packet_code_takes_precedence() {
        use n42_execution::read_log::encode_read_log;
        use revm::database_interface::Database;

        let code_hash = B256::from([0xDD; 32]);
        let packet_code = Bytes::from(vec![0x60, 0x01]);
        let cache_code = Bytes::from(vec![0x60, 0x00]);

        let mut cache = CodeCache::new(10);
        cache.insert(code_hash, cache_code);

        let mut codes = HashMap::new();
        codes.insert(code_hash, Bytecode::new_raw(packet_code.clone()));

        let data = encode_read_log(&[]);
        let mut db = StreamReplayDB::new(data, codes, &mut cache);
        let resolved = db.code_by_hash(code_hash).unwrap();
        assert_eq!(resolved.original_byte_slice(), &packet_code[..]);
    }
}
