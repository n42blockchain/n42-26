use crate::{code_cache::CodeCache, packet::VerificationPacket};
use alloy_consensus::Header;
use alloy_eips::Decodable2718;
use alloy_primitives::{B256, KECCAK256_EMPTY};
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
    database_interface::EmptyDB,
    state::AccountInfo,
};
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
}
