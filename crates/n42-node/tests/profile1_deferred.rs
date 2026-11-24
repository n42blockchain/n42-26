//! Deferred execution at depth 1 on import (A10, AM25; design section 3.2):
//! gov5's V5 chain through reth's engine validator with N42's consensus,
//! engine validator, committee pool and QMDB state-root strategy.
//!
//! Header N carries N−1's stateRoot, receiptsRoot, logsBloom, gasUsed and
//! requestsHash. The QMDB state-root job compares them with this node's
//! result for N−1 (the executed-fields registry, by hash), commits N's
//! operations and files N's own result; a header carrying anything else is
//! Invalid.
//!
//! (a) V5's six blocks import, each filing exactly gov5's `executed` result;
//! (b) block 2 resealed with any one of the five carried fields changed is
//!     Invalid, and so are V5's `deferred` refusals;
//! (c) a block whose parent's result is not filed is not refused: the import
//!     fails with a provider error, which reth does not remember, and
//!     succeeds once the result is filed;
//! (d) EIP-8037: blocks 1 and 3 are state-gas blocks; the filed (and
//!     carried) gasUsed is max(regular, state), not the receipts' cumulative;
//! (e) a sibling chain: 6' (block 6 at another time) carries result(5) as 6
//!     does; 7' on 6' must carry result(6'), and carrying result(6) is
//!     Invalid.
//!
//! (f) range catch-up over a deferred chain (PR 8): the chain is extended
//!     by 20 blocks; a second node with its own reth tree and its own QMDB
//!     store, starting from genesis, imports the whole 27-block range in
//!     height order through the header gate, the deferred check (Phase B)
//!     and the engine, and every child's carried state root is that node's
//!     own root of the parent. The results are in the durable log, which a
//!     restart reopens: the restart head's result is there (design 3.8).
//!
//! The whole story is one test: the registry is per process.

use alloy_consensus::{EMPTY_ROOT_HASH, Header};
use alloy_eips::eip2718::Decodable2718;
use alloy_primitives::{Address, B256, Bloom};
use n42_chainspec::profile1::{PROFILE1_REQUESTS_HASH, Profile1Config, Profile1Header};
use n42_consensus::{ExecutedFields, N42HeaderProfile};
use n42_execution::N42EvmConfig;
use n42_node::{
    engine_types::N42EngineTypes, engine_validator::N42EngineValidator,
    qmdb_state_root::Gov5QmdbStateRootStore,
};
use n42_primitives::BlsSecretKey;
use n42_tx_types::{N42Primitives, N42TxEnvelope};
use reth_chain_state::{ExecutedBlock, NewCanonicalChain};
use reth_chainspec::BaseFeeParams;
use reth_engine_tree::tree::{
    EngineApiTreeState, TreeConfig,
    payload_validator::{BasicEngineValidator, BlockOrPayload, TreeCtx},
};
use reth_primitives_traits::SealedHeader;
use n42_consensus_service::deferred_check::DeferredCheck;
use reth_provider::{providers::BlockchainProvider, test_utils::create_test_provider_factory_with_node_types};
use serde_json::Value;
use std::{path::PathBuf, sync::Arc};

fn read_json(path: PathBuf) -> Value {
    serde_json::from_slice(&std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display())))
        .expect("json")
}

fn manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn v5() -> Value {
    read_json(manifest().join("testdata/profile1/gov5/V5/header_validity.json"))
}

fn hex_bytes(value: &Value) -> Vec<u8> {
    alloy_primitives::hex::decode(value.as_str().expect("hex")).expect("hex")
}

fn b256(value: &Value) -> B256 {
    value.as_str().expect("hex").parse().expect("b256")
}

/// V1's genesis plus V5's `allocAdditions`.
fn genesis(v5: &Value) -> Value {
    let mut genesis =
        read_json(manifest().join("../n42-chainspec/testdata/profile1/V1/genesis_8s.json"));
    for (who, account) in v5["allocAdditions"].as_object().expect("allocAdditions") {
        genesis["alloc"][who] = account.clone();
    }
    genesis
}

fn validator_keys() -> Vec<BlsSecretKey> {
    let v1 = read_json(manifest().join("../n42-chainspec/testdata/profile1/V1/header_profile_v1.json"));
    v1["validators"]
        .as_array()
        .unwrap()
        .iter()
        .map(|validator| {
            let bytes: [u8; 32] = hex_bytes(&validator["blsSecretKey"]).try_into().unwrap();
            BlsSecretKey::from_bytes(&bytes).unwrap()
        })
        .collect()
}

fn transactions(block: &Value) -> Vec<N42TxEnvelope> {
    block["transactions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|raw| N42TxEnvelope::decode_2718(&mut hex_bytes(raw).as_slice()).expect("a V5 transaction"))
        .collect()
}

/// gov5's `executed` of a V5 block, as the registry files it.
fn executed(block: &Value) -> ExecutedFields {
    let executed = &block["executed"];
    ExecutedFields {
        state_root: b256(&executed["stateRoot"]),
        receipts_root: b256(&executed["receiptsRoot"]),
        logs_bloom: executed["logsBloom"].as_str().unwrap().parse::<Bloom>().unwrap(),
        gas_used: executed["gasUsed"].as_u64().unwrap(),
        requests_hash: PROFILE1_REQUESTS_HASH,
    }
}

fn rewards(config: &Profile1Config, coinbase: Address) -> Vec<n42_consensus::Gov5Reward> {
    [coinbase, config.dev_faucet_address]
        .map(|address| n42_consensus::Gov5Reward {
            address,
            amount: config.dev_block_reward,
        })
        .to_vec()
}

/// Writes `fields` into `header` as the carried five and reseals it with
/// `key`.
fn carry(header: &Profile1Header, fields: &ExecutedFields, key: &BlsSecretKey) -> Profile1Header {
    let mut header = header.clone();
    header.header.state_root = fields.state_root;
    header.header.receipts_root = fields.receipts_root;
    header.header.logs_bloom = fields.logs_bloom;
    header.header.gas_used = fields.gas_used;
    header.header.requests_hash = Some(fields.requests_hash);
    header.seal(key).expect("reseal");
    header
}

/// How an import ended.
#[derive(Debug)]
enum Verdict {
    Valid(Box<ExecutedBlock<N42Primitives>>),
    /// The gate refused the header before the engine saw it.
    Gate(#[allow(dead_code)] String),
    /// The engine refused the block as invalid (BodyStateRootDiff from the
    /// deferred check, or any other consensus error).
    Invalid(String),
    /// The engine could not decide (a provider error: not remembered).
    Retry(String),
}

fn valid(verdict: Verdict, what: &str) -> ExecutedBlock<N42Primitives> {
    match verdict {
        Verdict::Valid(block) => *block,
        other => panic!("{what}: expected Valid, got {other:?}"),
    }
}

fn invalid_by_deferred_check(verdict: Verdict, what: &str) {
    match verdict {
        Verdict::Invalid(text) => assert!(
            text.contains("BodyStateRootDiff"),
            "{what}: refused, but not by the deferred check: {text}"
        ),
        other => panic!("{what}: expected Invalid, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v5_deferred_chain_imports_and_every_wrong_carried_field_is_invalid() -> eyre::Result<()> {
    let v5 = v5();
    let (spec, config, genesis_header) =
        n42_chainspec::profile1::profile1_chain_spec(&genesis(&v5)).expect("Profile-1 genesis");
    let spec = Arc::new(spec);
    let genesis_hash = genesis_header.hash();
    assert_eq!(genesis_hash, b256(&v5["genesisHash"]), "V5's genesis");
    n42_consensus::remember_gov5_native_header(&genesis_header.encode());
    let keys = validator_keys();
    let blocks = v5["blocks"].as_array().unwrap().clone();
    let headers: Vec<Profile1Header> = blocks
        .iter()
        .map(|block| {
            let header = Profile1Header::decode(&hex_bytes(&block["rlp"])).expect("a V5 header");
            assert_eq!(header.hash(), b256(&block["hash"]));
            header
        })
        .collect();

    let factory = create_test_provider_factory_with_node_types::<n42_node::N42Node>(spec.clone());
    assert_eq!(reth_db_common::init::init_genesis(&factory)?, genesis_hash);
    let overlay = factory.overlay_manager().clone();
    let provider = BlockchainProvider::new(factory.clone())?;
    let tree = n42_node::qmdb_state::gov5_qmdb_genesis_tree(&spec.genesis)?;
    let genesis_root = B256::from(tree.root());
    let directory = tempfile::tempdir()?;
    let store = Arc::new(Gov5QmdbStateRootStore::persistent(
        genesis_hash,
        genesis_root,
        tree.snapshot(),
        100,
        directory.path().join("state.bin"),
    )?);
    let pool = n42_consensus::shared_committee_pool(
        &n42_consensus::CommitteePoolConfig::from_genesis(&spec.genesis)?
            .expect("the Profile-1 genesis configures the committee pool"),
    )?;
    let gate = n42_consensus_service::profile1_header::Profile1ImportGate::new(&config, genesis_header.clone())
        .map_err(|e| eyre::eyre!(e))?;
    let evm = N42EvmConfig::new(spec.clone());
    let genesis_sealed: SealedHeader<Header> = spec.sealed_genesis_header();

    tokio::task::block_in_place(|| -> eyre::Result<()> {
        let tree_config = TreeConfig::default();
        let state = EngineApiTreeState::new(
            10,
            10,
            tree_config.invalid_header_hit_eviction_threshold(),
            genesis_sealed.num_hash(),
            reth_engine_tree::engine::EngineApiKind::Ethereum,
            overlay.clone(),
        );
        let canonical = provider.canonical_in_memory_state();
        let engine = BasicEngineValidator::new(
            provider.clone(),
            Arc::new(
                n42_consensus::N42Consensus::new(spec.clone())
                    .with_header_profile(N42HeaderProfile::Gov5H2)
                    .with_committee_pool(pool.clone()),
            ),
            evm.clone(),
            N42EngineValidator::new(spec.clone(), N42HeaderProfile::Gov5H2),
            tree_config,
            Box::new(reth_engine_primitives::NoopInvalidBlockHook::default()),
            overlay.clone(),
            reth_tasks::Runtime::test(),
        )
        .with_state_root_strategy(Arc::new(
            n42_node::qmdb_state_root::Gov5QmdbStateRootStrategy::new(store.clone())
                .with_chain_spec(spec.clone()),
        ));
        let (mut gate, mut engine, mut state) = (gate, engine, state);
        let canonical = &canonical;
        // The follower's path: gov5 wire -> Profile-1 decode -> header gate
        // -> Engine payload -> reth's engine validator.
        let mut import = |header: &Profile1Header, transactions: &[N42TxEnvelope]| -> Verdict {
            let rewards = rewards(&config, header.header.beneficiary);
            let wire = n42_network::encode_profile1_block_rlp(header, transactions, &rewards);
            let decoded = n42_network::decode_profile1_block_rlp(&wire).expect("decodes");
            if let Err(refusal) = gate.admit_block(&decoded) {
                return Verdict::Gate(format!("{refusal:?}"));
            }
            let payload =
                n42_consensus_service::replay_import::build_gov5_gossip_execution_data(&decoded)
                    .expect("payload");
            match engine.validate_block_with_state::<N42EngineTypes>(
                BlockOrPayload::Payload(payload),
                TreeCtx::new(&mut state, canonical),
            ) {
                Ok(checked) => Verdict::Valid(Box::new(checked.executed_block)),
                Err(error) => {
                    let text = format!("{error:?}");
                    // INVALID to the Engine API: a payload error or a
                    // validation error; a provider error is not.
                    let invalid = match &error {
                        reth_engine_tree::tree::error::InsertPayloadError::Block(block) => {
                            block.kind().is_validation_error()
                        }
                        reth_engine_tree::tree::error::InsertPayloadError::Payload(_) => true,
                    };
                    if invalid {
                        Verdict::Invalid(text)
                    } else {
                        Verdict::Retry(text)
                    }
                }
            }
        };
        // PR 6: the deferred proposal check over the parent's post-state,
        // before the import (Phase B).
        let checker = {
            let provider = provider.clone();
            n42_node::el::DeferredChecker::new(spec.clone(), move |hash| {
                use reth_provider::StateProviderFactory as _;
                provider.state_by_block_hash(hash).ok()
            })
        };
        let payload_of = |header: &Profile1Header, transactions: &[N42TxEnvelope]| {
            let rewards = rewards(&config, header.header.beneficiary);
            let wire = n42_network::encode_profile1_block_rlp(header, transactions, &rewards);
            let decoded = n42_network::decode_profile1_block_rlp(&wire).expect("decodes");
            n42_consensus::remember_gov5_native_header(&header.encode());
            n42_consensus_service::replay_import::build_gov5_gossip_execution_data(&decoded)
                .expect("payload")
        };
        // A forkchoice update to `block`: on the head, or replacing `old`.
        let make_head = |block: &ExecutedBlock<N42Primitives>, old: Vec<ExecutedBlock<N42Primitives>>| {
            overlay.insert_block(block.clone());
            canonical.update_chain(if old.is_empty() {
                NewCanonicalChain::Commit { new: vec![block.clone()] }
            } else {
                NewCanonicalChain::Reorg { new: vec![block.clone()], old }
            });
            canonical.set_canonical_head(block.recovered_block().clone_sealed_header());
        };

        // The check cannot judge block 1 either: it retries, never refuses.
        assert!(matches!(
            checker.check(&payload_of(&headers[0], &transactions(&blocks[0]))),
            DeferredCheck::Retry(_)
        ));
        // (c) Genesis's result is not filed yet: block 1 cannot be judged.
        // The engine reports a provider error, commits nothing and does not
        // remember the block as invalid.
        match import(&headers[0], &transactions(&blocks[0])) {
            Verdict::Retry(text) => assert!(text.contains("is not known here"), "{text}"),
            other => panic!("block 1 without genesis's result: {other:?}"),
        }
        assert_eq!(store.root_for(headers[0].hash())?, None);
        assert_eq!(n42_consensus::executed_fields(&headers[0].hash()), None);
        // A6: genesis's result is the genesis header's own fields, filed
        // when the durable log is installed.
        let log_path = directory.path().join("executed_fields.log");
        n42_consensus::install_executed_fields_log(&log_path, genesis_hash, &genesis_header.header)?;
        assert_eq!(
            n42_consensus::executed_fields(&genesis_hash),
            Some(ExecutedFields::carried_by(&genesis_header.header))
        );

        let mut chain: Vec<ExecutedBlock<N42Primitives>> = Vec::new();
        let mut parent = genesis_header.clone();
        for (index, (block, header)) in blocks.iter().zip(&headers).enumerate() {
            let name = block["name"].as_str().unwrap();
            let number = index as u64 + 1;
            let signer = &keys[block["signerIndex"].as_u64().unwrap() as usize];
            let parent_result = n42_consensus::executed_fields(&parent.hash()).expect("filed");

            // V5's refusals at this height: `deferred` ones are Invalid, the
            // `import` ones never reach the engine.
            for refusal in v5["refusals"].as_array().unwrap() {
                let refused = Profile1Header::decode(&hex_bytes(&refusal["rlp"])).expect("decodes");
                if refused.header.number != number {
                    continue;
                }
                let what = format!("V5 refusal {}", refusal["name"]);
                match refusal["check"].as_str().unwrap() {
                    "deferred" => invalid_by_deferred_check(
                        import(&refused, &transactions(block)),
                        &what,
                    ),
                    "import" => assert!(
                        matches!(import(&refused, &transactions(block)), Verdict::Gate(_)),
                        "{what}"
                    ),
                    _ => {}
                }
                assert_eq!(store.root_for(refused.hash())?, None, "{what}");
                assert_eq!(n42_consensus::executed_fields(&refused.hash()), None, "{what}");
            }

            // (b) Block 2 carrying a wrong value in any one field.
            if number == 2 {
                type Change = fn(&mut ExecutedFields);
                let fields: [(&str, Change); 5] = [
                    ("stateRoot", |f| f.state_root = B256::repeat_byte(0x5e)),
                    ("receiptsRoot", |f| f.receipts_root = EMPTY_ROOT_HASH),
                    ("logsBloom", |f| f.logs_bloom = Bloom::repeat_byte(0x01)),
                    ("gasUsed", |f| f.gas_used = 332_612),
                    ("requestsHash", |f| f.requests_hash = EMPTY_ROOT_HASH),
                ];
                for (field, change) in fields {
                    let mut wrong = parent_result;
                    change(&mut wrong);
                    assert_eq!(wrong.first_difference(&parent_result), Some(field));
                    let tampered = carry(header, &wrong, signer);
                    invalid_by_deferred_check(
                        import(&tampered, &transactions(block)),
                        &format!("block 2 carrying a wrong {field}"),
                    );
                    assert_eq!(store.root_for(tampered.hash())?, None, "{field}");
                }
            }

            // PR 6: the check passes before the import (Phase B), over the
            // parent's post-state; a wrong carried field is refused by it.
            assert_eq!(
                checker.check(&payload_of(header, &transactions(block))),
                DeferredCheck::Checked,
                "block {number} ({name}): includable against its parent"
            );
            let mut wrong = parent_result;
            wrong.requests_hash = EMPTY_ROOT_HASH;
            match checker.check(&payload_of(&carry(header, &wrong, signer), &transactions(block))) {
                DeferredCheck::Refused(reason) => assert!(reason.contains("carried requestsHash"), "{reason}"),
                other => panic!("block {number} carrying a wrong requestsHash: {other:?}"),
            }

            // (a) The block itself: it carries its parent's result.
            assert_eq!(ExecutedFields::carried_by(&header.header), parent_result, "{name}");
            let imported = valid(import(header, &transactions(block)), name);
            assert_eq!(imported.recovered_block().hash(), header.hash());
            let filed = n42_consensus::executed_fields(&header.hash()).expect("filed");
            assert_eq!(filed, executed(block), "block {number} ({name}): this node's result is gov5's");
            assert_eq!(store.root_for(header.hash())?, Some(filed.state_root));

            // (d) EIP-8037: the block's gas is max(regular, state).
            let gas = &block["executed"];
            let (regular, state_gas, cumulative) = (
                gas["blockRegularGas"].as_u64().unwrap(),
                gas["blockStateGas"].as_u64().unwrap(),
                gas["receiptsCumulativeGasUsed"].as_u64().unwrap(),
            );
            assert_eq!(filed.gas_used, regular.max(state_gas), "{name}");
            if matches!(number, 1 | 3) {
                assert!(state_gas > regular, "{name} is a state-gas block");
                assert_ne!(filed.gas_used, cumulative, "{name}: not the receipts' gas");
            }

            make_head(&imported, Vec::new());
            chain.push(imported);
            parent = header.clone();
        }
        assert_eq!(chain.len(), 6);

        // (e) A sibling of block 6: the same idle block at a later time. It
        // carries result(5), as block 6 does, but executes differently (the
        // EIP-4788 write depends on the time).
        let six = &headers[5];
        let mut six_sibling = six.clone();
        six_sibling.header.timestamp += 1;
        let signer6 = &keys[blocks[5]["signerIndex"].as_u64().unwrap() as usize];
        six_sibling.seal(signer6)?;
        let six_sibling_block = valid(import(&six_sibling, &[]), "6'");
        let result6 = n42_consensus::executed_fields(&six.hash()).unwrap();
        let result6s = n42_consensus::executed_fields(&six_sibling.hash()).unwrap();
        assert_ne!(result6.state_root, result6s.state_root, "6 and 6' executed differently");
        assert_eq!(
            ExecutedFields::carried_by(&six_sibling.header),
            ExecutedFields::carried_by(&six.header),
            "6 and 6' both carry result(5)"
        );
        make_head(&six_sibling_block, vec![chain[5].clone()]);

        // 7' on 6', view 7: its leader is validator 3.
        let view = 7u64;
        let leader = (view % config.validators.len() as u64) as usize;
        let coinbase = config.validators[leader].address;
        let rewards_root = n42_consensus::gov5_native_rewards_root(&rewards(&config, coinbase));
        // The CommitQC block 6 embeds names block 5, an ancestor of 7'.
        let qc = six.extra()?.qc.to_vec();
        let seven = |carried: &ExecutedFields| -> eyre::Result<Profile1Header> {
            let mut header = Profile1Header {
                header: Header {
                    parent_hash: six_sibling.hash(),
                    ommers_hash: B256::ZERO,
                    beneficiary: coinbase,
                    transactions_root: EMPTY_ROOT_HASH,
                    withdrawals_root: Some(rewards_root),
                    number: 7,
                    gas_limit: config.gas_limit,
                    timestamp: six_sibling.header.timestamp + 8,
                    extra_data: n42_chainspec::profile1::profile1_extra(view, &qc),
                    base_fee_per_gas: six_sibling
                        .header
                        .next_block_base_fee(BaseFeeParams::ethereum()),
                    blob_gas_used: Some(0),
                    excess_blob_gas: Some(0),
                    parent_beacon_block_root: Some(pool.child_beacon_root(
                        6,
                        &six_sibling.hash(),
                        &six_sibling.header.receipts_root,
                    )?),
                    slot_number: Some(7),
                    ..Default::default()
                },
                mobile_registry_root: B256::ZERO,
            };
            header = carry(&header, carried, &keys[leader]);
            Ok(header)
        };
        // Carrying the other branch's result(6) is Invalid ...
        invalid_by_deferred_check(import(&seven(&result6)?, &[]), "7' carrying result(6)");
        // ... its own chain's result(6') is Valid.
        let seven_ok = seven(&result6s)?;
        let seven_block = valid(import(&seven_ok, &[]), "7' carrying result(6')");
        assert!(n42_consensus::executed_fields(&seven_ok.hash()).is_some());
        make_head(&seven_block, Vec::new());

        // (f) Twenty more blocks on 7', each carrying its parent's result.
        let mut range: Vec<Profile1Header> = headers[..5].to_vec();
        range.extend([six_sibling.clone(), seven_ok.clone()]);
        let mut parent = seven_ok.clone();
        for number in 8..=27u64 {
            let view = number;
            let leader = (view % config.validators.len() as u64) as usize;
            let coinbase = config.validators[leader].address;
            let mut header = Profile1Header {
                header: Header {
                    parent_hash: parent.hash(),
                    ommers_hash: B256::ZERO,
                    beneficiary: coinbase,
                    transactions_root: EMPTY_ROOT_HASH,
                    withdrawals_root: Some(n42_consensus::gov5_native_rewards_root(&rewards(
                        &config, coinbase,
                    ))),
                    number,
                    gas_limit: config.gas_limit,
                    timestamp: parent.header.timestamp + 8,
                    extra_data: n42_chainspec::profile1::profile1_extra(view, &qc),
                    base_fee_per_gas: parent.header.next_block_base_fee(BaseFeeParams::ethereum()),
                    blob_gas_used: Some(0),
                    excess_blob_gas: Some(0),
                    parent_beacon_block_root: Some(pool.child_beacon_root(
                        number - 1,
                        &parent.hash(),
                        &parent.header.receipts_root,
                    )?),
                    slot_number: Some(number),
                    ..Default::default()
                },
                mobile_registry_root: B256::ZERO,
            };
            let carried = n42_consensus::executed_fields(&parent.hash()).expect("the parent's result");
            header = carry(&header, &carried, &keys[leader]);
            let block = valid(import(&header, &[]), &format!("block {number}"));
            make_head(&block, Vec::new());
            range.push(header.clone());
            parent = header;
        }
        assert_eq!(range.len(), 27);
        let head = range.last().unwrap().clone();

        // Restart (design 3.8): the durable log holds the head's own result,
        // so a restarted node at this head can build or check its child.
        let reopened = n42_consensus::executed_fields::ExecutedFieldsRegistry::open(&log_path, genesis_hash)?;
        for header in &range {
            assert_eq!(
                reopened.get(&header.hash()),
                n42_consensus::executed_fields(&header.hash()),
                "block {} is in the durable log",
                header.header.number
            );
        }
        assert!(n42_consensus::require_head_result(27, head.hash()).is_ok());

        // The catching-up node: its own tree, QMDB store and gate.
        let factory2 = create_test_provider_factory_with_node_types::<n42_node::N42Node>(spec.clone());
        assert_eq!(reth_db_common::init::init_genesis(&factory2)?, genesis_hash);
        let overlay2 = factory2.overlay_manager().clone();
        let provider2 = BlockchainProvider::new(factory2.clone())?;
        let directory2 = tempfile::tempdir()?;
        let store2 = Arc::new(Gov5QmdbStateRootStore::persistent(
            genesis_hash,
            genesis_root,
            n42_node::qmdb_state::gov5_qmdb_genesis_tree(&spec.genesis)?.snapshot(),
            100,
            directory2.path().join("state.bin"),
        )?);
        let mut gate2 = n42_consensus_service::profile1_header::Profile1ImportGate::new(
            &config,
            genesis_header.clone(),
        )
        .map_err(|e| eyre::eyre!(e))?;
        let tree_config2 = TreeConfig::default();
        let mut state2 = EngineApiTreeState::new(
            10,
            10,
            tree_config2.invalid_header_hit_eviction_threshold(),
            genesis_sealed.num_hash(),
            reth_engine_tree::engine::EngineApiKind::Ethereum,
            overlay2.clone(),
        );
        let canonical2 = provider2.canonical_in_memory_state();
        let mut engine2 = BasicEngineValidator::new(
            provider2.clone(),
            Arc::new(
                n42_consensus::N42Consensus::new(spec.clone())
                    .with_header_profile(N42HeaderProfile::Gov5H2)
                    .with_committee_pool(pool.clone()),
            ),
            evm.clone(),
            N42EngineValidator::new(spec.clone(), N42HeaderProfile::Gov5H2),
            tree_config2,
            Box::new(reth_engine_primitives::NoopInvalidBlockHook::default()),
            overlay2.clone(),
            reth_tasks::Runtime::test(),
        )
        .with_state_root_strategy(Arc::new(
            n42_node::qmdb_state_root::Gov5QmdbStateRootStrategy::new(store2.clone())
                .with_chain_spec(spec.clone()),
        ));
        let checker2 = {
            let provider = provider2.clone();
            n42_node::el::DeferredChecker::new(spec.clone(), move |hash| {
                use reth_provider::StateProviderFactory as _;
                provider.state_by_block_hash(hash).ok()
            })
        };
        let body = |header: &Profile1Header| -> Vec<N42TxEnvelope> {
            blocks
                .iter()
                .find(|block| b256(&block["hash"]) == header.hash())
                .map(transactions)
                .unwrap_or_default()
        };
        // The range in height order, as the range client releases it.
        for (index, header) in range.iter().enumerate() {
            let number = header.header.number;
            let transactions = body(header);
            let rewards = rewards(&config, header.header.beneficiary);
            let wire = n42_network::encode_profile1_block_rlp(header, &transactions, &rewards);
            let decoded = n42_network::decode_profile1_block_rlp(&wire).expect("decodes");
            gate2
                .admit_block(&decoded)
                .unwrap_or_else(|refusal| panic!("range block {number}: {refusal:?}"));
            let payload = n42_consensus_service::replay_import::build_gov5_gossip_execution_data(&decoded)
                .expect("payload");
            // Phase B: the check needs only the parent's result and state.
            assert_eq!(checker2.check(&payload), DeferredCheck::Checked, "range block {number}");
            let imported = match engine2.validate_block_with_state::<N42EngineTypes>(
                BlockOrPayload::Payload(payload),
                TreeCtx::new(&mut state2, &canonical2),
            ) {
                Ok(checked) => checked.executed_block,
                Err(error) => panic!("range block {number}: {error:?}"),
            };
            // This node's own root of the parent is what the block carries.
            assert_eq!(
                store2.root_for(header.header.parent_hash)?,
                Some(header.header.state_root),
                "range block {number}: carried root = this node's root of the parent"
            );
            overlay2.insert_block(imported.clone());
            canonical2.update_chain(NewCanonicalChain::Commit { new: vec![imported.clone()] });
            canonical2.set_canonical_head(imported.recovered_block().clone_sealed_header());
            if let Some(child) = range.get(index + 1)
                && child.header.parent_hash == header.hash()
            {
                assert_eq!(store2.root_for(header.hash())?, Some(child.header.state_root));
            }
        }
        assert_eq!(
            store2.root_for(head.hash())?,
            Some(n42_consensus::executed_fields(&head.hash()).unwrap().state_root),
            "the catching-up node's head root"
        );
        // E14 on the caught-up node: safe = 26, finalized = the persisted
        // block at or below it.
        let link = |hash: &B256| {
            range
                .iter()
                .find(|header| header.hash() == *hash)
                .map(|header| (header.header.number, header.header.parent_hash))
        };
        let (safe, finalized) = n42_consensus_service::fcu_tags::deferred_tags(
            head.hash(),
            link,
            Some((20, range[19].hash())),
        );
        assert_eq!((safe, finalized), (range[25].hash(), range[19].hash()));
        Ok(())
    })?;
    Ok(())
}
