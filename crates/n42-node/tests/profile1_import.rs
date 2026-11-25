//! A Profile-1 chain through reth: V1's `genesis_8s.json` (plus V3's funded
//! sender) -> blocks 1, 2, 3, each produced the way a leader produces it
//! (executed on the engine's state, QMDB root and native receipts root,
//! sealed into the 24-element header by
//! `n42_network::seal_built_payload_for_profile1`), sent over the gov5 wire,
//! decoded and admitted by the Profile-1 header gate, turned into its Engine
//! payload and validated by reth's engine validator with N42's consensus,
//! engine validator and QMDB state-root strategy. Both blocks are then made
//! canonical as a forkchoice update does and persisted, and read back from
//! the provider under their Profile-1 hashes.
//!
//! Deferred execution (A10/AM25): each header carries its parent's five
//! execution fields, written by the leader's seal from the executed-fields
//! registry; the leader files the block's own result under the sealed hash.
//! The engine's QMDB state-root job checks the carried fields against this
//! node's result for the parent and files the block's own again, which must
//! be the same result the leader filed.

use alloy_consensus::{BlockBody, EMPTY_OMMER_ROOT_HASH, Header, proofs};
use alloy_eips::{eip2718::Decodable2718, eip4895::Withdrawals};
use alloy_primitives::{Address, B256, Bloom};
use alloy_rpc_types_engine::ExecutionData;
use n42_chainspec::profile1::{PROFILE1_REQUESTS_HASH, Profile1Config, Profile1Header};
use n42_consensus::N42HeaderProfile;
use n42_execution::N42EvmConfig;
use n42_node::{
    engine_types::N42EngineTypes, engine_validator::N42EngineValidator,
    qmdb_state::gov5_block_operations, qmdb_state_root::Gov5QmdbStateRootStore,
};
use n42_primitives::BlsSecretKey;
use n42_tx_types::{Block, N42TxEnvelope};
use reth_chain_state::{ExecutedBlock, NewCanonicalChain};
use reth_chainspec::{ChainSpec, EthChainSpec};
use reth_engine_tree::tree::{
    EngineApiTreeState, TreeConfig,
    payload_validator::{BasicEngineValidator, BlockOrPayload, TreeCtx},
};
use reth_evm::execute::{BasicBlockExecutor, Executor};
use reth_primitives_traits::{RecoveredBlock, SealedHeader, SignedTransaction};
use reth_provider::{
    BlockHashReader, BlockNumReader, HeaderProvider, SaveBlocksInput,
    providers::BlockchainProvider, test_utils::create_test_provider_factory_with_node_types,
};
use reth_storage_api::StateProviderFactory;
use serde_json::Value;
use std::{path::PathBuf, sync::Arc};

fn read_json(path: PathBuf) -> Value {
    serde_json::from_slice(&std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display())))
        .expect("json")
}

fn manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// V1's test genesis plus V3's funded sender.
fn genesis() -> Value {
    let mut genesis =
        read_json(manifest().join("../n42-chainspec/testdata/profile1/V1/genesis_8s.json"));
    let v3 = read_json(manifest().join("testdata/profile1/gov5/V3/main.json"));
    for (who, account) in v3["allocAdditions"].as_object().expect("allocAdditions") {
        genesis["alloc"][who] = account.clone();
    }
    genesis
}

/// The raw transactions of V3 `main.json` case `case`.
fn v3_transactions(case: &str) -> Vec<N42TxEnvelope> {
    let v3 = read_json(manifest().join("testdata/profile1/gov5/V3/main.json"));
    let block = v3["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|block| block["case"] == case)
        .expect(case);
    block["transactions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|raw| {
            let bytes = alloy_primitives::hex::decode(raw.as_str().unwrap()).unwrap();
            N42TxEnvelope::decode_2718(&mut bytes.as_slice()).expect("a V3 transaction")
        })
        .collect()
}

/// The validators' test BLS secret keys of V1, by index.
fn validator_keys() -> Vec<BlsSecretKey> {
    let v1 = read_json(manifest().join("../n42-chainspec/testdata/profile1/V1/header_profile_v1.json"));
    v1["validators"]
        .as_array()
        .unwrap()
        .iter()
        .map(|validator| {
            let bytes: [u8; 32] = alloy_primitives::hex::decode(
                validator["blsSecretKey"].as_str().unwrap(),
            )
            .unwrap()
            .try_into()
            .unwrap();
            BlsSecretKey::from_bytes(&bytes).unwrap()
        })
        .collect()
}

/// A8: `devBlockReward` to the coinbase and to the faucet.
fn rewards(config: &Profile1Config, coinbase: Address) -> Vec<n42_consensus::Gov5Reward> {
    [coinbase, config.dev_faucet_address]
        .map(|address| n42_consensus::Gov5Reward {
            address,
            amount: config.dev_block_reward,
        })
        .to_vec()
}

/// What the leader of `view` builds on `parent`: the block executed on the
/// engine's state of the parent, sealed into its Profile-1 header.
struct Produced {
    header: Profile1Header,
    transactions: Vec<N42TxEnvelope>,
    rewards: Vec<n42_consensus::Gov5Reward>,
    payload: ExecutionData,
    qmdb_root: B256,
    /// The builder's standard payload, before sealing.
    standard: ExecutionData,
    leader: usize,
    /// The block's own result, which its child carries.
    own: n42_consensus::ExecutedFields,
}

#[allow(clippy::too_many_arguments)]
fn produce(
    spec: &Arc<ChainSpec>,
    config: &Profile1Config,
    evm: &N42EvmConfig,
    tree: &mut n42_twig_core::qmdb_compat::QmdbCompatTree,
    state: reth_storage_api::StateProviderBox,
    parent: &Profile1Header,
    view: u64,
    transactions: Vec<N42TxEnvelope>,
    keys: &[BlsSecretKey],
) -> eyre::Result<Produced> {
    let leader = (view % config.validators.len() as u64) as usize;
    let coinbase = config.validators[leader].address;
    let rewards = rewards(config, coinbase);
    let withdrawals = n42_consensus::gov5_rewards_to_withdrawals(&rewards)?;
    let number = parent.header.number + 1;
    // AM27 (A17/B4): the next slot on the genesis grid.
    let timestamp = parent.header.timestamp + config.block_interval_ms / 1000;
    let base_fee = parent
        .header
        .next_block_base_fee(spec.base_fee_params_at_timestamp(timestamp))
        .expect("London");
    // The builder's standard header: execution inputs as the leader sets
    // them; the result fields are filled in after execution.
    let mut header = Header {
        parent_hash: parent.hash(),
        ommers_hash: EMPTY_OMMER_ROOT_HASH,
        beneficiary: coinbase,
        transactions_root: proofs::calculate_transaction_root(&transactions),
        withdrawals_root: Some(proofs::calculate_withdrawals_root(&withdrawals)),
        number,
        gas_limit: config.gas_limit,
        timestamp,
        base_fee_per_gas: Some(base_fee),
        blob_gas_used: Some(0),
        excess_blob_gas: Some(0),
        parent_beacon_block_root: Some(B256::ZERO),
        requests_hash: Some(PROFILE1_REQUESTS_HASH),
        ..Default::default()
    };
    let senders = transactions
        .iter()
        .map(|tx| tx.try_recover())
        .collect::<Result<Vec<_>, _>>()?;
    let block = RecoveredBlock::new_unhashed(
        Block {
            header: header.clone(),
            body: BlockBody {
                transactions: transactions.clone(),
                ommers: Vec::new(),
                withdrawals: Some(Withdrawals::new(withdrawals.clone())),
            },
        },
        senders,
    );
    let db = reth_evm::database::StateProviderDatabase::new(
        <reth_storage_api::StateProviderBox as reth_storage_api::StateProvider>::into_evm_state_provider(state),
    );
    let output = BasicBlockExecutor::new(evm.clone(), db).execute(&block)?;
    assert!(output.result.receipts.iter().all(|receipt| receipt.success));
    let operations = gov5_block_operations(&output.state, &[], spec, timestamp);
    let qmdb_root = B256::from(tree.apply_sorted_ops(operations)?);
    let receipts_root = n42_network::gov5_native_receipts_root(&output.result.receipts);
    let own = n42_consensus::ExecutedFields {
        state_root: qmdb_root,
        receipts_root,
        logs_bloom: output
            .result
            .receipts
            .iter()
            .fold(Bloom::ZERO, |bloom, receipt| {
                bloom | alloy_primitives::logs_bloom(receipt.logs.iter())
            }),
        gas_used: output.result.gas_used,
        requests_hash: output.result.requests.requests_hash(),
    };
    // The builder's payload holds the block's own result.
    header.gas_used = own.gas_used;
    header.logs_bloom = own.logs_bloom;
    header.requests_hash = Some(own.requests_hash);
    let standard = Block {
        header,
        body: BlockBody {
            transactions: transactions.clone(),
            ommers: Vec::new(),
            withdrawals: Some(Withdrawals::new(withdrawals)),
        },
    };
    let execution = ExecutionData::from_block_unchecked(standard.header.hash_slow(), &standard);
    assert_eq!(
        n42_network::profile1_built_own_fields(&execution, qmdb_root, receipts_root),
        own,
        "the leader reads its own result from its builder's payload"
    );
    // A10/AM25: the seal writes the parent's result.
    let carried = n42_consensus::executed_fields(&parent.hash()).expect("the parent's result");
    let built = n42_network::seal_built_payload_for_profile1(
        &execution,
        view,
        None,
        &carried,
        B256::ZERO,
        &keys[leader],
    )?;
    assert!(
        n42_network::profile1_seal_preserves_execution_input(&execution, &built.payload),
        "the seal changes no execution input: the leader's execution stands for the sealed block"
    );
    assert_eq!(n42_consensus::ExecutedFields::carried_by(&built.header.header), carried);
    // The leader files its own result under the sealed hash at once.
    n42_consensus::remember_executed_fields(built.header.hash(), own)?;
    built
        .header
        .verify_seal(&keys[leader].public_key())
        .expect("sealed by the leader");
    Ok(Produced {
        header: built.header,
        transactions,
        rewards,
        payload: built.payload,
        qmdb_root,
        standard: execution,
        leader,
        own,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_profile1_chain_imports_through_reth_and_becomes_canonical() -> eyre::Result<()> {
    let (spec, config, genesis_header) =
        n42_chainspec::profile1::profile1_chain_spec(&genesis()).expect("Profile-1 genesis");
    let spec = Arc::new(spec);
    let genesis_hash = genesis_header.hash();
    assert_eq!(spec.genesis_hash(), genesis_hash);
    n42_consensus::remember_gov5_native_header(&genesis_header.encode());
    n42_consensus::remember_executed_fields(
        genesis_hash,
        n42_consensus::ExecutedFields::carried_by(&genesis_header.header),
    )?;
    let keys = validator_keys();

    // reth's database: the genesis header under its Profile-1 hash and the
    // alloc as state.
    let factory = create_test_provider_factory_with_node_types::<n42_node::N42Node>(spec.clone());
    assert_eq!(reth_db_common::init::init_genesis(&factory)?, genesis_hash);
    let overlay = factory.overlay_manager().clone();
    let provider = BlockchainProvider::new(factory.clone())?;

    // QMDB: the genesis tree, as the node's root store starts.
    let mut tree = n42_node::qmdb_state::gov5_qmdb_genesis_tree(&spec.genesis)?;
    let genesis_root = B256::from(tree.root());
    assert_eq!(genesis_header.header.state_root, genesis_root);
    let directory = tempfile::tempdir()?;
    let store = Arc::new(Gov5QmdbStateRootStore::persistent(
        genesis_hash,
        genesis_root,
        tree.snapshot(),
        100,
        directory.path().join("state.bin"),
    )?);

    let mut gate =
        n42_consensus_service::profile1_header::Profile1ImportGate::new(&config, genesis_header.clone())
            .map_err(|e| eyre::eyre!(e))?;
    let evm = N42EvmConfig::new(spec.clone());
    let genesis_sealed: SealedHeader<Header> = spec.sealed_genesis_header();

    tokio::task::block_in_place(|| -> eyre::Result<()> {
        let runtime = reth_tasks::Runtime::test();
        let tree_config = TreeConfig::default();
        let mut state = EngineApiTreeState::new(
            10,
            10,
            tree_config.invalid_header_hit_eviction_threshold(),
            genesis_sealed.num_hash(),
            reth_engine_tree::engine::EngineApiKind::Ethereum,
            overlay.clone(),
        );
        // The provider's own in-memory canonical chain, which a forkchoice
        // update advances.
        let canonical = provider.canonical_in_memory_state();
        assert_eq!(canonical.get_canonical_head().hash(), genesis_hash);
        let mut engine = BasicEngineValidator::new(
            provider.clone(),
            Arc::new(
                n42_consensus::N42Consensus::new(spec.clone())
                    .with_header_profile(N42HeaderProfile::Gov5H2),
            ),
            evm.clone(),
            N42EngineValidator::new(spec.clone(), N42HeaderProfile::Gov5H2),
            tree_config,
            Box::new(reth_engine_primitives::NoopInvalidBlockHook::default()),
            overlay.clone(),
            runtime,
        )
        .with_state_root_strategy(Arc::new(
            n42_node::qmdb_state_root::Gov5QmdbStateRootStrategy::new(store.clone())
                .with_chain_spec(spec.clone()),
        ));

        let mut parent = genesis_header.clone();
        let mut executed: Vec<ExecutedBlock<n42_tx_types::N42Primitives>> = Vec::new();
        for (view, case) in [(1u64, "plain-transfer"), (2, "contract-create"), (3, "storage-set")] {
            // The leader builds on the canonical head's state.
            let parent_state = provider.state_by_block_hash(parent.hash())?;
            let produced = produce(
                &spec,
                &config,
                &evm,
                &mut tree,
                parent_state,
                &parent,
                view,
                v3_transactions(case),
                &keys,
            )?;
            let hash = produced.header.hash();
            assert_eq!(produced.payload.block_hash(), hash);

            // The follower's path: gov5 wire -> Profile-1 decode -> header
            // gate -> Engine payload.
            let wire = n42_network::encode_profile1_block_rlp(
                &produced.header,
                &produced.transactions,
                &produced.rewards,
            );
            assert_eq!(
                n42_network::encode_gov5_block_rlp(&produced.payload)?,
                wire,
                "the leader's gov5 copy of its payload is the Profile-1 wire block"
            );
            // The serve cache and the gossip fast path name the block by
            // its 24-element header hash; either decoder reads it.
            assert_eq!(n42_network::gov5_attributable_hash(&wire), Some(hash));
            assert_eq!(n42_network::decode_wire_block_rlp(&wire)?.block_hash, hash);
            let decoded = n42_network::decode_profile1_block_rlp(&wire)?;
            assert_eq!(decoded.block_hash, hash);
            gate.admit_block(&decoded)
                .map_err(|refusal| eyre::eyre!("gate refused block {view}: {refusal:?}"))?;
            let payload =
                n42_consensus_service::replay_import::build_gov5_gossip_execution_data(&decoded)?;
            assert_eq!(
                serde_json::to_value(&payload)?,
                serde_json::to_value(&produced.payload)?,
                "the follower's payload is the leader's"
            );

            // The same block carrying a wrong state root is refused by the
            // engine's deferred check, and nothing is committed for it.
            let mut wrong = n42_consensus::executed_fields(&parent.hash()).unwrap();
            wrong.state_root = B256::repeat_byte(0xfe);
            let tampered = n42_network::seal_built_payload_for_profile1(
                &produced.standard,
                view,
                None,
                &wrong,
                B256::ZERO,
                &keys[produced.leader],
            )?;
            // An execution input changed is not the leader's execution.
            let mut moved = tampered.payload.clone();
            moved.payload.as_v1_mut().timestamp += 1;
            assert!(!n42_network::profile1_seal_preserves_execution_input(&produced.standard, &moved));
            let tampered_hash = tampered.payload.block_hash();
            let refused = engine.validate_block_with_state::<N42EngineTypes>(
                BlockOrPayload::Payload(tampered.payload),
                TreeCtx::new(&mut state, &canonical),
            );
            assert!(
                format!("{refused:?}").contains("BodyStateRootDiff"),
                "a wrong QMDB root is refused as such: {refused:?}"
            );
            assert_eq!(store.root_for(tampered_hash)?, None);

            // reth's engine: payload conversion (binding the 24-element
            // header), consensus, execution, QMDB root, post-execution.
            let checked = engine
                .validate_block_with_state::<N42EngineTypes>(
                    BlockOrPayload::Payload(payload),
                    TreeCtx::new(&mut state, &canonical),
                )
                .map_err(|e| eyre::eyre!("block {view} refused by the engine: {e:?}"))?;
            let block = checked.executed_block.recovered_block();
            assert_eq!(block.hash(), hash);
            assert_eq!(block.header().slot_number, Some(block.header().number));
            assert_eq!(block.header().requests_hash, Some(PROFILE1_REQUESTS_HASH));
            assert_eq!(store.root_for(hash)?, Some(produced.qmdb_root));
            // The engine's own result for the block is the one the leader
            // filed (a different one would have failed the import).
            assert_eq!(n42_consensus::executed_fields(&hash), Some(produced.own));
            assert_eq!(
                n42_consensus::ExecutedFields::carried_by(block.header()),
                n42_consensus::executed_fields(&parent.hash()).unwrap(),
                "the header carries its parent's result"
            );
            // What the engine tree does with a valid block, then with a
            // forkchoice update to it: the block becomes the canonical head.
            overlay.insert_block(checked.executed_block.clone());
            canonical.update_chain(NewCanonicalChain::Commit {
                new: vec![checked.executed_block.clone()],
            });
            canonical.set_canonical_head(block.clone_sealed_header());
            assert_eq!(canonical.get_canonical_head().hash(), hash);
            assert_eq!(provider.block_hash(block.header().number)?, Some(hash));
            assert_eq!(
                provider.sealed_header_by_hash(hash)?.map(|header| header.hash()),
                Some(hash)
            );
            executed.push(checked.executed_block);
            parent = produced.header;
        }

        // Persistence writes the canonical chain to the database.
        let writer = factory.provider_rw()?;
        writer.save_blocks(&SaveBlocksInput::new(executed.clone(), 0, 0, 3, 3))?;
        writer.commit()?;
        let reader = factory.provider()?;
        assert_eq!(reader.last_block_number()?, 3);
        for block in &executed {
            let hash = block.recovered_block().hash();
            let number = block.recovered_block().header().number;
            assert_eq!(reader.block_hash(number)?, Some(hash));
            let stored = reader.header(hash)?.expect("stored under its Profile-1 hash");
            assert_eq!(stored.slot_number, Some(number));
            assert_eq!(
                n42_consensus::remembered_profile1_header(&hash).map(|h| h.header),
                Some(stored)
            );
        }
        Ok(())
    })?;
    Ok(())
}

/// C1-C4 on block import: a block of a Profile-1 chain carrying a blob
/// transaction (V4 `type-3-blob`, C2) or a 0x50 transaction (C4) is refused
/// before execution; the valid types of V4 pass the type rule. (The other
/// V4 types do not decode at all.)
#[test]
fn profile1_block_import_refuses_transaction_types_outside_c1_c4() {
    use reth_consensus::Consensus;
    let (spec, _, genesis_header) =
        n42_chainspec::profile1::profile1_chain_spec(&genesis()).expect("Profile-1 genesis");
    let consensus = n42_consensus::N42Consensus::new(Arc::new(spec))
        .with_header_profile(N42HeaderProfile::Gov5H2);
    let v4 = read_json(manifest().join("testdata/profile1/gov5/V4/transactions.json"));
    let mut checked = 0;
    for case in v4["cases"].as_array().unwrap() {
        let bytes = alloy_primitives::hex::decode(case["raw"].as_str().unwrap()).unwrap();
        let Ok(transaction) = N42TxEnvelope::decode_2718(&mut bytes.as_slice()) else {
            continue;
        };
        let transactions = vec![transaction];
        let mut header = genesis_header.header.clone();
        header.number = 1;
        header.parent_hash = genesis_header.hash();
        header.transactions_root = proofs::calculate_transaction_root(&transactions);
        header.extra_data = n42_chainspec::profile1::profile1_extra(1, &[]);
        let block = Block {
            header,
            body: BlockBody {
                transactions,
                ommers: Vec::new(),
                withdrawals: Some(Withdrawals::default()),
            },
        };
        let verdict = consensus.validate_block_pre_execution(&reth_primitives_traits::Block::seal_slow(block));
        let refused_by_type = verdict
            .as_ref()
            .err()
            .is_some_and(|error| error.to_string().contains("profile 1 (C"));
        let name = case["case"].as_str().unwrap();
        match case["typeByte"].as_str().unwrap() {
            "0x03" | "0x50" => assert!(refused_by_type, "{name}: {verdict:?}"),
            _ => assert!(!refused_by_type, "{name}: {verdict:?}"),
        }
        checked += 1;
    }
    assert!(checked >= 8, "the decodable V4 cases were checked");
}
