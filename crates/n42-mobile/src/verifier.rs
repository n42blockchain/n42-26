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

    for wa in &packet.witness_accounts {
        // Resolve bytecode: code_cache → uncached_bytecodes → None (EOA)
        let bytecode = if wa.code_hash == KECCAK256_EMPTY {
            None
        } else {
            let code = code_cache
                .get(&wa.code_hash)
                .cloned()
                .or_else(|| {
                    packet
                        .uncached_bytecodes
                        .iter()
                        .find(|(h, _)| *h == wa.code_hash)
                        .map(|(_, b)| b.clone())
                });
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
