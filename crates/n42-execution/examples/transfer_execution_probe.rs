//! The transfer fast path versus the stock handlers. No signatures, QMDB or chain TPS.
use alloy_consensus::{Header, TxLegacy};
use alloy_primitives::{Address, B256, TxKind, U256};
use evm2::{
    ethereum::TxEnvelope,
    evm::{AccountInfo, CacheDB, EmptyDB},
};
use n42_execution::{N42EvmConfig, N42EvmFactory, fast_transfer};
use reth_chainspec::ChainSpecBuilder;
use reth_evm::{ConfigureEvm, Evm};
use reth_primitives_traits::Recovered;
use std::{hint::black_box, sync::Arc, time::Instant};

type Accounts = Vec<Option<(U256, u64)>>;

fn run(fast: bool, count: u64) -> (u128, Accounts, u64) {
    let sender = Address::repeat_byte(0x11);
    let recipient = Address::repeat_byte(0x22);
    let beneficiary = Address::repeat_byte(0x33);
    let mut db = CacheDB::new(EmptyDB::default());
    for (address, balance) in [
        (sender, U256::from(10u128.pow(24))),
        (recipient, U256::ZERO),
        (beneficiary, U256::from(1)),
    ] {
        db.insert_account_info(&address, AccountInfo::empty().with_balance(balance));
    }
    let chain = Arc::new(ChainSpecBuilder::mainnet().osaka_activated().build());
    let config = N42EvmConfig::with_evm_factory(chain, N42EvmFactory::with_fast_transfers(fast));
    let env = config
        .evm_env(&Header {
            number: 1,
            timestamp: 1_000,
            beneficiary,
            base_fee_per_gas: Some(1000),
            gas_limit: 5_000_000_000,
            excess_blob_gas: Some(0),
            blob_gas_used: Some(0),
            parent_beacon_block_root: Some(B256::ZERO),
            ..Default::default()
        })
        .expect("environment");
    let mut evm = config.evm_with_env(db, env);
    let mut used = 0;
    let hits = fast_transfer::hits();
    let start = Instant::now();
    for nonce in 0..count {
        let tx = Recovered::new_unchecked(
            TxEnvelope::Legacy(TxLegacy {
                chain_id: Some(1),
                nonce,
                gas_price: 2000,
                gas_limit: 21_000,
                to: TxKind::Call(recipient),
                value: U256::from(1),
                input: Default::default(),
            }),
            sender,
        );
        let out = Evm::transact_commit(&mut evm, &tx).expect("transfer execution");
        assert!(out.status);
        used += out.total_gas_spent;
    }
    let elapsed = start.elapsed().as_nanos();
    assert_eq!(fast_transfer::hits() - hits, if fast { count } else { 0 });
    let accounts: Accounts = [sender, recipient, beneficiary]
        .map(|address| {
            evm.state_mut()
                .account_info_untracked(&address)
                .expect("account read")
                .map(|info| (info.balance, info.nonce))
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
