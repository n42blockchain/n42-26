//! The Profile-1 deferred proposal check on a full block: its sender cache
//! path gives the same verdicts as recovering every sender, and (ignored
//! bench) where its time goes on a 1200-transaction block.
//!
//! Bench: `cargo test --release -p n42-node --test deferred_check_speed -- --ignored --nocapture`

use alloy_consensus::{EMPTY_ROOT_HASH, Header, TxEip1559, transaction::TxHashRef as _};
use alloy_eips::eip2718::{Decodable2718 as _, Encodable2718};
use alloy_primitives::{Address, B256, Bytes, TxKind, U256};
use alloy_rpc_types_engine::{
    ExecutionData, ExecutionPayload, ExecutionPayloadSidecar, ExecutionPayloadV1,
};
use n42_chainspec::profile1::Profile1Header;
use n42_consensus::ExecutedFields;
use n42_consensus_service::deferred_check::DeferredCheck;
use n42_node::el::DeferredChecker;
use n42_tx_types::N42TxEnvelope;
use rayon::prelude::*;
use reth_ethereum_primitives::{Transaction, TransactionSigned};
use reth_evm::SenderRecoveryCache;
use reth_primitives_traits::SignerRecoverable as _;
use reth_provider::{
    StateProviderFactory as _, providers::BlockchainProvider,
    test_utils::create_test_provider_factory_with_node_types,
};
use reth_testing_utils::generators::sign_tx_with_key_pair;
use secp256k1::{Keypair, SecretKey};
use serde_json::Value;
use std::{path::PathBuf, sync::Arc, time::Instant};

const BASE_FEE: u64 = 1_000_000_000;

fn keypair(n: u64) -> (Keypair, Address) {
    let mut secret = [0u8; 32];
    secret[0] = 0x5d;
    secret[24..].copy_from_slice(&n.to_be_bytes());
    let key = Keypair::from_secret_key(&SecretKey::from_secret_bytes(secret).unwrap());
    let sender = Address::from_raw_public_key(&key.public_key().serialize_uncompressed()[1..]);
    (key, sender)
}

fn transfer(key: Keypair, chain_id: u64, nonce: u64, n: u64) -> N42TxEnvelope {
    let mut to = [0u8; 20];
    to[0] = 0xa0;
    to[12..].copy_from_slice(&n.to_be_bytes());
    let signed: TransactionSigned = sign_tx_with_key_pair(
        key,
        Transaction::Eip1559(TxEip1559 {
            chain_id,
            nonce,
            gas_limit: 100_000,
            max_fee_per_gas: 2 * u128::from(BASE_FEE),
            max_priority_fee_per_gas: 1,
            to: TxKind::Call(Address::from(to)),
            value: U256::from(1),
            ..Default::default()
        }),
    );
    signed.into()
}

/// A node whose genesis funds `senders` funded accounts, its checker over
/// reth's state of genesis, and the child header's frame.
struct Node {
    chain_id: u64,
    genesis_hash: B256,
    carried: ExecutedFields,
    keys: Vec<(Keypair, Address)>,
    checker: DeferredChecker,
}

fn node(senders: u64) -> Node {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut genesis: Value = serde_json::from_slice(
        &std::fs::read(manifest.join("../n42-chainspec/testdata/profile1/V1/genesis_8s.json"))
            .unwrap(),
    )
    .unwrap();
    let keys: Vec<_> = (1..=senders).map(keypair).collect();
    for (_, sender) in &keys {
        genesis["alloc"][sender.to_string()] =
            serde_json::json!({ "balance": "0xde0b6b3a7640000" });
    }
    let (spec, _config, genesis_header) =
        n42_chainspec::profile1::profile1_chain_spec(&genesis).expect("Profile-1 genesis");
    let spec = Arc::new(spec);
    let genesis_hash = genesis_header.hash();
    n42_consensus::remember_gov5_native_header(&genesis_header.encode());
    let factory = create_test_provider_factory_with_node_types::<n42_node::N42Node>(spec.clone());
    assert_eq!(
        reth_db_common::init::init_genesis(&factory).unwrap(),
        genesis_hash
    );
    let provider = BlockchainProvider::new(factory).unwrap();
    // Genesis's result is the genesis header's own fields (A6).
    let carried = ExecutedFields::carried_by(&genesis_header.header);
    n42_consensus::remember_executed_fields(genesis_hash, carried).unwrap();
    let checker = DeferredChecker::new(spec.clone(), move |hash| {
        provider.state_by_block_hash(hash).ok()
    });
    Node {
        chain_id: spec.chain.id(),
        genesis_hash,
        carried,
        keys,
        checker,
    }
}

/// The payload of genesis's child carrying `transactions`, its header
/// admitted (remembered) as the proposal's.
fn proposal(node: &Node, transactions: &[Bytes], salt: u64) -> ExecutionData {
    let fields = node.carried;
    let header = Profile1Header {
        header: Header {
            parent_hash: node.genesis_hash,
            transactions_root: EMPTY_ROOT_HASH,
            withdrawals_root: Some(EMPTY_ROOT_HASH),
            state_root: fields.state_root,
            receipts_root: fields.receipts_root,
            logs_bloom: fields.logs_bloom,
            gas_used: fields.gas_used,
            requests_hash: Some(fields.requests_hash),
            number: 1,
            gas_limit: 480_000_000,
            timestamp: 8 + salt,
            base_fee_per_gas: Some(BASE_FEE),
            blob_gas_used: Some(0),
            excess_blob_gas: Some(0),
            parent_beacon_block_root: Some(B256::ZERO),
            slot_number: Some(1),
            ..Default::default()
        },
        mobile_registry_root: B256::ZERO,
    };
    let hash = n42_consensus::remember_gov5_native_header(&header.encode());
    assert_eq!(hash, header.hash());
    ExecutionData {
        payload: ExecutionPayload::V1(ExecutionPayloadV1 {
            parent_hash: node.genesis_hash,
            fee_recipient: Address::ZERO,
            state_root: fields.state_root,
            receipts_root: fields.receipts_root,
            logs_bloom: fields.logs_bloom,
            prev_randao: B256::ZERO,
            block_number: 1,
            gas_limit: 480_000_000,
            gas_used: fields.gas_used,
            timestamp: 8 + salt,
            extra_data: Bytes::new(),
            base_fee_per_gas: U256::from(BASE_FEE),
            block_hash: hash,
            transactions: transactions.to_vec(),
        }),
        sidecar: ExecutionPayloadSidecar::none(),
    }
}

fn encoded(transactions: &[N42TxEnvelope]) -> Vec<Bytes> {
    transactions
        .iter()
        .map(|tx| tx.encoded_2718().into())
        .collect()
}

/// The cache path changes where a sender comes from, never the verdict:
/// a full block passes with and without it (the second time from the
/// cache), and each refusal reads the same.
#[test]
fn sender_cache_gives_the_same_verdicts() {
    let node = node(64);
    let txs: Vec<_> = node
        .keys
        .iter()
        .enumerate()
        .map(|(n, (key, _))| transfer(*key, node.chain_id, 0, n as u64))
        .collect();
    let cache = SenderRecoveryCache::new(1 << 12);
    let cached = node
        .checker
        .clone()
        .with_sender_recovery_cache(Some(cache.clone()));

    let payload = proposal(&node, &encoded(&txs), 0);
    let (plain, timings) = node.checker.check_timed(&payload);
    assert_eq!(plain, DeferredCheck::Checked);
    assert_eq!((timings.transactions, timings.cached_senders), (64, 0));
    // First through the cache: recovered and filed.
    let (first, timings) = cached.check_timed(&payload);
    assert_eq!(first, DeferredCheck::Checked);
    assert_eq!(timings.cached_senders, 0);
    let filed = txs
        .iter()
        .zip(&node.keys)
        .filter(|(tx, (_, sender))| cache.get(tx.tx_hash()) == Some(*sender))
        .count();
    // The cache is lossy (a slot collision evicts), never wrong.
    assert!(filed >= 60, "{filed} of 64 filed");
    assert!(txs.iter().all(|tx| {
        cache
            .get(tx.tx_hash())
            .is_none_or(|sender| node.keys.iter().any(|(_, s)| *s == sender))
    }));
    // Again: the filed senders from the cache.
    let (second, timings) = cached.check_timed(&payload);
    assert_eq!(second, DeferredCheck::Checked);
    assert_eq!(timings.cached_senders, filed);

    // Refusals, each with every other sender already cached.
    let mut cases: Vec<(&str, Vec<Bytes>)> = Vec::new();
    // A wrong nonce.
    let mut wrong_nonce = txs.clone();
    wrong_nonce[5] = transfer(node.keys[5].0, node.chain_id, 1, 5);
    cases.push(("nonce", encoded(&wrong_nonce)));
    // Another chain's transaction.
    let mut other_chain = txs.clone();
    other_chain[9] = transfer(node.keys[9].0, node.chain_id + 1, 0, 9);
    cases.push(("chain id", encoded(&other_chain)));
    // A signature that does not recover (s above the curve order).
    let mut bad_signature = encoded(&txs);
    let mut raw = bad_signature[11].to_vec();
    let len = raw.len();
    raw[len - 32..].fill(0xff);
    let unrecoverable = N42TxEnvelope::decode_2718_exact(&raw).expect("still decodes");
    assert!(unrecoverable.recover_signer().is_err());
    bad_signature[11] = raw.into();
    cases.push(("signature", bad_signature));
    // A transaction that does not decode after one whose signature does
    // not recover: the decode failure is the one reported.
    let mut undecodable = encoded(&txs);
    undecodable[1] = encoded(&[unrecoverable])[0].clone();
    undecodable[20] = Bytes::from(vec![0x02, 0xc0]);
    cases.push(("decode", undecodable));
    for (salt, (what, transactions)) in cases.into_iter().enumerate() {
        let payload = proposal(&node, &transactions, salt as u64 + 1);
        let plain = node.checker.check(&payload);
        assert!(
            matches!(plain, DeferredCheck::Refused(_)),
            "{what}: {plain:?}"
        );
        if what == "decode" {
            let DeferredCheck::Refused(reason) = &plain else {
                unreachable!()
            };
            assert!(reason.contains("tx 20 does not decode"), "{reason}");
        }
        assert_eq!(cached.check(&payload), plain, "{what}");
        // And once more, with whatever the first pass cached.
        assert_eq!(cached.check(&payload), plain, "{what}");
    }
    // A failed recovery is never cached.
    assert_eq!(cache.get(&alloy_primitives::keccak256(b"none")), None);
}

/// Where the check's time goes on a 1200-transaction block (1200 senders),
/// with no sender cached (today's follower: every sender recovered here and
/// again by the import) and with the ingress having cached them.
#[test]
#[ignore = "bench"]
fn deferred_check_on_a_1200_transaction_block() {
    const TXS: u64 = 1200;
    const ROUNDS: usize = 200;
    let node = node(TXS);
    let txs: Vec<_> = node
        .keys
        .iter()
        .enumerate()
        .map(|(n, (key, _))| transfer(*key, node.chain_id, 0, n as u64))
        .collect();
    let raw = encoded(&txs);
    let payload = proposal(&node, &raw, 0);
    let median = |mut samples: Vec<f64>| {
        samples.sort_by(f64::total_cmp);
        samples[samples.len() / 2]
    };
    let report = |name: &str, samples: Vec<(f64, n42_node::el::DeferredCheckTimings)>| {
        let total = median(samples.iter().map(|(total, _)| *total).collect());
        let stage = |f: fn(&n42_node::el::DeferredCheckTimings) -> f64| {
            median(samples.iter().map(|(_, t)| f(t)).collect())
        };
        println!(
            "{name}: total {total:.2} ms | carried {:.2} senders {:.2} plan {:.2} open {:.2} state {:.2} | cached {}/{}",
            stage(|t| t.carried_ms),
            stage(|t| t.senders_ms),
            stage(|t| t.plan_ms),
            stage(|t| t.open_ms),
            stage(|t| t.state_ms),
            samples[0].1.cached_senders,
            samples[0].1.transactions,
        );
    };

    let cache = SenderRecoveryCache::default();
    let cached_checker = node
        .checker
        .clone()
        .with_sender_recovery_cache(Some(cache.clone()));
    // The ingress: each gossiped transaction recovered once, into the cache.
    for tx in &txs {
        cache.recover(tx).unwrap();
    }
    // The import beside the check: another block's senders recovered over
    // and over on the global rayon pool, as reth's payload conversion does.
    let other: Vec<_> = (TXS + 1..=2 * TXS)
        .map(|n| transfer(keypair(n).0, node.chain_id, 0, n))
        .collect();
    for load in [false, true] {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let loader = load.then(|| {
            let (stop, other) = (stop.clone(), other.clone());
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let senders: Vec<_> = other.par_iter().map(|tx| tx.recover_signer()).collect();
                    assert_eq!(senders.len(), other.len());
                }
            })
        });
        let label = if load { "beside an import" } else { "quiet" };
        // The former sender stage: decode, then recover, as two passes.
        let mut former = Vec::new();
        for _ in 0..ROUNDS {
            let started = Instant::now();
            let decoded: Vec<_> = raw
                .par_iter()
                .map(|raw| N42TxEnvelope::decode_2718_exact(raw.as_ref()).unwrap())
                .collect();
            let senders: Vec<_> = decoded.par_iter().map(|tx| tx.recover_signer()).collect();
            assert_eq!(senders.len(), TXS as usize);
            former.push(started.elapsed().as_secs_f64() * 1000.0);
        }
        println!(
            "[{label}] former decode+recover passes: {:.2} ms",
            median(former)
        );
        for (name, checker) in [
            ("no cache", &node.checker),
            ("ingress-cached senders", &cached_checker),
        ] {
            let mut samples = Vec::new();
            for _ in 0..ROUNDS {
                let started = Instant::now();
                let (outcome, timings) = checker.check_timed(&payload);
                samples.push((started.elapsed().as_secs_f64() * 1000.0, timings));
                assert_eq!(outcome, DeferredCheck::Checked);
            }
            report(&format!("[{label}] {name}"), samples);
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(loader) = loader {
            loader.join().unwrap();
        }
    }
}
