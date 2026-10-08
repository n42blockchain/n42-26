//! Direct transfer versus interpreter execution. No signatures, QMDB or chain TPS.
use alloy_evm::{Evm, EvmEnv, EvmFactory};
use alloy_primitives::{Address, TxKind, U256};
use n42_execution::N42EvmFactory;
use revm::{
    Database, DatabaseCommit,
    context::{BlockEnv, CfgEnv, TxEnv},
    database::{CacheDB, EmptyDB},
    primitives::hardfork::SpecId,
    state::AccountInfo,
};
use std::{hint::black_box, time::Instant};

fn run(fast: bool, count: u64) -> (u128, Vec<Option<AccountInfo>>, u64) {
    let sender = Address::repeat_byte(0x11);
    let recipient = Address::repeat_byte(0x22);
    let beneficiary = Address::repeat_byte(0x33);
    let mut db = CacheDB::new(EmptyDB::default());
    for (address, balance) in [
        (sender, U256::from(10u128.pow(24))),
        (recipient, U256::ZERO),
        (beneficiary, U256::from(1)),
    ] {
        db.insert_account_info(
            address,
            AccountInfo {
                balance,
                ..Default::default()
            },
        );
    }
    let mut cfg = CfgEnv::new_with_spec(SpecId::OSAKA);
    cfg.chain_id = 1;
    let env = EvmEnv::new(
        cfg,
        BlockEnv {
            beneficiary,
            basefee: 1000,
            gas_limit: 5_000_000_000,
            ..Default::default()
        },
    );
    let mut evm = N42EvmFactory::with_fast_transfers(fast).create_evm(db, env);
    let mut used = 0;
    let start = Instant::now();
    for nonce in 0..count {
        let tx = TxEnv {
            caller: sender,
            kind: TxKind::Call(recipient),
            nonce,
            value: U256::from(1),
            gas_limit: 21_000,
            gas_price: 2000,
            chain_id: Some(1),
            ..Default::default()
        };
        let out = evm.transact_raw(tx).expect("transfer execution");
        assert!(out.result.is_success());
        used += out.result.gas().tx_gas_used();
        evm.db_mut().commit(out.state);
    }
    let elapsed = start.elapsed().as_nanos();
    assert_eq!(evm.fast_transfer_hits(), if fast { count } else { 0 });
    let (mut db, _) = evm.finish();
    let accounts = [sender, recipient, beneficiary]
        .map(|address| {
            db.basic(address)
                .expect("account read")
                .map(|info| AccountInfo { code: None, ..info })
        })
        .to_vec();
    black_box(&accounts);
    (elapsed, accounts, used)
}

fn main() {
    let count = std::env::args()
        .nth(1)
        .map(|v| v.parse::<u64>().expect("positive transaction count"))
        .unwrap_or(100_000);
    assert!((1..=1_000_000).contains(&count));
    let (_, baseline, gas) = run(false, count);
    let (_, candidate, candidate_gas) = run(true, count);
    assert_eq!(baseline, candidate, "complete final account equality");
    assert_eq!(gas, candidate_gas);
    assert_eq!(gas, count * 21_000);
    for (tag, fast) in [("warmup", false), ("a1", false), ("b", true), ("a2", false)] {
        for iteration in 0..10 {
            let (duration, accounts, used) = run(fast, count);
            assert_eq!(accounts, baseline);
            assert_eq!(used, gas);
            println!(
                "TRANSFER_PROBE {{\"tag\":\"{tag}\",\"iteration\":{iteration},\"transactions\":{count},\"duration_ns\":{duration},\"gas_used\":{used}}}"
            );
        }
    }
}
