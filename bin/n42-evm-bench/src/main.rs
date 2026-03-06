//! N42 EVM single-core benchmark
//!
//! Measures pure EVM execution time for:
//! 1. Simple ETH transfer (21000 gas)
//! 2. ERC-20 style transfer (SLOAD/SSTORE contract call)
//!
//! All initialization is outside the timing loop.
//! Results show theoretical single-core TPS ceiling.

use alloy_primitives::{address, Address, TxKind, U256};
use revm::{
    context::TxEnv,
    database::{CacheDB, EmptyDB},
    state::AccountInfo,
    Context, ExecuteCommitEvm, MainBuilder, MainContext,
};
use std::time::Instant;

const SENDER: Address = address!("1000000000000000000000000000000000000001");
const RECEIVER: Address = address!("2000000000000000000000000000000000000002");
const ERC20_ADDR: Address = address!("3000000000000000000000000000000000000003");

fn main() {
    println!("=== N42 EVM Single-Core Benchmark ===\n");

    bench_eth_transfer();
    println!();
    bench_erc20_transfer();
}

fn bench_eth_transfer() {
    println!("--- ETH Transfer (21000 gas) ---");

    let mut db = CacheDB::<EmptyDB>::default();
    db.insert_account_info(SENDER, AccountInfo {
        balance: U256::from(u128::MAX),
        ..Default::default()
    });

    let warmup = 1_000;
    let iterations = 100_000;

    // Warmup
    {
        let ctx = Context::mainnet().with_db(&mut db);
        let mut evm = ctx.build_mainnet();
        for i in 0..warmup {
            let tx = TxEnv::builder()
                .caller(SENDER)
                .kind(TxKind::Call(RECEIVER))
                .value(U256::from(1))
                .gas_limit(21_000)
                .gas_price(0)
                .nonce(i)
                .build()
                .unwrap();
            let _ = evm.transact_commit(tx);
        }
    }

    // Reset state for clean measurement
    let mut db = CacheDB::<EmptyDB>::default();
    db.insert_account_info(SENDER, AccountInfo {
        balance: U256::from(u128::MAX),
        nonce: 0,
        ..Default::default()
    });

    let ctx = Context::mainnet().with_db(&mut db);
    let mut evm = ctx.build_mainnet();

    let start = Instant::now();
    for i in 0..iterations {
        let tx = TxEnv::builder()
            .caller(SENDER)
            .kind(TxKind::Call(RECEIVER))
            .value(U256::from(1))
            .gas_limit(21_000)
            .gas_price(0)
            .nonce(i)
            .build()
            .unwrap();
        let _ = evm.transact_commit(tx);
    }
    let elapsed = start.elapsed();

    let ns_per_tx = elapsed.as_nanos() as f64 / iterations as f64;
    let tps = 1_000_000_000.0 / ns_per_tx;

    println!("  Iterations: {iterations}");
    println!("  Total time: {elapsed:.2?}");
    println!("  Per tx:     {ns_per_tx:.0} ns");
    println!("  Theoretical single-core TPS: {tps:.0}");
}

fn bench_erc20_transfer() {
    println!("--- ERC-20 Transfer (2x SLOAD + 2x SSTORE contract call) ---");

    let mut db = CacheDB::<EmptyDB>::default();
    db.insert_account_info(SENDER, AccountInfo {
        balance: U256::from(u128::MAX),
        ..Default::default()
    });

    // Hand-crafted bytecode: SLOAD slot 0 (sender), SUB 1, SSTORE;
    // SLOAD slot 1 (receiver), ADD 1, SSTORE; RETURN(32, 0)
    let bytecode: Vec<u8> = vec![
        0x60, 0x00, 0x54,       // PUSH1 0, SLOAD (sender bal)
        0x60, 0x01, 0x90, 0x03, // PUSH1 1, SWAP1, SUB
        0x60, 0x00, 0x55,       // PUSH1 0, SSTORE
        0x60, 0x01, 0x54,       // PUSH1 1, SLOAD (receiver bal)
        0x60, 0x01, 0x01,       // PUSH1 1, ADD
        0x60, 0x01, 0x55,       // PUSH1 1, SSTORE
        0x60, 0x01,             // PUSH1 1 (true)
        0x60, 0x00,             // PUSH1 0 (memory offset)
        0x52,                   // MSTORE
        0x60, 0x20,             // PUSH1 32
        0x60, 0x00,             // PUSH1 0
        0xf3,                   // RETURN
    ];

    let mut contract_info = AccountInfo::default();
    contract_info.code = Some(revm::bytecode::Bytecode::new_legacy(bytecode.clone().into()));
    contract_info.code_hash = contract_info.code.as_ref().unwrap().hash_slow();
    db.insert_account_info(ERC20_ADDR, contract_info);

    // Initial sender balance in contract storage slot 0
    db.insert_account_storage(ERC20_ADDR, U256::ZERO, U256::from(u128::MAX)).unwrap();

    let iterations: u64 = 100_000;

    // Warmup
    {
        let ctx = Context::mainnet().with_db(&mut db);
        let mut evm = ctx.build_mainnet();
        for i in 0..1000u64 {
            let tx = TxEnv::builder()
                .caller(SENDER)
                .kind(TxKind::Call(ERC20_ADDR))
                .gas_limit(100_000)
                .gas_price(0)
                .nonce(i)
                .build()
                .unwrap();
            let _ = evm.transact_commit(tx);
        }
    }

    // Reset
    let mut db = CacheDB::<EmptyDB>::default();
    db.insert_account_info(SENDER, AccountInfo {
        balance: U256::from(u128::MAX),
        nonce: 0,
        ..Default::default()
    });

    let mut contract_info = AccountInfo::default();
    contract_info.code = Some(revm::bytecode::Bytecode::new_legacy(bytecode.into()));
    contract_info.code_hash = contract_info.code.as_ref().unwrap().hash_slow();
    db.insert_account_info(ERC20_ADDR, contract_info);
    db.insert_account_storage(ERC20_ADDR, U256::ZERO, U256::from(u128::MAX)).unwrap();

    let ctx = Context::mainnet().with_db(&mut db);
    let mut evm = ctx.build_mainnet();

    let start = Instant::now();
    for i in 0..iterations {
        let tx = TxEnv::builder()
            .caller(SENDER)
            .kind(TxKind::Call(ERC20_ADDR))
            .gas_limit(100_000)
            .gas_price(0)
            .nonce(i)
            .build()
            .unwrap();
        let _ = evm.transact_commit(tx);
    }
    let elapsed = start.elapsed();

    let ns_per_tx = elapsed.as_nanos() as f64 / iterations as f64;
    let tps = 1_000_000_000.0 / ns_per_tx;

    println!("  Iterations: {iterations}");
    println!("  Total time: {elapsed:.2?}");
    println!("  Per tx:     {ns_per_tx:.0} ns");
    println!("  Theoretical single-core TPS: {tps:.0}");
}
