use crate::{
    code_cache::CodeCache,
    packet::{StreamPacket, VerificationPacket},
};
use alloy_consensus::Header;
use alloy_eips::Decodable2718;
use alloy_primitives::{Address, B256, KECCAK256_EMPTY, U256};
use alloy_rlp::Decodable;
use n42_execution::{read_log::ReadLogEntry, N42EvmConfig};
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

/// Re-executes a block using the reth EVM and verifies the receipts root.
///
/// This is the core mobile verification function. It takes a `VerificationPacket`
/// containing the block header, transactions, and witness data, reconstructs the
/// EVM state, re-executes all transactions, and checks whether the computed
/// receipts root matches the expected value from the block header.
///
/// # Arguments
///
/// * `packet` - The verification packet from the IDC node
/// * `code_cache` - Local bytecode cache (read for existing codes)
/// * `chain_spec` - Chain specification for EVM configuration
///
/// # Returns
///
/// `VerificationResult` indicating whether the receipts root matches.
pub fn verify_block(
    packet: &VerificationPacket,
    code_cache: &mut CodeCache,
    chain_spec: Arc<ChainSpec>,
) -> Result<VerificationResult, VerifyError> {
    // 1. Decode header from RLP and verify hash
    let header = Header::decode(&mut &packet.header_rlp[..])?;
    let sealed_header = SealedHeader::seal_slow(header);

    if sealed_header.hash() != packet.block_hash {
        return Err(VerifyError::HeaderHashMismatch {
            expected: packet.block_hash,
            actual: sealed_header.hash(),
        });
    }

    // 2. Decode transactions from EIP-2718 envelopes
    let txs: Vec<EthTx> = packet
        .transactions
        .iter()
        .map(|rlp| EthTx::decode_2718(&mut &rlp[..]))
        .collect::<Result<_, _>>()
        .map_err(VerifyError::TxDecode)?;

    // 3. Assemble the block and recover senders
    let body = alloy_consensus::BlockBody {
        transactions: txs,
        ommers: vec![],
        withdrawals: None,
    };
    let block = SealedBlock::from_sealed_parts(sealed_header, body);
    let recovered =
        RecoveredBlock::try_recover_sealed(block).map_err(|_| VerifyError::SenderRecovery)?;

    // 4. Build CachedReads from witness + code_cache + uncached_bytecodes
    let mut cached_reads = build_cached_reads(packet, code_cache)?;

    // 5. Execute the block using the same reth EVM as the main node.
    //    EmptyDB serves as fallback — all needed state is in CachedReads.
    let evm_config = N42EvmConfig::new(chain_spec);
    let db = cached_reads.as_db_mut(EmptyDB::default());
    let mut executor = evm_config.executor(db);
    let result = executor.execute_one(&recovered)?;

    // 6. Compute receipts root and compare
    let computed = Receipt::calculate_receipt_root_no_memo(&result.receipts);

    Ok(VerificationResult {
        receipts_root_match: computed == packet.receipts_root,
        computed_receipts_root: computed,
    })
}

/// Builds a `CachedReads` database from the verification packet's witness data.
///
/// Combines account state from the witness, bytecodes from the local cache and
/// the packet's uncached_bytecodes, and ancestor block hashes for the BLOCKHASH
/// opcode.
fn build_cached_reads(
    packet: &VerificationPacket,
    code_cache: &mut CodeCache,
) -> Result<CachedReads, VerifyError> {
    let mut cached = CachedReads::default();

    // Build O(1) lookup table for uncached bytecodes (avoids O(N*M) linear scan).
    let uncached_map: std::collections::HashMap<&B256, &alloy_primitives::Bytes> = packet
        .uncached_bytecodes
        .iter()
        .map(|(h, b)| (h, b))
        .collect();

    for wa in &packet.witness_accounts {
        // Resolve bytecode: code_cache → uncached_bytecodes → None (EOA)
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

        let storage = wa.storage.iter().cloned().collect();
        cached.insert_account(wa.address, info, storage);
    }

    // Ancestor block hashes for BLOCKHASH opcode
    for (num, hash) in &packet.block_hashes {
        cached.block_hashes.insert(*num, *hash);
    }

    Ok(cached)
}

/// Updates the local code cache with newly received bytecodes after successful verification.
///
/// Call this after `verify_block()` succeeds to reduce future packet sizes —
/// bytecodes cached locally will be excluded from subsequent packets by the IDC node.
pub fn update_cache_after_verify(packet: &VerificationPacket, code_cache: &mut CodeCache) {
    for (hash, code) in &packet.uncached_bytecodes {
        code_cache.insert(*hash, code.clone());
    }
}

/// Updates the local code cache with bytecodes from a `StreamPacket`.
pub fn update_cache_after_stream_verify(packet: &StreamPacket, code_cache: &mut CodeCache) {
    for (hash, code) in &packet.bytecodes {
        code_cache.insert(*hash, code.clone());
    }
}

// ─── StreamReplayDB ─────────────────────────────────────────────────────────

/// Error type for StreamReplayDB — implements DBErrorMarker.
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

/// Sequential replay database for mobile verification.
///
/// Consumes `ReadLogEntry` values in order, ignoring address/slot/number arguments.
/// This is safe because:
/// - Same block + same txs → same revm Database call sequence (deterministic)
/// - If execution order diverges → wrong values returned → receipts_root mismatch → fail-safe
///
/// ## Code handling difference (IDC vs phone)
///
/// - IDC: `basic()` returns AccountInfo with `code=Some(...)` → EVM uses directly, no `code_by_hash` call
/// - Phone: stream Account has no code → AccountInfo with `code=None` → EVM calls `code_by_hash` → lookup from bytecodes map
/// - Both sides have identical basic/storage/block_hash call counts and order; `code_by_hash` doesn't consume cursor
#[derive(Debug)]
pub struct StreamReplayDB {
    entries: Vec<ReadLogEntry>,
    cursor: usize,
    bytecodes: HashMap<B256, Bytecode>,
}

impl StreamReplayDB {
    /// Creates a new replay DB from decoded read log entries and bytecodes.
    pub fn new(entries: Vec<ReadLogEntry>, bytecodes: HashMap<B256, Bytecode>) -> Self {
        Self {
            entries,
            cursor: 0,
            bytecodes,
        }
    }

    /// Returns the current cursor position (number of entries consumed).
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Returns true if all entries have been consumed.
    pub fn is_exhausted(&self) -> bool {
        self.cursor >= self.entries.len()
    }

    fn next_entry(&mut self) -> Result<&ReadLogEntry, StreamDbError> {
        if self.cursor >= self.entries.len() {
            return Err(StreamDbError::Exhausted(self.cursor));
        }
        let entry = &self.entries[self.cursor];
        self.cursor += 1;
        Ok(entry)
    }
}

impl revm::database_interface::Database for StreamReplayDB {
    type Error = StreamDbError;

    fn basic(&mut self, _address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        let cursor = self.cursor;
        let entry = self.next_entry()?;
        match entry {
            ReadLogEntry::AccountNotFound => Ok(None),
            ReadLogEntry::Account { nonce, balance, code_hash } => {
                // Construct AccountInfo without code — EVM will call code_by_hash when needed.
                Ok(Some(AccountInfo {
                    nonce: *nonce,
                    balance: *balance,
                    code_hash: *code_hash,
                    account_id: None,
                    code: None,
                }))
            }
            ReadLogEntry::Storage(_) => Err(StreamDbError::UnexpectedEntry {
                cursor,
                expected: "Account or AccountNotFound",
                actual: "Storage",
            }),
            ReadLogEntry::BlockHash(_) => Err(StreamDbError::UnexpectedEntry {
                cursor,
                expected: "Account or AccountNotFound",
                actual: "BlockHash",
            }),
        }
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        // Does NOT consume cursor — bytecodes are in a separate lookup map.
        self.bytecodes
            .get(&code_hash)
            .cloned()
            .ok_or(StreamDbError::MissingCode(code_hash))
    }

    fn storage(
        &mut self,
        _address: Address,
        _index: U256,
    ) -> Result<U256, Self::Error> {
        let cursor = self.cursor;
        let entry = self.next_entry()?;
        match entry {
            ReadLogEntry::Storage(value) => Ok(*value),
            ReadLogEntry::AccountNotFound | ReadLogEntry::Account { .. } => {
                Err(StreamDbError::UnexpectedEntry {
                    cursor,
                    expected: "Storage",
                    actual: "Account",
                })
            }
            ReadLogEntry::BlockHash(_) => Err(StreamDbError::UnexpectedEntry {
                cursor,
                expected: "Storage",
                actual: "BlockHash",
            }),
        }
    }

    fn block_hash(&mut self, _number: u64) -> Result<B256, Self::Error> {
        let cursor = self.cursor;
        let entry = self.next_entry()?;
        match entry {
            ReadLogEntry::BlockHash(hash) => Ok(*hash),
            ReadLogEntry::AccountNotFound | ReadLogEntry::Account { .. } => {
                Err(StreamDbError::UnexpectedEntry {
                    cursor,
                    expected: "BlockHash",
                    actual: "Account",
                })
            }
            ReadLogEntry::Storage(_) => Err(StreamDbError::UnexpectedEntry {
                cursor,
                expected: "BlockHash",
                actual: "Storage",
            }),
        }
    }
}

/// Re-executes a block using the V2 stream-based verification packet.
///
/// This is the new verification entry point for `StreamPacket`. Instead of
/// building a keyed CachedReads database, it uses `StreamReplayDB` to replay
/// state reads in the exact order they occurred during IDC execution.
///
/// # Arguments
///
/// * `packet` - The stream packet from the IDC node
/// * `code_cache` - Local bytecode cache (for resolving code_by_hash)
/// * `chain_spec` - Chain specification for EVM configuration
pub fn verify_block_stream(
    packet: &StreamPacket,
    code_cache: &mut CodeCache,
    chain_spec: Arc<ChainSpec>,
) -> Result<VerificationResult, VerifyError> {
    // 1. Decode header from RLP and verify hash
    let header = Header::decode(&mut &packet.header_rlp[..])?;
    let sealed_header = SealedHeader::seal_slow(header);

    if sealed_header.hash() != packet.block_hash {
        return Err(VerifyError::HeaderHashMismatch {
            expected: packet.block_hash,
            actual: sealed_header.hash(),
        });
    }

    // 2. Decode transactions from EIP-2718 envelopes
    let txs: Vec<EthTx> = packet
        .transactions
        .iter()
        .map(|rlp| EthTx::decode_2718(&mut &rlp[..]))
        .collect::<Result<_, _>>()
        .map_err(VerifyError::TxDecode)?;

    // 3. Assemble the block and recover senders
    let body = alloy_consensus::BlockBody {
        transactions: txs,
        ommers: vec![],
        withdrawals: None,
    };
    let block = SealedBlock::from_sealed_parts(sealed_header, body);
    let recovered =
        RecoveredBlock::try_recover_sealed(block).map_err(|_| VerifyError::SenderRecovery)?;

    // 4. Build bytecodes map: packet.bytecodes + code_cache
    let mut bytecodes_map: HashMap<B256, Bytecode> = HashMap::new();
    for (hash, code) in &packet.bytecodes {
        bytecodes_map.insert(*hash, Bytecode::new_raw(code.clone()));
    }
    // Also include codes from local cache
    for hash in code_cache.cached_hashes() {
        if !bytecodes_map.contains_key(&hash) {
            if let Some(code) = code_cache.get(&hash) {
                bytecodes_map.insert(hash, Bytecode::new_raw(code.clone()));
            }
        }
    }

    // 5. Create StreamReplayDB and execute
    let db = StreamReplayDB::new(packet.read_log.clone(), bytecodes_map);
    let evm_config = N42EvmConfig::new(chain_spec);
    let mut executor = evm_config.executor(db);
    let result = executor.execute_one(&recovered)?;

    // 6. Compute receipts root and compare
    let expected_receipts_root = sealed_header_receipts_root(&packet.header_rlp);
    let computed = Receipt::calculate_receipt_root_no_memo(&result.receipts);

    Ok(VerificationResult {
        receipts_root_match: computed == expected_receipts_root,
        computed_receipts_root: computed,
    })
}

/// Extracts the receipts root from an RLP-encoded header.
fn sealed_header_receipts_root(header_rlp: &[u8]) -> B256 {
    use alloy_consensus::BlockHeader;
    if let Ok(header) = Header::decode(&mut &header_rlp[..]) {
        header.receipts_root()
    } else {
        B256::ZERO
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::WitnessAccount;
    use alloy_consensus::Header;
    use alloy_primitives::{keccak256, Address, Bytes, U256, KECCAK256_EMPTY};
    use alloy_rlp::Encodable;

    /// Helper: create a default Header, RLP-encode it, and compute its hash.
    fn make_sealed_header() -> (Header, Bytes, B256) {
        let header = Header::default();
        let mut buf = Vec::new();
        header.encode(&mut buf);
        let header_rlp = Bytes::from(buf);
        let sealed = reth_primitives_traits::SealedHeader::seal_slow(header.clone());
        let hash = sealed.hash();
        (header, header_rlp, hash)
    }

    /// Helper: create a minimal empty-block VerificationPacket with valid header.
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

    // ── build_cached_reads tests ──

    #[test]
    fn test_build_cached_reads_empty() {
        let packet = make_empty_block_packet();
        let mut cache = CodeCache::new(10);
        let result = build_cached_reads(&packet, &mut cache);
        assert!(result.is_ok(), "empty packet should build CachedReads successfully");
    }

    #[test]
    fn test_build_cached_reads_eoa_account() {
        let mut packet = make_empty_block_packet();
        packet.witness_accounts.push(WitnessAccount {
            address: Address::with_last_byte(0x01),
            nonce: 1,
            balance: U256::from(1000),
            code_hash: KECCAK256_EMPTY, // EOA
            storage: vec![],
        });

        let mut cache = CodeCache::new(10);
        let result = build_cached_reads(&packet, &mut cache);
        assert!(result.is_ok(), "EOA account (KECCAK_EMPTY code_hash) should not require bytecode");
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
        let result = build_cached_reads(&packet, &mut cache);
        assert!(result.is_ok(), "contract bytecode should be resolved from uncached_bytecodes");
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
        // Do NOT put code in uncached_bytecodes — it's in the local cache.

        let mut cache = CodeCache::new(10);
        cache.insert(code_hash, code);

        let result = build_cached_reads(&packet, &mut cache);
        assert!(result.is_ok(), "contract bytecode should be resolved from local CodeCache");
    }

    #[test]
    fn test_build_cached_reads_missing_bytecode_error() {
        let code_hash = B256::with_last_byte(0xFF); // not in cache or uncached

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
        assert!(result.is_err(), "missing bytecode should return error");
        let err = result.unwrap_err();
        assert!(
            matches!(err, VerifyError::MissingBytecode(h) if h == code_hash),
            "error should be MissingBytecode with correct hash"
        );
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

    // ── verify_block error path tests ──

    #[test]
    fn test_verify_block_header_hash_mismatch() {
        let mut packet = make_empty_block_packet();
        // Tamper with block_hash
        packet.block_hash = B256::with_last_byte(0xFF);

        let mut cache = CodeCache::new(10);
        let chain_spec = n42_chainspec::n42_dev_chainspec();
        let result = verify_block(&packet, &mut cache, chain_spec);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), VerifyError::HeaderHashMismatch { .. }));
    }

    #[test]
    fn test_verify_block_bad_header_rlp() {
        let mut packet = make_empty_block_packet();
        packet.header_rlp = Bytes::from(vec![0xFF, 0xFE, 0xFD]); // invalid RLP

        let mut cache = CodeCache::new(10);
        let chain_spec = n42_chainspec::n42_dev_chainspec();
        let result = verify_block(&packet, &mut cache, chain_spec);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), VerifyError::HeaderDecode(_)));
    }

    #[test]
    fn test_verify_block_bad_transaction() {
        let mut packet = make_empty_block_packet();
        // Add invalid transaction bytes
        packet.transactions.push(Bytes::from(vec![0xFF, 0xFE]));

        let mut cache = CodeCache::new(10);
        let chain_spec = n42_chainspec::n42_dev_chainspec();
        let result = verify_block(&packet, &mut cache, chain_spec);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), VerifyError::TxDecode(_)));
    }

    // ── update_cache_after_verify test ──

    #[test]
    fn test_update_cache_after_verify() {
        let code_a = Bytes::from(vec![0xAA; 100]);
        let hash_a = keccak256(&code_a);
        let code_b = Bytes::from(vec![0xBB; 200]);
        let hash_b = keccak256(&code_b);

        let mut packet = make_empty_block_packet();
        packet.uncached_bytecodes = vec![(hash_a, code_a.clone()), (hash_b, code_b.clone())];

        let mut cache = CodeCache::new(10);
        assert!(cache.is_empty());

        update_cache_after_verify(&packet, &mut cache);

        assert_eq!(cache.len(), 2);
        assert_eq!(cache.get(&hash_a).unwrap(), &code_a);
        assert_eq!(cache.get(&hash_b).unwrap(), &code_b);
    }

    // ── StreamReplayDB tests ──

    #[test]
    fn test_stream_replay_db_basic_account() {
        use revm::database_interface::Database;

        let entries = vec![
            ReadLogEntry::Account {
                nonce: 10,
                balance: U256::from(5000u64),
                code_hash: KECCAK256_EMPTY,
            },
        ];
        let mut db = StreamReplayDB::new(entries, HashMap::new());

        let info = db.basic(Address::ZERO).unwrap().expect("should return Some");
        assert_eq!(info.nonce, 10);
        assert_eq!(info.balance, U256::from(5000u64));
        assert_eq!(info.code_hash, KECCAK256_EMPTY);
        assert!(info.code.is_none()); // StreamReplayDB always returns code=None
        assert!(db.is_exhausted());
    }

    #[test]
    fn test_stream_replay_db_account_not_found() {
        use revm::database_interface::Database;

        let entries = vec![ReadLogEntry::AccountNotFound];
        let mut db = StreamReplayDB::new(entries, HashMap::new());

        let result = db.basic(Address::ZERO).unwrap();
        assert!(result.is_none());
        assert!(db.is_exhausted());
    }

    #[test]
    fn test_stream_replay_db_storage() {
        use revm::database_interface::Database;

        let entries = vec![ReadLogEntry::Storage(U256::from(42u64))];
        let mut db = StreamReplayDB::new(entries, HashMap::new());

        let value = db.storage(Address::ZERO, U256::ZERO).unwrap();
        assert_eq!(value, U256::from(42u64));
        assert!(db.is_exhausted());
    }

    #[test]
    fn test_stream_replay_db_block_hash() {
        use revm::database_interface::Database;

        let hash = B256::from([0xAB; 32]);
        let entries = vec![ReadLogEntry::BlockHash(hash)];
        let mut db = StreamReplayDB::new(entries, HashMap::new());

        let result = db.block_hash(100).unwrap();
        assert_eq!(result, hash);
        assert!(db.is_exhausted());
    }

    #[test]
    fn test_stream_replay_db_exhausted_error() {
        use revm::database_interface::Database;

        let entries = vec![];
        let mut db = StreamReplayDB::new(entries, HashMap::new());

        let result = db.basic(Address::ZERO);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), StreamDbError::Exhausted(0)));
    }

    #[test]
    fn test_stream_replay_db_unexpected_entry_type() {
        use revm::database_interface::Database;

        // Expect Account but get Storage
        let entries = vec![ReadLogEntry::Storage(U256::ZERO)];
        let mut db = StreamReplayDB::new(entries, HashMap::new());

        let result = db.basic(Address::ZERO);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), StreamDbError::UnexpectedEntry { .. }));
    }

    #[test]
    fn test_stream_replay_db_code_by_hash() {
        use revm::database_interface::Database;

        let code_hash = B256::from([0xCC; 32]);
        let bytecode = Bytecode::new_raw(Bytes::from(vec![0x60, 0x00]));
        let mut codes = HashMap::new();
        codes.insert(code_hash, bytecode.clone());

        let mut db = StreamReplayDB::new(vec![], codes);

        // code_by_hash doesn't consume cursor
        let result = db.code_by_hash(code_hash).unwrap();
        assert_eq!(result.original_byte_slice(), bytecode.original_byte_slice());
        assert_eq!(db.cursor(), 0); // not advanced
    }

    #[test]
    fn test_stream_replay_db_mixed_sequence() {
        use revm::database_interface::Database;

        let entries = vec![
            ReadLogEntry::Account {
                nonce: 1,
                balance: U256::from(100u64),
                code_hash: KECCAK256_EMPTY,
            },
            ReadLogEntry::Storage(U256::from(42u64)),
            ReadLogEntry::AccountNotFound,
            ReadLogEntry::BlockHash(B256::from([0xBB; 32])),
        ];
        let mut db = StreamReplayDB::new(entries, HashMap::new());

        // Consume in order
        let info = db.basic(Address::ZERO).unwrap().unwrap();
        assert_eq!(info.nonce, 1);

        let val = db.storage(Address::ZERO, U256::ZERO).unwrap();
        assert_eq!(val, U256::from(42u64));

        let none = db.basic(Address::ZERO).unwrap();
        assert!(none.is_none());

        let hash = db.block_hash(0).unwrap();
        assert_eq!(hash, B256::from([0xBB; 32]));

        assert!(db.is_exhausted());
    }

    // ── verify_block_stream error path tests ──

    #[test]
    fn test_verify_block_stream_header_hash_mismatch() {
        let (_, header_rlp, _) = make_sealed_header();
        let packet = StreamPacket {
            block_hash: B256::with_last_byte(0xFF), // wrong hash
            header_rlp,
            transactions: vec![],
            read_log: vec![],
            bytecodes: vec![],
        };

        let mut cache = CodeCache::new(10);
        let chain_spec = n42_chainspec::n42_dev_chainspec();
        let result = verify_block_stream(&packet, &mut cache, chain_spec);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), VerifyError::HeaderHashMismatch { .. }));
    }

    #[test]
    fn test_verify_block_stream_bad_header_rlp() {
        let packet = StreamPacket {
            block_hash: B256::ZERO,
            header_rlp: Bytes::from(vec![0xFF, 0xFE, 0xFD]),
            transactions: vec![],
            read_log: vec![],
            bytecodes: vec![],
        };

        let mut cache = CodeCache::new(10);
        let chain_spec = n42_chainspec::n42_dev_chainspec();
        let result = verify_block_stream(&packet, &mut cache, chain_spec);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), VerifyError::HeaderDecode(_)));
    }

    #[test]
    fn test_update_cache_after_stream_verify() {
        let (_, header_rlp, block_hash) = make_sealed_header();
        let code = Bytes::from(vec![0xAA; 100]);
        let code_hash = keccak256(&code);

        let packet = StreamPacket {
            block_hash,
            header_rlp,
            transactions: vec![],
            read_log: vec![],
            bytecodes: vec![(code_hash, code.clone())],
        };

        let mut cache = CodeCache::new(10);
        update_cache_after_stream_verify(&packet, &mut cache);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.get(&code_hash).unwrap(), &code);
    }
}
