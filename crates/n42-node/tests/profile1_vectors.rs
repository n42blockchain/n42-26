//! gov5's Profile-1 conformance vectors V3 (QMDB write sets) and V4
//! (transaction validity), run against this node's execution
//! (`testdata/profile1/gov5`, byte-exact copies of n42-rs
//! `crates/n42/engine-types/testdata/profile1/gov5`; SHA-256 manifest in its
//! README).
//!
//! Ported from n42-rs `crates/n42/engine-types/src/profile1_vectors.rs` at
//! f6c2fd6aa, with n42-26's types: the chain is the Profile-1 chain spec of
//! V1's `genesis_8s.json` plus the vector's `allocAdditions`
//! (`n42_chainspec::profile1::profile1_chain_spec`, no reth Amsterdam
//! hardfork), executed by `n42_execution::N42EvmConfig` (the Amsterdam
//! execution raised by `amsterdamTime`, AM1). Each block's bundle becomes QMDB
//! operations by `n42_node::qmdb_state::gov5_block_operations`, the rule the
//! node's root job and builder apply, over the QMDB tree of the genesis alloc.
//! The block rewards (A8) are the withdrawals `devBlockReward` to the
//! coinbase and to `devFaucetAddress`.

use alloy_consensus::{BlockBody, Header};
use alloy_eips::{Typed2718, eip2718::Decodable2718};
use alloy_primitives::{Address, B256, U256};
use evm2::{
    bytecode::Bytecode,
    evm::{AccountInfo, CacheDB, EmptyDB},
};
use n42_chainspec::profile1::{PROFILE1_REQUESTS_HASH, Profile1Config, profile1_chain_spec};
use n42_execution::{N42EvmConfig, restored_slots_for};
use n42_node::qmdb_state::{gov5_block_operations, gov5_qmdb_genesis_tree, gov5_restored_slots_key};
use n42_tx_types::{Block, N42TxEnvelope};
use reth_chainspec::ChainSpec;
use reth_evm::{
    ConfigureEvm,
    execute::{BasicBlockExecutor, Executor},
};
use reth_primitives_traits::{RecoveredBlock, SignedTransaction};
use reth_trie_common::bundle_state::BundleState;
use serde_json::Value;
use std::{path::PathBuf, sync::Arc};

fn testdata() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/profile1/gov5")
}

fn vector(name: &str) -> Value {
    let path = testdata().join(name);
    let raw = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_slice(&raw).expect("vector json")
}

fn hex(value: &Value) -> Vec<u8> {
    alloy_primitives::hex::decode(value.as_str().expect("a hex string")).expect("hex")
}

fn b256(value: &Value) -> B256 {
    B256::from_slice(&hex(value))
}

fn address(value: &Value) -> Address {
    value.as_str().expect("an address").parse().expect("address")
}

fn u64_of(value: &Value) -> u64 {
    value.as_u64().expect("a number")
}

/// V1's test genesis plus `additions`, through the Profile-1 chain-spec
/// parser (P4, A5-A8, the closed key lists).
fn chain(additions: &Value) -> (Arc<ChainSpec>, Profile1Config) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../n42-chainspec/testdata/profile1/V1/genesis_8s.json");
    let mut genesis: Value =
        serde_json::from_slice(&std::fs::read(path).expect("genesis_8s.json")).expect("json");
    for (who, account) in additions.as_object().expect("allocAdditions") {
        genesis["alloc"][who] = account.clone();
    }
    let (spec, config, _) = profile1_chain_spec(&genesis).expect("a valid Profile-1 genesis");
    (Arc::new(spec), config)
}

/// The genesis state as the executor reads it.
fn genesis_state(spec: &ChainSpec) -> CacheDB<EmptyDB> {
    let mut db = CacheDB::new(EmptyDB::default());
    for (who, account) in &spec.genesis.alloc {
        let mut info = AccountInfo {
            balance: account.balance,
            nonce: account.nonce.unwrap_or_default(),
            ..Default::default()
        };
        if let Some(code) = &account.code {
            info.code_hash = alloy_primitives::keccak256(code);
            info.code = Some(Bytecode::new_raw(code.clone()));
        }
        db.insert_account_info(who, info);
        for (slot, value) in account.storage.iter().flatten() {
            db.insert_account_storage(
                who,
                &U256::from_be_bytes(slot.0),
                &U256::from_be_bytes(value.0),
            );
        }
    }
    db
}

/// A block's post-state written back into `db` for the next block.
fn apply(db: &mut CacheDB<EmptyDB>, bundle: &BundleState) {
    for (who, account) in &bundle.state {
        match &account.info {
            Some(info) => {
                let mut info = info.clone();
                if info.code.is_none() {
                    info.code = bundle.contracts.get(&info.code_hash).cloned();
                }
                db.insert_account_info(who, info);
            }
            None => {
                db.cache.accounts.insert(*who, None);
                db.cache.storage.entry(*who).or_default().wipe();
            }
        }
        for (slot, value) in &account.storage {
            db.insert_account_storage(who, slot, &value.present_value);
        }
    }
}

/// A8: `devBlockReward` to the coinbase and to the faucet, as withdrawals.
fn rewards(config: &Profile1Config, coinbase: Address) -> Vec<alloy_eips::eip4895::Withdrawal> {
    let rewards = [coinbase, config.dev_faucet_address].map(|address| n42_consensus::Gov5Reward {
        address,
        amount: config.dev_block_reward,
    });
    n42_consensus::gov5_rewards_to_withdrawals(&rewards).expect("whole gwei")
}

/// The vector's header fields as an executable Profile-1 header.
fn header(fields: &Value) -> Header {
    let number = u64_of(&fields["number"]);
    Header {
        number,
        timestamp: u64_of(&fields["timestamp"]),
        parent_hash: b256(&fields["parentHash"]),
        beneficiary: address(&fields["coinbase"]),
        gas_limit: u64_of(&fields["gasLimit"]),
        base_fee_per_gas: Some(u64_of(&fields["baseFeePerGas"])),
        parent_beacon_block_root: Some(b256(&fields["parentBeaconBlockRoot"])),
        mix_hash: b256(&fields["prevRandao"]),
        ommers_hash: B256::ZERO,
        withdrawals_root: Some(alloy_consensus::EMPTY_ROOT_HASH),
        blob_gas_used: Some(0),
        excess_blob_gas: Some(0),
        requests_hash: Some(PROFILE1_REQUESTS_HASH),
        slot_number: Some(number),
        ..Default::default()
    }
}

fn decode(raw: &[u8]) -> Result<N42TxEnvelope, String> {
    let mut buf = raw;
    let tx = N42TxEnvelope::decode_2718(&mut buf).map_err(|e| e.to_string())?;
    if !buf.is_empty() {
        return Err("trailing bytes".to_owned());
    }
    Ok(tx)
}

fn block_of(
    config: &Profile1Config,
    fields: &Value,
    txs: Vec<N42TxEnvelope>,
) -> Result<RecoveredBlock<Block>, String> {
    let header = header(fields);
    let senders = txs
        .iter()
        .map(|tx| tx.try_recover().map_err(|e| format!("recover: {e}")))
        .collect::<Result<Vec<_>, _>>()?;
    let body = BlockBody {
        transactions: txs,
        ommers: Vec::new(),
        withdrawals: Some(rewards(config, header.beneficiary).into()),
    };
    Ok(RecoveredBlock::new_unhashed(Block { header, body }, senders))
}

/// Executes `block` on `db` as the node's import does; returns the bundle
/// and the restored slots the tracking executor filed for it.
fn execute(
    evm: &N42EvmConfig,
    db: CacheDB<EmptyDB>,
    block: &RecoveredBlock<Block>,
) -> Result<(BundleState, Vec<n42_execution::RestoredSlot>), String> {
    let env_key = n42_execution::restored_slots_env_key(
        &evm.evm_env(block.header()).map_err(|e| e.to_string())?.block,
    );
    let output = BasicBlockExecutor::new(evm.clone(), db)
        .execute(block)
        .map_err(|e| e.to_string())?;
    let restored = restored_slots_for(gov5_restored_slots_key(block.sealed_block(), env_key))
        .map(|restored| restored.as_ref().clone())
        .unwrap_or_default();
    Ok((output.state, restored))
}

// --------------------------------------------------------------------- V3 --

/// One operation: key, and the value (`None` = delete).
type Op = (B256, Option<Vec<u8>>);

fn expected_ops(block: &Value) -> Vec<Op> {
    let mut ops: Vec<Op> = Vec::new();
    for op in block["accountOps"].as_array().expect("accountOps") {
        let value = (op["op"] == "write").then(|| hex(&op["qmdbValue"]));
        ops.push((b256(&op["qmdbKeyHash"]), value));
    }
    for op in block["storageOps"].as_array().expect("storageOps") {
        let value = (op["op"] == "write").then(|| hex(&op["value"]));
        ops.push((b256(&op["qmdbKeyHash"]), value));
    }
    ops.sort();
    ops
}

/// What one V3 block came to on this node.
#[derive(Debug)]
struct V3Block {
    case: String,
    ours: Vec<Op>,
    theirs: Vec<Op>,
    our_root: B256,
    their_root: B256,
    must_not_appear: Vec<(Address, bool)>,
}

/// Runs a V3 file's blocks in order, each on the post-state of the one
/// before (ours, so a difference carries forward and shows where it starts).
fn run_v3(file: &str) -> (B256, B256, Vec<V3Block>) {
    let vector = vector(file);
    let (spec, config) = chain(&vector["allocAdditions"]);
    let evm = N42EvmConfig::new(spec.clone());
    let blocks = vector["blocks"].as_array().expect("blocks");

    let mut tree = gov5_qmdb_genesis_tree(&spec.genesis).expect("the genesis tree");
    let genesis_root = B256::from(tree.root());
    assert_eq!(
        spec.genesis_header().state_root,
        genesis_root,
        "the genesis header carries the QMDB root"
    );

    let mut db = genesis_state(&spec);
    let mut out = Vec::new();
    for block in blocks {
        let case = block["case"].as_str().expect("case").to_owned();
        let txs = block["transactions"]
            .as_array()
            .expect("transactions")
            .iter()
            .map(|raw| decode(&hex(raw)).expect("a V3 transaction decodes"))
            .collect();
        let recovered = block_of(&config, &block["header"], txs).expect("a V3 block");
        let (state, restored) = execute(&evm, db.clone(), &recovered)
            .unwrap_or_else(|e| panic!("{case}: execution: {e}"));
        let operations =
            gov5_block_operations(&state, &restored, &spec, recovered.header().timestamp);
        let mut ours: Vec<Op> = operations
            .iter()
            .map(|op| (B256::from(op.key), op.value.clone()))
            .collect();
        ours.sort();
        let our_root = B256::from(tree.apply_sorted_ops(operations).expect("the block applies"));
        apply(&mut db, &state);
        let must_not_appear = block["mustNotAppear"]
            .as_array()
            .expect("mustNotAppear")
            .iter()
            .map(|who| {
                let who = address(who);
                let key = B256::from(n42_twig_core::qmdb_compat::gov5_account_key(&who.0.0));
                (who, ours.iter().any(|(k, _)| *k == key))
            })
            .collect();
        out.push(V3Block {
            case,
            theirs: expected_ops(block),
            ours,
            our_root,
            their_root: b256(&block["qmdbStateRoot"]),
            must_not_appear,
        });
    }
    (genesis_root, b256(&vector["genesisQmdbStateRoot"]), out)
}

/// Every difference of a V3 file, one line per block.
fn v3_differences(file: &str) -> Vec<String> {
    let (our_genesis, their_genesis, blocks) = run_v3(file);
    let mut differences = Vec::new();
    if our_genesis != their_genesis {
        differences.push(format!("genesis root ours {our_genesis} gov5 {their_genesis}"));
    }
    for block in &blocks {
        for (who, present) in &block.must_not_appear {
            if *present {
                differences.push(format!("{}: {who} must not appear", block.case));
            }
        }
        if block.ours != block.theirs || block.our_root != block.their_root {
            let only_ours: Vec<_> = block
                .ours
                .iter()
                .filter(|op| !block.theirs.contains(op))
                .collect();
            let only_theirs: Vec<_> = block
                .theirs
                .iter()
                .filter(|op| !block.ours.contains(op))
                .collect();
            differences.push(format!(
                "{}: root ours {} gov5 {}; only ours {only_ours:?}; only gov5 {only_theirs:?}",
                block.case, block.our_root, block.their_root
            ));
        } else {
            println!("{file} {}: equal ({} ops, root {})", block.case, block.ours.len(), block.our_root);
        }
    }
    differences
}

fn assert_v3(file: &str, cases: usize) {
    let vector = vector(file);
    assert_eq!(vector["blocks"].as_array().unwrap().len(), cases, "{file}");
    let differences = v3_differences(file);
    assert!(differences.is_empty(), "{file}:\n{}", differences.join("\n"));
}

/// The genesis header hash with the vector's alloc additions equals the one
/// gov5's block 1 names.
#[test]
fn v3_genesis_hash_equals_gov5() {
    for file in ["V3/main.json", "V3/system_address_live.json"] {
        let vector = vector(file);
        let (spec, _) = chain(&vector["allocAdditions"]);
        assert_eq!(
            spec.genesis_hash(),
            b256(&vector["blocks"][0]["header"]["parentHash"]),
            "{file}"
        );
    }
}

/// V3 `main.json`: seven blocks, every write set and root equal to gov5's.
#[test]
fn v3_main_write_sets_and_roots_equal_gov5() {
    assert_v3("V3/main.json", 7);
}

/// V3 `system_address_live.json`: `0xff..fe` exists (1 wei) and is never
/// written (D3).
#[test]
fn v3_system_address_live_write_sets_and_roots_equal_gov5() {
    assert_v3("V3/system_address_live.json", 2);
}

// --------------------------------------------------------------------- V4 --

/// This node's verdict on one V4 transaction: the first stage that refused.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Refused {
    /// The consensus envelope does not decode it.
    Decode(String),
    /// C1-C4: only 0x00, 0x01, 0x02 and 0x04 on Profile 1.
    TypeRule(String),
    /// C5: its gas limit exceeds the block's gas limit.
    BlockGas(u64),
    /// The import of block 1 carrying it alone refuses.
    Execution(String),
}

fn judge(spec: &Arc<ChainSpec>, config: &Profile1Config, block: &Value, raw: &[u8]) -> Result<(), Refused> {
    let tx = decode(raw).map_err(Refused::Decode)?;
    n42_chainspec::profile1::check_profile1_tx_type(tx.ty()).map_err(Refused::TypeRule)?;
    let gas_limit = alloy_consensus::Transaction::gas_limit(&tx);
    if gas_limit > u64_of(&block["gasLimit"]) {
        return Err(Refused::BlockGas(gas_limit));
    }
    import_alone(spec, config, block, tx).map_err(Refused::Execution)
}

/// The import alone (no type gate, no gas pre-check) of block 1 carrying
/// the transaction: what block execution says by itself.
fn import_alone(
    spec: &Arc<ChainSpec>,
    config: &Profile1Config,
    block: &Value,
    tx: N42TxEnvelope,
) -> Result<(), String> {
    let recovered = block_of(config, block, vec![tx])?;
    let evm = N42EvmConfig::new(spec.clone());
    execute(&evm, genesis_state(spec), &recovered).map(|_| ())
}

/// Every V4 case judged; `(case, gov5 verdict, gov5 stage, ours)`.
fn run_v4() -> Vec<(String, String, String, Result<(), Refused>)> {
    let vector = vector("V4/transactions.json");
    let (spec, config) = chain(&vector["allocAdditions"]);
    vector["cases"]
        .as_array()
        .expect("cases")
        .iter()
        .map(|case| {
            let ours = judge(&spec, &config, &vector["block"], &hex(&case["raw"]));
            (
                case["case"].as_str().expect("case").to_owned(),
                case["verdict"].as_str().expect("verdict").to_owned(),
                case["refusedAt"].as_str().unwrap_or("-").to_owned(),
                ours,
            )
        })
        .collect()
}

/// V4: every case gets gov5's verdict (valid / invalid) on this node. The
/// stage may differ and is printed; the verdict is the vector.
#[test]
fn v4_transaction_verdicts_equal_gov5() {
    let mut differences = Vec::new();
    for (case, verdict, stage, ours) in run_v4() {
        println!("{case:24} gov5 {verdict:7} at {stage:10} | ours {ours:?}");
        if ours.is_ok() != (verdict == "valid") {
            differences.push(format!("{case}: gov5 {verdict} at {stage}, ours {ours:?}"));
        }
    }
    assert!(differences.is_empty(), "{}", differences.join("\n"));
}

/// The import alone (block execution without the type gate and the gas
/// pre-check) on each V4 case, printed; for the report.
#[test]
fn v4_the_import_alone() {
    let vector = vector("V4/transactions.json");
    let (spec, config) = chain(&vector["allocAdditions"]);
    for case in vector["cases"].as_array().expect("cases") {
        let name = case["case"].as_str().expect("case");
        let alone = decode(&hex(&case["raw"]))
            .and_then(|tx| import_alone(&spec, &config, &vector["block"], tx));
        println!("{name:24} gov5 {:7} | import alone {alone:?}", case["verdict"].as_str().unwrap());
    }
}

/// C5: a transaction at the 2^32-1 cap, or over it, is above the 480M block
/// gas limit and refused for that; one at exactly the block gas limit is
/// valid and executes.
#[test]
fn v4_c5_gas_above_the_block_limit_is_refused() {
    let verdicts = run_v4();
    let of = |name: &str| {
        verdicts
            .iter()
            .find(|(case, ..)| case == name)
            .map(|(.., ours)| ours.clone())
            .expect(name)
    };
    assert_eq!(of("gas-at-block-limit"), Ok(()));
    assert_eq!(of("gas-at-cap"), Err(Refused::BlockGas(u32::MAX as u64)));
    assert_eq!(of("gas-over-cap"), Err(Refused::BlockGas(1 << 32)));
}

// ---------------------------------------------------------------- EIP-8282 --

/// Puts the two EIP-8282 predeploys into `db`: alloy-eips' code, nonce 1, the
/// excess slot (0) at the inhibitor 2^256-1 a deployment leaves, and one
/// queued request each (count 1, queue tail 1, the entry's slots from 4 on:
/// six for a builder deposit, three for an exit). A system call would reset
/// the excess and drain the queue into a 0x03 / 0x04 request.
fn deploy_eip8282_predeploys(db: &mut CacheDB<EmptyDB>) {
    use alloy_eips::eip8282::{
        BUILDER_DEPOSIT_CONTRACT_ADDRESS, BUILDER_DEPOSIT_CONTRACT_CODE,
        BUILDER_EXIT_CONTRACT_ADDRESS, BUILDER_EXIT_CONTRACT_CODE,
    };
    for (who, code, entry_slots) in [
        (BUILDER_DEPOSIT_CONTRACT_ADDRESS, &BUILDER_DEPOSIT_CONTRACT_CODE, 6u64),
        (BUILDER_EXIT_CONTRACT_ADDRESS, &BUILDER_EXIT_CONTRACT_CODE, 3),
    ] {
        db.insert_account_info(
            &who,
            AccountInfo {
                nonce: 1,
                code_hash: alloy_primitives::keccak256(code),
                code: Some(Bytecode::new_raw(code.clone())),
                ..Default::default()
            },
        );
        db.insert_account_storage(&who, &U256::ZERO, &U256::MAX);
        db.insert_account_storage(&who, &U256::from(1), &U256::from(1));
        db.insert_account_storage(&who, &U256::from(3), &U256::from(1));
        for slot in 4..4 + entry_slots {
            db.insert_account_storage(&who, &U256::from(slot), &U256::from(0x4242_0000 + slot));
        }
    }
}

const EIP8282_ADDRESSES: [Address; 2] = [
    alloy_eips::eip8282::BUILDER_DEPOSIT_CONTRACT_ADDRESS,
    alloy_eips::eip8282::BUILDER_EXIT_CONTRACT_ADDRESS,
];

/// What one block of [`run_eip8282_blocks`] came to.
struct Eip8282Block {
    ops: Vec<Op>,
    root: B256,
    state: BundleState,
    requests: alloy_eips::eip7685::Requests,
}

/// V3 main's first two blocks (empty, plain transfer) executed in turn on
/// `db` by `evm`, the EIP-8282 predeploys put into the state before block
/// `deploy_before` when it is `Some`: each block's sorted QMDB operations and
/// root (over the QMDB tree of `spec`'s genesis, which never holds the
/// predeploys, so roots compare the blocks' own writes), bundle and requests.
fn run_eip8282_blocks(
    spec: &Arc<ChainSpec>,
    config: &Profile1Config,
    deploy_before: Option<usize>,
) -> Vec<Eip8282Block> {
    let evm = N42EvmConfig::new(spec.clone());
    let vector = vector("V3/main.json");
    let mut tree = gov5_qmdb_genesis_tree(&spec.genesis).expect("the genesis tree");
    let mut db = genesis_state(spec);
    let mut out = Vec::new();
    for (index, block) in vector["blocks"].as_array().expect("blocks").iter().take(2).enumerate() {
        if deploy_before == Some(index) {
            deploy_eip8282_predeploys(&mut db);
        }
        let txs = block["transactions"]
            .as_array()
            .expect("transactions")
            .iter()
            .map(|raw| decode(&hex(raw)).expect("a V3 transaction decodes"))
            .collect();
        let recovered = block_of(config, &block["header"], txs).expect("a V3 block");
        let env_key = n42_execution::restored_slots_env_key(
            &evm.evm_env(recovered.header()).expect("env").block,
        );
        let output = BasicBlockExecutor::new(evm.clone(), db.clone())
            .execute(&recovered)
            .expect("the block executes");
        let restored = restored_slots_for(gov5_restored_slots_key(recovered.sealed_block(), env_key))
            .map(|restored| restored.as_ref().clone())
            .unwrap_or_default();
        let operations =
            gov5_block_operations(&output.state, &restored, spec, recovered.header().timestamp);
        let mut ops: Vec<Op> = operations
            .iter()
            .map(|op| (B256::from(op.key), op.value.clone()))
            .collect();
        ops.sort();
        let root = B256::from(tree.apply_sorted_ops(operations).expect("the block applies"));
        apply(&mut db, &output.state);
        out.push(Eip8282Block {
            ops,
            root,
            state: output.state,
            requests: output.result.requests,
        });
    }
    out
}

/// Every block of `deployed` equals the same block of `empty` (QMDB write
/// set, root, requests) and leaves both predeploys alone.
fn assert_no_eip8282_calls(empty: &[Eip8282Block], deployed: &[Eip8282Block]) {
    for (block, (empty, deployed)) in empty.iter().zip(deployed).enumerate() {
        for who in EIP8282_ADDRESSES {
            assert!(!deployed.state.state.contains_key(&who), "block {block}: {who} changed");
        }
        assert!(
            deployed.requests.iter().all(|r| !matches!(r.first(), Some(0x03 | 0x04))),
            "block {block}: a builder request"
        );
        assert_eq!(deployed.requests, empty.requests, "block {block}: requests");
        assert_eq!(deployed.ops, empty.ops, "block {block}: QMDB write set");
        assert_eq!(deployed.root, empty.root, "block {block}: QMDB root");
    }
}

/// A Profile-1 chain under its Amsterdam execution makes no EIP-8282
/// builder-request call even with code (and queued requests) at both
/// predeploys from the start: their state is untouched, no 0x03 / 0x04
/// request, and every block's QMDB write set, root and requests equal those
/// of the same blocks with the addresses empty (gov5: no
/// `UsesEIP8282BuilderRequests` on a native chain; standard D2).
#[test]
fn profile1_amsterdam_makes_no_eip8282_calls() {
    let vector = vector("V3/main.json");
    let (spec, config) = chain(&vector["allocAdditions"]);
    let timestamp = u64_of(&vector["blocks"][0]["header"]["timestamp"]);
    assert!(
        N42EvmConfig::new(spec.clone()).amsterdam_execution_active_at(timestamp),
        "the blocks run the Amsterdam rules"
    );
    assert!(!n42_execution::eip8282_builder_requests_active(&spec.genesis));

    let empty = run_eip8282_blocks(&spec, &config, None);
    let deployed = run_eip8282_blocks(&spec, &config, Some(0));
    assert_no_eip8282_calls(&empty, &deployed);
}

/// The same with the predeploys appearing mid-chain, as a Nick's-method
/// CREATE in block k would leave them: code in the state after block k (0),
/// block k+1 (1) executed on it. Block k+1 makes no call and equals the same
/// block with the addresses empty.
#[test]
fn profile1_mid_chain_eip8282_deploy_makes_no_calls() {
    let vector = vector("V3/main.json");
    let (spec, config) = chain(&vector["allocAdditions"]);
    let empty = run_eip8282_blocks(&spec, &config, None);
    let deployed = run_eip8282_blocks(&spec, &config, Some(1));
    assert_no_eip8282_calls(&empty, &deployed);
}

/// The upstream default stays on for a chain that is not N42-native: the
/// same alloc and blocks on a genesis without the Profile-1 and HotStuff keys,
/// with reth's Amsterdam scheduled, make both calls — each resets its
/// contract's excess slot and drains the queued request into a 0x03 / 0x04
/// request (and so this is what the tests above would see without the gate).
#[test]
fn non_native_amsterdam_chain_keeps_the_eip8282_calls() {
    use reth_chainspec::{EthereumHardfork, ForkCondition};

    let vector = vector("V3/main.json");
    let (spec, config) = chain(&vector["allocAdditions"]);
    let mut plain = (*spec).clone();
    for key in ["n42Profile", "consensus", "hotstuff"] {
        plain.genesis.config.extra_fields.remove(key);
    }
    plain
        .hardforks
        .insert(EthereumHardfork::Amsterdam, ForkCondition::Timestamp(0));
    let spec = Arc::new(plain);
    assert!(n42_execution::eip8282_builder_requests_active(&spec.genesis));
    assert!(n42_execution::profile1_amsterdam_execution_at(spec.as_ref()).is_none());

    let deployed = run_eip8282_blocks(&spec, &config, Some(0));
    let first = &deployed[0];
    for who in EIP8282_ADDRESSES {
        let account = first.state.state.get(&who).unwrap_or_else(|| panic!("{who} was not called"));
        let excess = account.storage.get(&U256::ZERO).expect("the excess slot changed");
        assert_eq!(excess.original_value(), U256::MAX, "{who}");
        assert_eq!(excess.present_value, U256::ZERO, "{who}");
    }
    for request_type in [0x03, 0x04] {
        assert!(
            first.requests.iter().any(|r| r.first() == Some(&request_type) && r.len() > 1),
            "no {request_type:#04x} request"
        );
    }
}
