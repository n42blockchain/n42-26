//! Integration tests: V2 stream-based verification pipeline end-to-end.
//!
//! Verifies that the complete pipeline works with real EVM execution:
//! 1. IDC side: ReadLogDatabase captures ordered reads during block execution
//! 2. Encode read log + bytecodes into StreamPacket
//! 3. Phone side: StreamReplayDB replays from raw bytes
//! 4. Both sides produce identical receipts → receipts_root match proves correctness

use alloy_consensus::{Header, TxLegacy};
use alloy_primitives::{keccak256, Address, Bytes, TxKind, B256, U256};
use n42_execution::read_log::{encode_read_log, ReadLogDatabase};
use n42_mobile::verifier::StreamReplayDB;
use reth_chainspec::{ChainSpecBuilder, EthereumHardfork, ForkCondition, MAINNET};
use reth_ethereum_primitives::{Block, BlockBody, Receipt, Transaction};
use reth_evm::execute::{BasicBlockExecutor, Executor};
use reth_evm_ethereum::EthEvmConfig;
use reth_primitives_traits::{crypto::secp256k1::public_key_to_address, RecoveredBlock};
use reth_testing_utils::generators::{self, sign_tx_with_key_pair};
use revm::{
    bytecode::Bytecode,
    database::CacheDB,
    state::AccountInfo,
};
use std::collections::HashMap;
use std::sync::Arc;

/// Minimal ERC-20 token contract: transfer(address,uint256) and balanceOf(address)
/// Compiled to EVM bytecode for deterministic testing.
fn minimal_erc20_bytecode() -> Bytes {
    let mut code = Vec::new();

    // Selector dispatch
    code.extend_from_slice(&[0x5F, 0x35, 0x60, 0xE0, 0x1C, 0x80]);

    let transfer_jump_pos = code.len() + 1;
    code.extend_from_slice(&[0x63, 0xa9, 0x05, 0x9c, 0xbb, 0x14, 0x61, 0x00, 0x00, 0x57, 0x80]);

    let balanceof_jump_pos = code.len() + 1;
    code.extend_from_slice(&[0x63, 0x70, 0xa0, 0x82, 0x31, 0x14, 0x61, 0x00, 0x00, 0x57, 0x00]);

    // transfer(address to, uint256 amount)
    let transfer_offset = code.len();
    code.extend_from_slice(&[
        0x5B, 0x50, 0x33, 0x5F, 0x52, 0x5F, 0x60, 0x20, 0x52, 0x60, 0x40, 0x5F, 0x20,
        0x80, 0x54, 0x60, 0x24, 0x35, 0x80, 0x82, 0x10,
    ]);

    let revert_jump_pos = code.len() + 1;
    code.extend_from_slice(&[0x61, 0x00, 0x00, 0x57, 0x90, 0x03, 0x90, 0x55]);
    code.extend_from_slice(&[0x60, 0x04, 0x35, 0x5F, 0x52, 0x5F, 0x60, 0x20, 0x52]);
    code.extend_from_slice(&[0x60, 0x40, 0x5F, 0x20, 0x80, 0x54, 0x60, 0x24, 0x35, 0x01, 0x90, 0x55]);
    code.extend_from_slice(&[0x60, 0x01, 0x5F, 0x52, 0x60, 0x20, 0x5F, 0xF3]);

    // balanceOf(address)
    let balanceof_offset = code.len();
    code.extend_from_slice(&[
        0x5B, 0x50, 0x60, 0x04, 0x35, 0x5F, 0x52, 0x5F, 0x60, 0x20, 0x52,
        0x60, 0x40, 0x5F, 0x20, 0x54, 0x5F, 0x52, 0x60, 0x20, 0x5F, 0xF3,
    ]);

    // revert label
    let revert_offset = code.len();
    code.extend_from_slice(&[0x5B, 0x5F, 0x5F, 0xFD]);

    // Patch jump targets
    let patch_offset = |code: &mut Vec<u8>, pos: usize, target: usize| {
        let be = (target as u16).to_be_bytes();
        code[pos] = be[0];
        code[pos + 1] = be[1];
    };
    patch_offset(&mut code, transfer_jump_pos, transfer_offset);
    patch_offset(&mut code, balanceof_jump_pos, balanceof_offset);
    patch_offset(&mut code, revert_jump_pos, revert_offset);

    Bytes::from(code)
}

/// Compute the Solidity mapping storage slot: keccak256(abi.encode(key, slot_index))
fn mapping_slot(key: Address, slot_index: u64) -> U256 {
    let mut buf = [0u8; 64];
    // left-pad address to 32 bytes
    buf[12..32].copy_from_slice(key.as_slice());
    // slot_index as uint256 in bytes 32..64 (big-endian in last bytes)
    buf[56..64].copy_from_slice(&slot_index.to_be_bytes());
    let hash = keccak256(buf);
    U256::from_be_bytes(hash.0)
}

/// Chain spec for tests: Shanghai activated (supports PUSH0), no Cancun complexity.
fn test_chain_spec() -> Arc<reth_chainspec::ChainSpec> {
    Arc::new(
        ChainSpecBuilder::from(&*MAINNET)
            .shanghai_activated()
            .with_fork(EthereumHardfork::Cancun, ForkCondition::Never)
            .build(),
    )
}

/// Build the ABI-encoded calldata for `transfer(address to, uint256 amount)`.
fn encode_transfer_call(to: Address, amount: U256) -> Bytes {
    let mut data = Vec::with_capacity(68);
    data.extend_from_slice(&[0xa9, 0x05, 0x9c, 0xbb]); // selector
    // to: left-padded to 32 bytes
    let mut to_padded = [0u8; 32];
    to_padded[12..32].copy_from_slice(to.as_slice());
    data.extend_from_slice(&to_padded);
    // amount: 32 bytes big-endian
    data.extend_from_slice(&amount.to_be_bytes::<32>());
    Bytes::from(data)
}

/// Execute a block using ReadLogDatabase, then replay using StreamReplayDB,
/// and verify that both produce identical receipts.
fn run_pipeline_test(
    base_db: CacheDB<revm::database::EmptyDB>,
    header: Header,
    transactions: Vec<reth_ethereum_primitives::TransactionSigned>,
    senders: Vec<Address>,
) {
    let chain_spec = test_chain_spec();

    // ── IDC side: execute with ReadLogDatabase ──
    let logged_db = ReadLogDatabase::new(base_db.clone());
    let log_handle = logged_db.log_handle();
    let codes_handle = logged_db.codes_handle();

    let evm_config = EthEvmConfig::new(chain_spec.clone());
    let mut idc_executor = BasicBlockExecutor::new(evm_config.clone(), logged_db);

    let block = RecoveredBlock::new_unhashed(
        Block {
            header: header.clone(),
            body: BlockBody {
                transactions: transactions.clone(),
                ommers: vec![],
                withdrawals: None,
            },
        },
        senders.clone(),
    );

    let idc_result = idc_executor.execute_one(&block).expect("IDC execution should succeed");

    // Extract read log and captured codes
    let read_log = match Arc::try_unwrap(log_handle) {
        Ok(mutex) => mutex.into_inner().unwrap(),
        Err(arc) => arc.lock().unwrap().clone(),
    };
    let captured_codes = match Arc::try_unwrap(codes_handle) {
        Ok(mutex) => mutex.into_inner().unwrap(),
        Err(arc) => arc.lock().unwrap().clone(),
    };

    let read_log_data = encode_read_log(&read_log);

    eprintln!("  Read log entries: {}", read_log.len());
    eprintln!("  Read log bytes:   {}", read_log_data.len());
    eprintln!("  Captured codes:   {}", captured_codes.len());
    eprintln!("  Receipts count:   {}", idc_result.receipts.len());
    for (i, entry) in read_log.iter().enumerate() {
        eprintln!("    [{:2}] {:?}", i, entry);
    }

    // ── Phone side: replay with StreamReplayDB ──
    let bytecodes_map: HashMap<B256, Bytecode> = captured_codes
        .into_iter()
        .map(|(h, c)| (h, Bytecode::new_raw(c)))
        .collect();

    let replay_db = StreamReplayDB::new_without_cache(read_log_data, bytecodes_map);
    let mut phone_executor = BasicBlockExecutor::new(evm_config, replay_db);

    let phone_result = phone_executor
        .execute_one(&block)
        .expect("Phone replay execution should succeed");

    // ── Compare receipts ──
    assert_eq!(
        idc_result.receipts.len(),
        phone_result.receipts.len(),
        "Receipt count mismatch"
    );

    for (i, (idc_receipt, phone_receipt)) in idc_result
        .receipts
        .iter()
        .zip(phone_result.receipts.iter())
        .enumerate()
    {
        assert_eq!(
            idc_receipt.success, phone_receipt.success,
            "Receipt {} success mismatch: IDC={}, Phone={}",
            i, idc_receipt.success, phone_receipt.success
        );
        assert_eq!(
            idc_receipt.cumulative_gas_used, phone_receipt.cumulative_gas_used,
            "Receipt {} gas mismatch: IDC={}, Phone={}",
            i, idc_receipt.cumulative_gas_used, phone_receipt.cumulative_gas_used
        );
        assert_eq!(
            idc_receipt.logs, phone_receipt.logs,
            "Receipt {} logs mismatch",
            i
        );
    }

    // Verify receipts root match
    let idc_root = Receipt::calculate_receipt_root_no_memo(&idc_result.receipts);
    let phone_root = Receipt::calculate_receipt_root_no_memo(&phone_result.receipts);
    assert_eq!(
        idc_root, phone_root,
        "Receipts root mismatch: IDC={}, Phone={}",
        idc_root, phone_root
    );

    eprintln!("  Receipts root:    {} ✓", idc_root);
}

// ─── Test: Simple ETH transfer ──────────────────────────────────────────────

#[test]
fn test_v2_pipeline_eth_transfer() {
    eprintln!("\n=== V2 Pipeline: ETH Transfer ===");

    let key_pair = generators::generate_key(&mut generators::rng());
    let sender = public_key_to_address(key_pair.public_key());
    let receiver = Address::with_last_byte(0x42);
    let coinbase = Address::with_last_byte(0xFF);

    // Setup state
    let mut db = CacheDB::new(Default::default());
    db.insert_account_info(
        sender,
        AccountInfo {
            nonce: 0,
            balance: U256::from(10u64) * U256::from(10u64).pow(U256::from(18)), // 10 ETH
            ..Default::default()
        },
    );
    db.insert_account_info(
        receiver,
        AccountInfo {
            nonce: 0,
            balance: U256::from(10u64).pow(U256::from(18)), // 1 ETH
            ..Default::default()
        },
    );

    let chain_spec = test_chain_spec();

    // Create signed transaction: sender → receiver, 1 ETH
    let tx = sign_tx_with_key_pair(
        key_pair,
        Transaction::Legacy(TxLegacy {
            chain_id: Some(chain_spec.chain.id()),
            nonce: 0,
            gas_price: 7,
            gas_limit: 50_000,
            to: TxKind::Call(receiver),
            value: U256::from(10u64).pow(U256::from(18)), // 1 ETH
            input: Bytes::new(),
        }),
    );

    let header = Header {
        number: 1,
        timestamp: 1_700_000_000,
        gas_limit: 30_000_000,
        beneficiary: coinbase,
        base_fee_per_gas: Some(7),
        ..Header::default()
    };

    run_pipeline_test(db, header, vec![tx], vec![sender]);
}

// ─── Test: ERC-20 transfer ──────────────────────────────────────────────────

#[test]
fn test_v2_pipeline_erc20_transfer() {
    eprintln!("\n=== V2 Pipeline: ERC-20 Transfer ===");

    let key_pair = generators::generate_key(&mut generators::rng());
    let sender = public_key_to_address(key_pair.public_key());
    let receiver = Address::with_last_byte(0x42);
    let contract = Address::with_last_byte(0xC0);
    let coinbase = Address::with_last_byte(0xFF);

    // Setup state
    let erc20_code = minimal_erc20_bytecode();
    let code_hash = keccak256(&erc20_code);

    let mut db = CacheDB::new(Default::default());

    // Sender: has ETH for gas
    db.insert_account_info(
        sender,
        AccountInfo {
            nonce: 0,
            balance: U256::from(10u64) * U256::from(10u64).pow(U256::from(18)), // 10 ETH
            ..Default::default()
        },
    );

    // ERC-20 contract: deployed with code
    db.insert_account_info(
        contract,
        AccountInfo {
            nonce: 1,
            balance: U256::ZERO,
            code_hash,
            code: Some(Bytecode::new_raw(erc20_code)),
            account_id: None,
        },
    );

    // Pre-populate storage: balances[sender] = 1,000,000 tokens
    let sender_balance_slot = mapping_slot(sender, 0);
    let token_amount = U256::from(1_000_000u64);
    db.insert_account_storage(contract, sender_balance_slot, token_amount)
        .expect("insert storage");

    let chain_spec = test_chain_spec();

    // Create signed transaction: transfer(receiver, 500_000)
    let transfer_amount = U256::from(500_000u64);
    let calldata = encode_transfer_call(receiver, transfer_amount);

    let tx = sign_tx_with_key_pair(
        key_pair,
        Transaction::Legacy(TxLegacy {
            chain_id: Some(chain_spec.chain.id()),
            nonce: 0,
            gas_price: 7,
            gas_limit: 200_000,
            to: TxKind::Call(contract),
            value: U256::ZERO,
            input: calldata,
        }),
    );

    let header = Header {
        number: 1,
        timestamp: 1_700_000_000,
        gas_limit: 30_000_000,
        beneficiary: coinbase,
        base_fee_per_gas: Some(7),
        ..Header::default()
    };

    run_pipeline_test(db, header, vec![tx], vec![sender]);
}

// ─── Test: Mixed block (ETH transfer + ERC-20 transfer) ─────────────────────

#[test]
fn test_v2_pipeline_mixed_block() {
    eprintln!("\n=== V2 Pipeline: Mixed Block (ETH + ERC-20) ===");

    let key_pair_a = generators::generate_key(&mut generators::rng());
    let sender_a = public_key_to_address(key_pair_a.public_key());
    let key_pair_b = generators::generate_key(&mut generators::rng());
    let sender_b = public_key_to_address(key_pair_b.public_key());

    let receiver = Address::with_last_byte(0x42);
    let contract = Address::with_last_byte(0xC0);
    let coinbase = Address::with_last_byte(0xFF);

    let erc20_code = minimal_erc20_bytecode();
    let code_hash = keccak256(&erc20_code);

    let mut db = CacheDB::new(Default::default());

    // Sender A: has ETH, does ETH transfer
    db.insert_account_info(
        sender_a,
        AccountInfo {
            nonce: 0,
            balance: U256::from(10u64) * U256::from(10u64).pow(U256::from(18)),
            ..Default::default()
        },
    );

    // Sender B: has ETH for gas + ERC-20 tokens
    db.insert_account_info(
        sender_b,
        AccountInfo {
            nonce: 0,
            balance: U256::from(10u64) * U256::from(10u64).pow(U256::from(18)),
            ..Default::default()
        },
    );

    // ERC-20 contract
    db.insert_account_info(
        contract,
        AccountInfo {
            nonce: 1,
            balance: U256::ZERO,
            code_hash,
            code: Some(Bytecode::new_raw(erc20_code)),
            account_id: None,
        },
    );

    // balances[sender_b] = 1,000,000
    let sender_b_slot = mapping_slot(sender_b, 0);
    db.insert_account_storage(contract, sender_b_slot, U256::from(1_000_000u64))
        .expect("insert storage");

    let chain_spec = test_chain_spec();

    // Tx 1: ETH transfer from A to receiver
    let tx1 = sign_tx_with_key_pair(
        key_pair_a,
        Transaction::Legacy(TxLegacy {
            chain_id: Some(chain_spec.chain.id()),
            nonce: 0,
            gas_price: 7,
            gas_limit: 50_000,
            to: TxKind::Call(receiver),
            value: U256::from(10u64).pow(U256::from(18)), // 1 ETH
            input: Bytes::new(),
        }),
    );

    // Tx 2: ERC-20 transfer from B to receiver
    let tx2 = sign_tx_with_key_pair(
        key_pair_b,
        Transaction::Legacy(TxLegacy {
            chain_id: Some(chain_spec.chain.id()),
            nonce: 0,
            gas_price: 7,
            gas_limit: 200_000,
            to: TxKind::Call(contract),
            value: U256::ZERO,
            input: encode_transfer_call(receiver, U256::from(100_000u64)),
        }),
    );

    let header = Header {
        number: 1,
        timestamp: 1_700_000_000,
        gas_limit: 30_000_000,
        beneficiary: coinbase,
        base_fee_per_gas: Some(7),
        ..Header::default()
    };

    run_pipeline_test(
        db,
        header,
        vec![tx1, tx2],
        vec![sender_a, sender_b],
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// Full V2 packet encode/decode/verify end-to-end tests
//
// These tests verify the COMPLETE V2 pipeline including wire format serialization:
//   IDC: ReadLogDatabase → encode_read_log → StreamPacket → encode_stream_packet
//     → zstd compress → [network] → zstd decompress
//     → decode_stream_packet → verify_block_stream → receipts_root match
// ═══════════════════════════════════════════════════════════════════════════════

use alloy_eips::Encodable2718;
use alloy_rlp::Encodable;
use n42_mobile::code_cache::CodeCache;
use n42_mobile::packet::{encode_stream_packet, decode_stream_packet, StreamPacket};
use n42_mobile::verifier::verify_block_stream;
use reth_primitives_traits::SealedHeader;

/// Execute on IDC with ReadLogDatabase, build StreamPacket, encode to wire format,
/// compress with zstd, decompress, decode, verify with verify_block_stream.
///
/// This tests the EXACT same path as production:
///   generate_and_broadcast_v2 → zstd → [network] → decompress → decode → verify
///
/// Two-pass design (mirrors real block building):
///   Pass 1: execute to compute receipts_root (block builder does this)
///   Pass 2: execute with ReadLogDatabase using header with correct receipts_root
fn run_full_v2_packet_test(
    base_db: CacheDB<revm::database::EmptyDB>,
    mut header: Header,
    transactions: Vec<reth_ethereum_primitives::TransactionSigned>,
    senders: Vec<Address>,
) {
    let chain_spec = test_chain_spec();
    let evm_config = EthEvmConfig::new(chain_spec.clone());

    // ── Pass 1: Compute the correct receipts_root ──
    // In production, the block builder computes this before creating the header.
    {
        let block = RecoveredBlock::new_unhashed(
            Block {
                header: header.clone(),
                body: BlockBody {
                    transactions: transactions.clone(),
                    ommers: vec![],
                    withdrawals: None,
                },
            },
            senders.clone(),
        );

        let mut executor =
            BasicBlockExecutor::new(evm_config.clone(), base_db.clone());
        let result = executor
            .execute_one(&block)
            .expect("pre-execution should succeed");
        let receipts_root = Receipt::calculate_receipt_root_no_memo(&result.receipts);
        header.receipts_root = receipts_root;
    }

    // ── Pass 2 (Step 1): IDC side — execute with ReadLogDatabase ──
    let logged_db = ReadLogDatabase::new(base_db);
    let log_handle = logged_db.log_handle();
    let codes_handle = logged_db.codes_handle();

    let block = RecoveredBlock::new_unhashed(
        Block {
            header: header.clone(),
            body: BlockBody {
                transactions: transactions.clone(),
                ommers: vec![],
                withdrawals: None,
            },
        },
        senders,
    );

    let mut idc_executor = BasicBlockExecutor::new(evm_config, logged_db);
    let idc_result = idc_executor.execute_one(&block).expect("IDC execution should succeed");
    let idc_root = Receipt::calculate_receipt_root_no_memo(&idc_result.receipts);

    // Verify our pre-computed root matches
    assert_eq!(idc_root, header.receipts_root, "pre-computed receipts_root must match");

    // Extract read log and captured codes
    let read_log = match Arc::try_unwrap(log_handle) {
        Ok(mutex) => mutex.into_inner().unwrap(),
        Err(arc) => arc.lock().unwrap().clone(),
    };
    let captured_codes = match Arc::try_unwrap(codes_handle) {
        Ok(mutex) => mutex.into_inner().unwrap(),
        Err(arc) => arc.lock().unwrap().clone(),
    };

    // ── Step 2: IDC side — build StreamPacket (same as generate_and_broadcast_v2) ──
    let sealed_header = SealedHeader::seal_slow(header);
    let block_hash = sealed_header.hash();

    let mut header_buf = Vec::new();
    sealed_header.header().encode(&mut header_buf);
    let header_rlp = Bytes::from(header_buf);

    let transactions_encoded: Vec<Bytes> = transactions
        .iter()
        .map(|tx| {
            let mut buf = Vec::new();
            tx.encode_2718(&mut buf);
            Bytes::from(buf)
        })
        .collect();

    let read_log_data = encode_read_log(&read_log);
    let bytecodes: Vec<(B256, Bytes)> = captured_codes.into_iter().collect();

    let packet = StreamPacket {
        block_hash,
        header_rlp,
        transactions: transactions_encoded,
        read_log_data,
        bytecodes,
    };

    eprintln!("  Block hash:       {:#x}", block_hash);
    eprintln!("  Read log entries: {}", read_log.len());
    eprintln!("  Read log bytes:   {}", packet.read_log_data.len());
    eprintln!("  Bytecodes:        {}", packet.bytecodes.len());
    eprintln!("  Transactions:     {}", packet.transactions.len());
    eprintln!("  Estimated size:   {} bytes", packet.estimated_size());

    // ── Step 3: encode_stream_packet → wire format bytes ──
    let encoded = encode_stream_packet(&packet);
    eprintln!("  Encoded size:     {} bytes", encoded.len());

    // Verify wire header magic
    assert_eq!(&encoded[0..2], &[0x4E, 0x32], "wire header magic must be 'N2'");
    assert_eq!(encoded[2], 0x01, "wire header version must be 1");

    // ── Step 4: zstd compress (simulating network transit) ──
    let compressed = zstd::bulk::compress(&encoded, 3).expect("zstd compress");
    let compression_ratio = compressed.len() as f64 / encoded.len() as f64;
    eprintln!(
        "  Compressed size:  {} bytes ({:.1}% ratio)",
        compressed.len(),
        compression_ratio * 100.0
    );

    // ── Step 5: zstd decompress (phone receives) ──
    let decompressed =
        zstd::bulk::decompress(&compressed, 16 * 1024 * 1024).expect("zstd decompress");
    assert_eq!(decompressed, encoded, "decompress must match original");

    // ── Step 6: decode_stream_packet ──
    let decoded = decode_stream_packet(&decompressed).expect("decode_stream_packet should succeed");

    // Verify decoded packet fields match original
    assert_eq!(decoded.block_hash, packet.block_hash, "block_hash mismatch");
    assert_eq!(
        decoded.transactions.len(),
        packet.transactions.len(),
        "tx count mismatch"
    );
    assert_eq!(
        decoded.read_log_data.len(),
        packet.read_log_data.len(),
        "read_log_data length mismatch"
    );
    assert_eq!(
        decoded.bytecodes.len(),
        packet.bytecodes.len(),
        "bytecodes count mismatch"
    );

    // Verify header_info convenience method
    let (block_number, receipts_root) = decoded
        .header_info()
        .expect("header_info should succeed");
    eprintln!("  Block number:     {}", block_number);
    eprintln!("  Receipts root:    {:#x}", receipts_root);

    // ── Step 7: verify_block_stream (phone verifies) ──
    let mut code_cache = CodeCache::new(100);
    let result = verify_block_stream(&decoded, &mut code_cache, chain_spec)
        .expect("verify_block_stream should succeed");

    assert!(
        result.receipts_root_match,
        "receipts root must match! computed={:#x}, expected={:#x}",
        result.computed_receipts_root, receipts_root
    );
    assert_eq!(
        result.computed_receipts_root, idc_root,
        "phone computed root must equal IDC computed root"
    );

    eprintln!("  Verification:     PASS ✓");
    eprintln!(
        "  Receipts root:    {:#x} (IDC == Phone)",
        result.computed_receipts_root
    );
}

// ─── Test: Full V2 packet path — ETH transfer ──────────────────────────────

#[test]
fn test_v2_full_packet_eth_transfer() {
    eprintln!("\n=== Full V2 Packet: ETH Transfer ===");

    let key_pair = generators::generate_key(&mut generators::rng());
    let sender = public_key_to_address(key_pair.public_key());
    let receiver = Address::with_last_byte(0x42);
    let coinbase = Address::with_last_byte(0xFF);

    let mut db = CacheDB::new(Default::default());
    db.insert_account_info(
        sender,
        AccountInfo {
            nonce: 0,
            balance: U256::from(10u64) * U256::from(10u64).pow(U256::from(18)),
            ..Default::default()
        },
    );
    db.insert_account_info(
        receiver,
        AccountInfo {
            nonce: 0,
            balance: U256::from(10u64).pow(U256::from(18)),
            ..Default::default()
        },
    );

    let chain_spec = test_chain_spec();

    let tx = sign_tx_with_key_pair(
        key_pair,
        Transaction::Legacy(TxLegacy {
            chain_id: Some(chain_spec.chain.id()),
            nonce: 0,
            gas_price: 7,
            gas_limit: 50_000,
            to: TxKind::Call(receiver),
            value: U256::from(10u64).pow(U256::from(18)),
            input: Bytes::new(),
        }),
    );

    let header = Header {
        number: 1,
        timestamp: 1_700_000_000,
        gas_limit: 30_000_000,
        beneficiary: coinbase,
        base_fee_per_gas: Some(7),
        ..Header::default()
    };

    run_full_v2_packet_test(db, header, vec![tx], vec![sender]);
}

// ─── Test: Full V2 packet path — ERC-20 transfer ────────────────────────────

#[test]
fn test_v2_full_packet_erc20_transfer() {
    eprintln!("\n=== Full V2 Packet: ERC-20 Transfer ===");

    let key_pair = generators::generate_key(&mut generators::rng());
    let sender = public_key_to_address(key_pair.public_key());
    let receiver = Address::with_last_byte(0x42);
    let contract = Address::with_last_byte(0xC0);
    let coinbase = Address::with_last_byte(0xFF);

    let erc20_code = minimal_erc20_bytecode();
    let code_hash = keccak256(&erc20_code);

    let mut db = CacheDB::new(Default::default());
    db.insert_account_info(
        sender,
        AccountInfo {
            nonce: 0,
            balance: U256::from(10u64) * U256::from(10u64).pow(U256::from(18)),
            ..Default::default()
        },
    );
    db.insert_account_info(
        contract,
        AccountInfo {
            nonce: 1,
            balance: U256::ZERO,
            code_hash,
            code: Some(Bytecode::new_raw(erc20_code)),
            account_id: None,
        },
    );
    let sender_balance_slot = mapping_slot(sender, 0);
    db.insert_account_storage(contract, sender_balance_slot, U256::from(1_000_000u64))
        .expect("insert storage");

    let chain_spec = test_chain_spec();

    let tx = sign_tx_with_key_pair(
        key_pair,
        Transaction::Legacy(TxLegacy {
            chain_id: Some(chain_spec.chain.id()),
            nonce: 0,
            gas_price: 7,
            gas_limit: 200_000,
            to: TxKind::Call(contract),
            value: U256::ZERO,
            input: encode_transfer_call(receiver, U256::from(500_000u64)),
        }),
    );

    let header = Header {
        number: 1,
        timestamp: 1_700_000_000,
        gas_limit: 30_000_000,
        beneficiary: coinbase,
        base_fee_per_gas: Some(7),
        ..Header::default()
    };

    run_full_v2_packet_test(db, header, vec![tx], vec![sender]);
}

// ─── Test: Full V2 packet path — Mixed block ────────────────────────────────

#[test]
fn test_v2_full_packet_mixed_block() {
    eprintln!("\n=== Full V2 Packet: Mixed Block (ETH + ERC-20) ===");

    let key_pair_a = generators::generate_key(&mut generators::rng());
    let sender_a = public_key_to_address(key_pair_a.public_key());
    let key_pair_b = generators::generate_key(&mut generators::rng());
    let sender_b = public_key_to_address(key_pair_b.public_key());

    let receiver = Address::with_last_byte(0x42);
    let contract = Address::with_last_byte(0xC0);
    let coinbase = Address::with_last_byte(0xFF);

    let erc20_code = minimal_erc20_bytecode();
    let code_hash = keccak256(&erc20_code);

    let mut db = CacheDB::new(Default::default());
    db.insert_account_info(
        sender_a,
        AccountInfo {
            nonce: 0,
            balance: U256::from(10u64) * U256::from(10u64).pow(U256::from(18)),
            ..Default::default()
        },
    );
    db.insert_account_info(
        sender_b,
        AccountInfo {
            nonce: 0,
            balance: U256::from(10u64) * U256::from(10u64).pow(U256::from(18)),
            ..Default::default()
        },
    );
    db.insert_account_info(
        contract,
        AccountInfo {
            nonce: 1,
            balance: U256::ZERO,
            code_hash,
            code: Some(Bytecode::new_raw(erc20_code)),
            account_id: None,
        },
    );
    let sender_b_slot = mapping_slot(sender_b, 0);
    db.insert_account_storage(contract, sender_b_slot, U256::from(1_000_000u64))
        .expect("insert storage");

    let chain_spec = test_chain_spec();

    let tx1 = sign_tx_with_key_pair(
        key_pair_a,
        Transaction::Legacy(TxLegacy {
            chain_id: Some(chain_spec.chain.id()),
            nonce: 0,
            gas_price: 7,
            gas_limit: 50_000,
            to: TxKind::Call(receiver),
            value: U256::from(10u64).pow(U256::from(18)),
            input: Bytes::new(),
        }),
    );

    let tx2 = sign_tx_with_key_pair(
        key_pair_b,
        Transaction::Legacy(TxLegacy {
            chain_id: Some(chain_spec.chain.id()),
            nonce: 0,
            gas_price: 7,
            gas_limit: 200_000,
            to: TxKind::Call(contract),
            value: U256::ZERO,
            input: encode_transfer_call(receiver, U256::from(100_000u64)),
        }),
    );

    let header = Header {
        number: 1,
        timestamp: 1_700_000_000,
        gas_limit: 30_000_000,
        beneficiary: coinbase,
        base_fee_per_gas: Some(7),
        ..Header::default()
    };

    run_full_v2_packet_test(db, header, vec![tx1, tx2], vec![sender_a, sender_b]);
}
