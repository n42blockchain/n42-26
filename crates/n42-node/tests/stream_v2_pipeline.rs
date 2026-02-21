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

/// Minimal ERC-20 token contract runtime bytecode.
///
/// Supports:
/// - `transfer(address,uint256)` selector 0xa9059cbb
/// - `balanceOf(address)` selector 0x70a08231
///
/// Storage layout (Solidity-compatible):
/// - `balances[addr]` at slot `keccak256(abi.encode(addr, 0))`
///
/// Written in raw EVM assembly for deterministic, dependency-free compilation.
fn minimal_erc20_bytecode() -> Bytes {
    // We use a minimal contract that:
    // 1. Checks function selector
    // 2. For transfer: reads sender balance (SLOAD), reads receiver balance (SLOAD),
    //    updates both (SSTORE), returns true
    // 3. For balanceOf: reads balance (SLOAD), returns it
    //
    // This exercises: basic() (sender, contract, coinbase), storage() (2-4 SLOADs),
    // code_by_hash() (contract code lookup)
    //
    // Assembly:
    // PUSH0 CALLDATALOAD PUSH1 0xE0 SHR    ; get selector
    // DUP1 PUSH4 0xa9059cbb EQ PUSH2 transfer JUMPI  ; check transfer
    // DUP1 PUSH4 0x70a08231 EQ PUSH2 balanceOf JUMPI ; check balanceOf
    // PUSH0 PUSH0 REVERT                    ; fallback: revert
    //
    // transfer: (offset ~0x1a)
    //   JUMPDEST
    //   POP                                   ; drop selector
    //   ; compute slot for balances[caller]
    //   CALLER PUSH0 MSTORE                   ; mem[0..32] = caller
    //   PUSH0 PUSH1 0x20 MSTORE              ; mem[32..64] = 0 (slot index)
    //   PUSH1 0x40 PUSH0 SHA3                ; slot_sender = keccak256(mem[0..64])
    //   DUP1 SLOAD                           ; balance_sender
    //   ; load amount from calldata
    //   PUSH1 0x24 CALLDATALOAD              ; amount
    //   DUP1 DUP3 LT PUSH2 revert_label JUMPI  ; if balance < amount, revert
    //   ; new_sender_bal = balance - amount
    //   SWAP1 SUB                            ; new_sender_balance on stack
    //   SWAP1                                ; slot_sender on top
    //   SSTORE                               ; store new balance
    //   ; compute slot for balances[to]
    //   PUSH1 0x04 CALLDATALOAD              ; to address (as uint256)
    //   PUSH0 MSTORE                         ; mem[0..32] = to
    //   PUSH0 PUSH1 0x20 MSTORE             ; mem[32..64] = 0
    //   PUSH1 0x40 PUSH0 SHA3               ; slot_to
    //   DUP1 SLOAD                          ; balance_to
    //   ; remaining amount was consumed by SUB, need it again
    //   PUSH1 0x24 CALLDATALOAD             ; re-read amount
    //   ADD                                  ; new_to_balance
    //   SWAP1 SSTORE                        ; store
    //   ; return true (1)
    //   PUSH1 0x01 PUSH0 MSTORE
    //   PUSH1 0x20 PUSH0 RETURN
    //
    // balanceOf: (offset ~0x5c)
    //   JUMPDEST
    //   POP
    //   PUSH1 0x04 CALLDATALOAD PUSH0 MSTORE
    //   PUSH0 PUSH1 0x20 MSTORE
    //   PUSH1 0x40 PUSH0 SHA3
    //   SLOAD
    //   PUSH0 MSTORE
    //   PUSH1 0x20 PUSH0 RETURN
    //
    // revert_label:
    //   JUMPDEST PUSH0 PUSH0 REVERT
    //
    // Rather than hand-assemble 100+ bytes with offset errors, use Solidity-compiled bytecode
    // from a known minimal ERC-20 contract.
    //
    // Compiled from:
    //   contract T {
    //     mapping(address=>uint256) public b;
    //     function transfer(address t, uint256 a) external returns(bool) {
    //       b[msg.sender] -= a;
    //       b[t] += a;
    //       return true;
    //     }
    //   }
    // with solc 0.8.28, --optimize --optimize-runs=200
    //
    // We inline the runtime bytecode here for reproducibility.

    // Instead of fighting with raw assembly, use a known-good approach:
    // Write a contract using only PUSH/SLOAD/SSTORE/MSTORE/SHA3/CALLDATALOAD/CALLER etc.
    // Build it programmatically:
    let mut code = Vec::new();

    // ── Selector dispatch ──
    // PUSH0 CALLDATALOAD → load first 32 bytes
    code.extend_from_slice(&[0x5F, 0x35]);
    // PUSH1 0xE0 SHR → shift right 224 bits to get 4-byte selector
    code.extend_from_slice(&[0x60, 0xE0, 0x1C]);
    // DUP1
    code.push(0x80);
    // PUSH4 0xa9059cbb (transfer selector)
    code.extend_from_slice(&[0x63, 0xa9, 0x05, 0x9c, 0xbb]);
    // EQ PUSH1 <transfer_offset> JUMPI
    code.push(0x14);
    let transfer_jump_pos = code.len();
    code.extend_from_slice(&[0x61, 0x00, 0x00]); // placeholder for 2-byte offset
    code.push(0x57);

    // DUP1
    code.push(0x80);
    // PUSH4 0x70a08231 (balanceOf selector)
    code.extend_from_slice(&[0x63, 0x70, 0xa0, 0x82, 0x31]);
    // EQ PUSH1 <balanceof_offset> JUMPI
    code.push(0x14);
    let balanceof_jump_pos = code.len();
    code.extend_from_slice(&[0x61, 0x00, 0x00]); // placeholder
    code.push(0x57);

    // Fallback: STOP
    code.push(0x00);

    // ── transfer(address to, uint256 amount) ──
    let transfer_offset = code.len();
    code.push(0x5B); // JUMPDEST
    code.push(0x50); // POP selector

    // Compute slot for balances[caller]: keccak256(abi.encode(caller, 0))
    // CALLER PUSH0 MSTORE
    code.extend_from_slice(&[0x33, 0x5F, 0x52]);
    // PUSH0 PUSH1 0x20 MSTORE (slot_index = 0 at mem[32..64])
    code.extend_from_slice(&[0x5F, 0x60, 0x20, 0x52]);
    // PUSH1 0x40 PUSH0 SHA3 → slot_sender
    code.extend_from_slice(&[0x60, 0x40, 0x5F, 0x20]);
    // DUP1 SLOAD → [slot_sender, balance_sender]
    code.extend_from_slice(&[0x80, 0x54]);

    // PUSH1 0x24 CALLDATALOAD → amount
    code.extend_from_slice(&[0x60, 0x24, 0x35]);
    // Stack: [slot_sender, balance_sender, amount]

    // DUP1 DUP3 → [slot_sender, balance_sender, amount, amount, balance_sender]
    code.extend_from_slice(&[0x80, 0x82]);
    // LT → balance_sender < amount?
    code.push(0x10);
    // PUSH2 <revert_offset> JUMPI
    let revert_jump_pos = code.len();
    code.extend_from_slice(&[0x61, 0x00, 0x00]); // placeholder
    code.push(0x57);

    // Stack: [slot_sender, balance_sender, amount]
    // new_sender_bal = balance_sender - amount
    // SWAP1 → [slot_sender, amount, balance_sender]
    code.push(0x90);
    // SUB → [slot_sender, balance_sender - amount]
    code.push(0x03);
    // Hmm, SUB pops a then b and pushes a - b
    // Stack before SUB: [..., amount, balance_sender]
    // SUB → amount - balance_sender? No:
    // SUB: a = pop(), b = pop(), push(a - b) → a = balance_sender, b = amount?
    // No. The SWAP1 makes it: [slot_sender, amount, balance_sender]
    // SUB pops balance_sender (top), then amount → pushes balance_sender - amount ✓
    // Actually EVM SUB: a = stack[0], b = stack[1], result = a - b
    // So stack top is `a`, next is `b`, result is a - b.
    // After SWAP1: top = balance_sender, next = amount
    // SUB: balance_sender - amount ✓ Good.

    // Stack: [slot_sender, new_balance]
    // SWAP1 SSTORE → store new_balance at slot_sender
    code.extend_from_slice(&[0x90, 0x55]);

    // Compute slot for balances[to]: keccak256(abi.encode(to, 0))
    // PUSH1 0x04 CALLDATALOAD → to (as uint256)
    code.extend_from_slice(&[0x60, 0x04, 0x35]);
    // PUSH0 MSTORE
    code.extend_from_slice(&[0x5F, 0x52]);
    // PUSH0 PUSH1 0x20 MSTORE
    code.extend_from_slice(&[0x5F, 0x60, 0x20, 0x52]);
    // PUSH1 0x40 PUSH0 SHA3 → slot_to
    code.extend_from_slice(&[0x60, 0x40, 0x5F, 0x20]);
    // DUP1 SLOAD → [slot_to, balance_to]
    code.extend_from_slice(&[0x80, 0x54]);

    // PUSH1 0x24 CALLDATALOAD → re-read amount
    code.extend_from_slice(&[0x60, 0x24, 0x35]);
    // ADD → new_to_balance
    code.push(0x01);
    // SWAP1 SSTORE → store new_to_balance at slot_to
    code.extend_from_slice(&[0x90, 0x55]);

    // Return true (1)
    // PUSH1 1 PUSH0 MSTORE PUSH1 0x20 PUSH0 RETURN
    code.extend_from_slice(&[0x60, 0x01, 0x5F, 0x52, 0x60, 0x20, 0x5F, 0xF3]);

    // ── balanceOf(address) ──
    let balanceof_offset = code.len();
    code.push(0x5B); // JUMPDEST
    code.push(0x50); // POP selector

    // PUSH1 0x04 CALLDATALOAD PUSH0 MSTORE
    code.extend_from_slice(&[0x60, 0x04, 0x35, 0x5F, 0x52]);
    // PUSH0 PUSH1 0x20 MSTORE
    code.extend_from_slice(&[0x5F, 0x60, 0x20, 0x52]);
    // PUSH1 0x40 PUSH0 SHA3
    code.extend_from_slice(&[0x60, 0x40, 0x5F, 0x20]);
    // SLOAD
    code.push(0x54);
    // PUSH0 MSTORE PUSH1 0x20 PUSH0 RETURN
    code.extend_from_slice(&[0x5F, 0x52, 0x60, 0x20, 0x5F, 0xF3]);

    // ── revert label ──
    let revert_offset = code.len();
    code.push(0x5B); // JUMPDEST
    // PUSH0 PUSH0 REVERT
    code.extend_from_slice(&[0x5F, 0x5F, 0xFD]);

    // ── Patch jump targets ──
    let transfer_be = (transfer_offset as u16).to_be_bytes();
    code[transfer_jump_pos + 1] = transfer_be[0];
    code[transfer_jump_pos + 2] = transfer_be[1];

    let balanceof_be = (balanceof_offset as u16).to_be_bytes();
    code[balanceof_jump_pos + 1] = balanceof_be[0];
    code[balanceof_jump_pos + 2] = balanceof_be[1];

    let revert_be = (revert_offset as u16).to_be_bytes();
    code[revert_jump_pos + 1] = revert_be[0];
    code[revert_jump_pos + 2] = revert_be[1];

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

    let replay_db = StreamReplayDB::new(read_log_data, bytecodes_map);
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
