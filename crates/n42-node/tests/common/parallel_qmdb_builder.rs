//! Production provider hook + actual payload builder + native QMDB root preparation.
//! The write-once adapter registry lives in this separate test process.
use alloy_consensus::{Header, TxLegacy};
use alloy_primitives::{Address, B256, Bytes, TxKind, U256};
use n42_execution::{N42EvmConfig, N42EvmFactory};
use n42_node::native_pool::N42PooledTransaction;
use n42_node::{
    consensus_state::SharedConsensusState,
    exec_cache::{ExecutionOutputCache, RethExecutionOutputCache},
    payload::N42InnerPayloadBuilder,
    qmdb_state_reader::{self, QmdbReadsMode},
    qmdb_state_root::Gov5QmdbStateRootStore,
};
use n42_twig_core::qmdb_compat::{
    QmdbCompatTree, QmdbOperation, encode_gov5_account_value, gov5_account_key,
};
use n42_tx_types::{Block, N42TxEnvelope};
use reth_basic_payload_builder::{BuildArguments, PayloadBuilder, PayloadConfig};
use reth_ethereum_payload_builder::EthereumBuilderConfig;
use reth_ethereum_primitives::Transaction;
use reth_evm::execute::{BasicBlockExecutor, BlockExecutionOutput, Executor};
use reth_primitives_traits::{
    Block as _, SignedTransaction,
};
use reth_provider::{
    providers::BlockchainProvider, test_utils::create_test_provider_factory_with_node_types,
};
use reth_storage_api::{BlockWriter, StateProvider, StateProviderFactory};
use reth_testing_utils::generators::sign_tx_with_key_pair;
use reth_transaction_pool::{
    CoinbaseTipOrdering, Pool, PoolConfig, PoolTransaction, TransactionOrigin, TransactionPool,
    blobstore::InMemoryBlobStore, noop::MockTransactionValidator,
};
use secp256k1::{Keypair, SecretKey};
use std::sync::Arc;

fn account(tree: &mut QmdbCompatTree, address: Address, nonce: u64, balance: U256) {
    tree.set(
        gov5_account_key(address.as_ref()),
        encode_gov5_account_value(nonce, &balance.to_be_bytes(), &B256::ZERO.0),
    );
}

#[allow(dead_code)] // The cache-hit test binary calls run_with_cache directly.
pub async fn run(native: bool) -> eyre::Result<()> {
    run_with_cache(native, false).await
}

pub async fn run_with_cache(native: bool, reuse_builder_output: bool) -> eyre::Result<()> {
    run_mode(native, reuse_builder_output, false).await
}

/// The native block imported as a follower imports it with
/// `N42_PARALLEL_IMPORT=1`: executed by the parallel import over QMDB-only
/// state, the output filed for the engine, which validates it. Before that,
/// outputs that are deliberately wrong are refused by the engine as outputs,
/// not as the block.
#[allow(dead_code)]
pub async fn run_follower() -> eyre::Result<()> {
    run_mode(true, false, true).await
}

async fn run_mode(native: bool, reuse_builder_output: bool, follower: bool) -> eyre::Result<()> {
    assert!(
        !reuse_builder_output || native,
        "cache rekey requires the native profile"
    );
    let profile = if native {
        n42_consensus::N42HeaderProfile::Gov5H2
    } else {
        n42_consensus::N42HeaderProfile::Ethereum
    };
    let chain = n42_chainspec::n42_dev_chainspec();
    let keys: Vec<_> = [1u8, 2]
        .into_iter()
        .map(|n| Keypair::from_secret_key(&SecretKey::from_secret_bytes([n; 32]).unwrap()))
        .collect();
    let senders: Vec<_> = keys
        .iter()
        .map(|key| Address::from_raw_public_key(&key.public_key().serialize_uncompressed()[1..]))
        .collect();
    let recipient = Address::repeat_byte(0x33);
    let beneficiary = Address::repeat_byte(0x44);
    let untouched = Address::repeat_byte(0x55);
    let funding = U256::from(1_000_000_000_000_000u64);
    let mut tree = QmdbCompatTree::new();
    for sender in &senders {
        account(&mut tree, *sender, 0, funding);
    }
    account(&mut tree, recipient, 0, U256::from(1));
    account(&mut tree, beneficiary, 0, U256::from(1));
    account(&mut tree, untouched, 7, U256::from(42));
    let base_root = B256::from(tree.root());
    let genesis = Block {
        header: Header {
            state_root: base_root,
            gas_limit: 200_000_000,
            gas_used: 0,
            withdrawals_root: native.then_some(alloy_consensus::EMPTY_ROOT_HASH),
            parent_beacon_block_root: native.then_some(B256::ZERO),
            base_fee_per_gas: Some(7),
            timestamp: 1_700_000_000,
            blob_gas_used: Some(0),
            excess_blob_gas: Some(0),
            ..Default::default()
        },
        ..Default::default()
    }
    .seal_slow();
    let genesis = if native {
        let native_header = n42_consensus::Gov5NativeHeader {
            header: genesis.header().clone(),
            mobile_registry_root: None,
        };
        let hash = n42_consensus::remember_gov5_native_header(&native_header.encode());
        genesis.into_block().seal_unchecked(hash)
    } else {
        genesis
    };
    let parent = genesis.hash();
    let factory = create_test_provider_factory_with_node_types::<n42_node::N42Node>(chain.clone());
    let writer = factory.provider_rw()?;
    writer.insert_block(&genesis.clone().try_recover()?)?;
    writer.commit()?;
    let provider = BlockchainProvider::new(factory)?;
    assert!(
        provider
            .state_by_block_hash(parent)?
            .basic_account(&senders[0])?
            .is_none()
    );
    let directory = tempfile::tempdir()?;
    let store = Arc::new(Gov5QmdbStateRootStore::persistent(
        parent,
        base_root,
        tree.snapshot(),
        100,
        directory.path().join("state.bin"),
    )?);
    qmdb_state_reader::register(store.clone(), QmdbReadsMode::Only)?;
    let before = qmdb_state_reader::status(Some(parent))?;
    // Use the real in-memory pool and transaction representation. Its test
    // validator supplies admission metadata; every signature is checked below.
    let pool = Pool::new(
        MockTransactionValidator::<N42PooledTransaction>::default(),
        CoinbaseTipOrdering::default(),
        InMemoryBlobStore::default(),
        PoolConfig {
            max_account_slots: 4_096,
            ..Default::default()
        }
        .with_disabled_protocol_base_fee(),
    );
    const PER_SENDER: u64 = 2_048;
    for nonce in 0..PER_SENDER {
        for (key, sender) in keys.iter().zip(&senders) {
            let signed = sign_tx_with_key_pair(
                *key,
                Transaction::Legacy(TxLegacy {
                    chain_id: Some(chain.chain.id()),
                    nonce,
                    gas_price: 9,
                    gas_limit: 21_000,
                    to: TxKind::Call(recipient),
                    value: U256::from(1),
                    input: Bytes::new(),
                }),
            );
            let recovered = N42TxEnvelope::from(signed)
                .try_into_recovered()
                .map_err(|_| eyre::eyre!("signature recovery failed"))?;
            assert_eq!(recovered.signer(), *sender);
            pool.add_transaction(
                TransactionOrigin::Local,
                N42PooledTransaction::try_from_consensus(recovered)?,
            )
            .await?;
        }
    }
    assert_eq!(pool.pool_size().pending, 4_096);
    let config =
        N42EvmConfig::with_evm_factory(chain.clone(), N42EvmFactory::with_fast_transfers(false));
    n42_node::payload::set_builder_state_root_replaced(native);
    if native {
        // An H2-v4 participant's builder derives the replacing QMDB root
        // itself; the normalization below then hands that root back and it is
        // compared against the independently built tree.
        n42_node::payload::set_builder_qmdb_store(store.clone());
    }
    let builder = N42InnerPayloadBuilder::new(
        provider.clone(),
        pool,
        config.clone(),
        // The native profile builds as an H2-v4 participant does: no builder
        // state root, and the roots normalization replaces are not computed.
        EthereumBuilderConfig::new()
            .with_gas_limit(200_000_000)
            .with_skip_state_root(native),
        Arc::new(SharedConsensusState::new(n42_consensus::ValidatorSet::new(
            &[],
            0,
        ))),
    )
    .with_parallel_transfers(true);
    let attributes = alloy_rpc_types_engine::PayloadAttributes {
        timestamp: 1_700_000_001,
        prev_randao: B256::repeat_byte(1),
        suggested_fee_recipient: beneficiary,
        parent_beacon_block_root: Some(B256::ZERO),
        withdrawals: Some(Default::default()),
        ..Default::default()
    };
    let args = BuildArguments::new(
        Default::default(),
        Default::default(),
        None,
        PayloadConfig::new(
            Arc::new(genesis.clone_sealed_header()),
            attributes,
            Default::default(),
        ),
        Default::default(),
        None,
    );
    let recorder = metrics_util::debugging::DebuggingRecorder::new();
    let payload = metrics::with_local_recorder(&recorder, || builder.try_build(args))?
        .into_payload()
        .ok_or_else(|| eyre::eyre!("no payload"))?;
    assert!(
        recorder
            .snapshotter()
            .snapshot()
            .into_vec()
            .iter()
            .any(
                |(key, _, _, value)| key.key().name() == "n42_parallel_payload_executed_total"
                    && matches!(value, metrics_util::debugging::DebugValue::Counter(4_096))
            )
    );
    assert_eq!(payload.block().body().transactions.len(), 4_096);
    let after = qmdb_state_reader::status(Some(parent))?;
    assert!(after.counters.account_reads > before.counters.account_reads);
    assert_eq!(
        after.counters.read_errors
            + after.counters.provider_errors
            + after.counters.mismatches
            + after.counters.unavailable_providers,
        0
    );
    let hash = payload.block().hash();
    type Cached = (BlockExecutionOutput<n42_tx_types::Receipt>, Vec<Address>);
    let (mut cached, cached_senders) =
        reth_evm::payload_cache::take_payload_execution::<Cached>(&hash).unwrap();
    let db = reth_evm::database::StateProviderDatabase::new(
        provider
            .state_by_block_hash(parent)?
            .into_evm_state_provider(),
    );
    let mut serial =
        BasicBlockExecutor::new(config.clone(), db).execute(payload.recovered_block())?;
    for output in [&mut cached, &mut serial] {
        for reverts in output.state.reverts.iter_mut() {
            reverts.sort_unstable_by_key(|(address, _)| *address);
        }
    }
    assert_eq!(cached.state, serial.state);
    assert_eq!(cached.result, serial.result);
    if native {
        let header = payload.block().header();
        assert_eq!(header.state_root, genesis.state_root);
        assert_eq!(
            header.receipts_root,
            n42_network::gov5_native_receipts_root(&serial.result.receipts)
        );
        assert_eq!(
            header.logs_bloom,
            alloy_primitives::logs_bloom(serial.result.receipts.iter().flat_map(|r| &r.logs))
        );
    }
    if reuse_builder_output {
        // The fixture consumes the builder cache to compare it with serial
        // execution. Restore that verified output for the Engine cache-hit test.
        reth_evm::payload_cache::store_payload_execution(hash, (cached, cached_senders));
    }
    // The follower's parallel import of the built block, over the same
    // production parent provider, gives the serial execution's output.
    let import_provider = provider.clone();
    let db = reth_evm::database::StateProviderDatabase::new(
        provider
            .state_by_block_hash(parent)?
            .into_evm_state_provider(),
    );
    let (mut parallel_import, import_stats) = n42_execution::parallel_build::import_block(
        &config,
        payload.recovered_block(),
        db,
        &|| {
            let provider = import_provider.state_by_block_hash(parent).ok()?;
            Some(reth_evm::database::StateProviderDatabase::new(
                provider.into_evm_state_provider(),
            ))
        },
    )
    .map_err(|refused| eyre::eyre!("parallel import refused: {refused}"))?;
    assert_eq!((import_stats.txs, import_stats.tail), (4_096, 0));
    for reverts in parallel_import.state.reverts.iter_mut() {
        reverts.sort_unstable_by_key(|(address, _)| *address);
    }
    assert_eq!(
        parallel_import.state, serial.state,
        "parallel import complete bundle and rollback originals"
    );
    assert_eq!(
        parallel_import.result, serial.result,
        "parallel import receipts and gas retain sealed order"
    );
    // The ordinary import executor with its state hook, as the engine runs
    // a block the parallel import refused.
    let updates = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let sink = updates.clone();
    let db = reth_evm::database::StateProviderDatabase::new(
        provider
            .state_by_block_hash(parent)?
            .into_evm_state_provider(),
    );
    let mut imported = BasicBlockExecutor::new(config.clone(), db).execute_with_state_hook(
        payload.recovered_block(),
        move |update: reth_execution_types::StateUpdate| {
            let mut latest = sink.lock().unwrap();
            for account in update.accounts {
                latest.insert(account.address, account.current);
            }
        },
    )?;
    for reverts in imported.state.reverts.iter_mut() {
        reverts.sort_unstable_by_key(|(address, _)| *address);
    }
    assert_eq!(
        imported.state, serial.state,
        "import complete bundle and rollback originals"
    );
    assert_eq!(
        imported.result, serial.result,
        "import receipts and gas retain sealed order"
    );
    let updates = updates.lock().unwrap();
    for (address, account) in &imported.state.state {
        assert_eq!(
            updates.get(address),
            Some(&account.info),
            "root notification for {address}"
        );
    }
    drop(updates);
    // The actual normalization bridge consumes the builder's broadcast copy.
    // Compare its binary QMDB candidate root to independently derived balances,
    // including an untouched account that is absent from the Reth database.
    let balances = senders
        .iter()
        .map(|sender| {
            (
                *sender,
                PER_SENDER,
                funding - U256::from(PER_SENDER) * U256::from(21_000 * 9 + 1),
            )
        })
        .chain([
            (recipient, 0, U256::from(4_097)),
            (beneficiary, 0, U256::from(1 + 4_096 * 21_000 * 2u64)),
        ]);
    let mut operations: Vec<_> = balances
        .map(|(address, nonce, balance)| QmdbOperation {
            key: gov5_account_key(address.as_ref()),
            value: Some(encode_gov5_account_value(
                nonce,
                &balance.to_be_bytes(),
                &B256::ZERO.0,
            )),
        })
        .collect();
    // Frozen leaf positions are part of the root: apply the protocol's sorted
    // operation order, not the arbitrary order of the independent balance list.
    operations.sort_unstable_by_key(|operation| operation.key);
    tree.apply_sorted_ops(operations)?;
    let mut engine_block = payload.block().clone().into_block();
    engine_block.header.state_root = B256::from(tree.root());
    let engine_block = engine_block.seal_slow();
    let block_senders = payload.recovered_block().senders().to_vec();
    let execution = payload.into_execution_data();
    let roots = RethExecutionOutputCache::new(Some(store.clone()), chain.clone())
        .take_gov5_normalization(&execution)
        .ok_or_else(|| eyre::eyre!("native normalization root unavailable"))?;
    assert_eq!(roots.0, B256::from(tree.root()));
    assert_eq!(
        roots.1,
        n42_network::gov5_native_receipts_root(&serial.result.receipts)
    );
    assert_eq!(store.root_for(parent)?, Some(base_root));
    assert_eq!(
        store.root_for(hash)?,
        None,
        "preparing a candidate must not commit it"
    );
    let native_payload = if native {
        let normalized =
            n42_network::normalize_execution_payload_for_gov5_h2(&execution, 1, roots.0, roots.1)?;
        assert_ne!(normalized.block_hash(), hash);
        let encoded = n42_network::encode_gov5_block_rlp(&normalized)?;
        let decoded = n42_network::decode_gov5_block_rlp(&encoded)?;
        assert_eq!(decoded.block_hash, normalized.block_hash());
        assert_eq!(decoded.mobile_registry_root, Some(B256::ZERO));
        assert_eq!(decoded.header.parent_beacon_block_root, Some(B256::ZERO));
        assert_eq!(
            decoded.header.withdrawals_root,
            Some(alloy_primitives::keccak256([]))
        );
        assert_eq!(decoded.header.receipts_root, roots.1);
        assert!(decoded.rewards.is_empty());
        let wire_block = alloy_consensus::Block {
            header: decoded.header,
            body: alloy_consensus::BlockBody {
                transactions: decoded.transactions,
                ommers: Vec::new(),
                withdrawals: Some(Default::default()),
            },
        };
        let payload = alloy_rpc_types_engine::ExecutionData::from_block_unchecked(
            decoded.block_hash,
            &wire_block,
        );
        assert_eq!(
            serde_json::to_value(&payload)?,
            serde_json::to_value(&normalized)?
        );
        Some(payload)
    } else {
        None
    };
    if reuse_builder_output {
        assert!(
            RethExecutionOutputCache::new(Some(store.clone()), chain.clone())
                .rekey_gov5_normalized(&execution, native_payload.as_ref().unwrap())
        );
    }
    let engine_block = if let Some(payload) = &native_payload {
        use reth_engine_primitives::PayloadValidator;
        <n42_node::engine_validator::N42EngineValidator<_> as PayloadValidator<
            n42_node::engine_types::N42EngineTypes,
        >>::convert_payload_to_block(
            &n42_node::engine_validator::N42EngineValidator::new(chain.clone(), profile),
            payload.clone(),
        )?
    } else {
        engine_block
    };
    // Exercise the production Engine validator (not only BasicBlockExecutor).
    // Changing to the independently derived binary root also changes the block
    // hash, ensuring the builder's execution cache cannot satisfy this import.
    use reth_engine_tree::tree::{
        EngineApiTreeState,
        payload_validator::{BasicEngineValidator, BlockOrPayload, TreeCtx},
    };
    let follower_provider = provider.clone();
    tokio::task::block_in_place(|| -> eyre::Result<()> {
        let runtime = reth_tasks::Runtime::test();
        let overlay =
            reth_storage_overlay::OverlayManager::new(runtime.state_trie_overlay_worker_pool());
        let tree_config = reth_engine_tree::tree::TreeConfig::default();
        let mut state = EngineApiTreeState::new(
            10,
            10,
            tree_config.invalid_header_hit_eviction_threshold(),
            genesis.num_hash(),
            reth_engine_tree::engine::EngineApiKind::Ethereum,
            overlay.clone(),
        );
        let canonical = reth_chain_state::CanonicalInMemoryState::with_head(
            genesis.clone_sealed_header(),
            None,
            None,
        );
        let mut engine = BasicEngineValidator::new(
            provider,
            Arc::new(n42_consensus::N42Consensus::new(chain.clone()).with_header_profile(profile)),
            config.clone(),
            n42_node::engine_validator::N42EngineValidator::new(chain.clone(), profile),
            tree_config,
            Box::new(reth_engine_primitives::NoopInvalidBlockHook::default()),
            overlay,
            runtime,
        )
        .with_state_root_strategy(Arc::new(
            n42_node::qmdb_state_root::Gov5QmdbStateRootStrategy::new(store.clone())
                .with_chain_spec(chain),
        ));
        let mut bad_root = engine_block.clone().into_block();
        bad_root.header.state_root = B256::repeat_byte(0xfe);
        let bad_root = bad_root.seal_slow();
        assert!(
            engine
                .validate_block_with_state::<n42_node::engine_types::N42EngineTypes>(
                    BlockOrPayload::Block(bad_root.clone().into()),
                    TreeCtx::new(&mut state, &canonical),
                )
                .is_err(),
            "independent QMDB root mismatch must reject the payload"
        );
        assert_eq!(store.root_for(bad_root.hash())?, None);
        let mut bad_nonce = engine_block.clone().into_block();
        let last = bad_nonce.body.transactions.len() - 1;
        bad_nonce.body.transactions[last] = bad_nonce.body.transactions[0].clone();
        bad_nonce.header.transactions_root =
            alloy_consensus::proofs::calculate_transaction_root(&bad_nonce.body.transactions);
        let bad_nonce = bad_nonce.seal_slow();
        assert!(
            engine
                .validate_block_with_state::<n42_node::engine_types::N42EngineTypes>(
                    BlockOrPayload::Block(bad_nonce.clone().into()),
                    TreeCtx::new(&mut state, &canonical),
                )
                .is_err(),
            "a signed duplicate nonce must fail the import"
        );
        assert_eq!(store.root_for(bad_nonce.hash())?, None);
        if follower {
            let payload = native_payload.as_ref().expect("the follower imports the wire payload");
            let wire_hash = payload.block_hash();
            // An output that is not the block's is refused as an output: the
            // error is the transient one, nothing is committed, and the
            // block is still importable afterwards.
            let mut wrong_state = serial.clone();
            let credited = wrong_state.state.state.get_mut(&recipient).unwrap();
            credited.info.as_mut().unwrap().balance += U256::from(1);
            let mut wrong_receipts = serial.clone();
            wrong_receipts.result.receipts[0].cumulative_gas_used += 1;
            for wrong in [wrong_state, wrong_receipts] {
                reth_evm::payload_cache::store_payload_execution(
                    wire_hash,
                    (wrong, block_senders.clone()),
                );
                let Err(refused) = engine
                    .validate_block_with_state::<n42_node::engine_types::N42EngineTypes>(
                        BlockOrPayload::Payload(payload.clone()),
                        TreeCtx::new(&mut state, &canonical),
                    )
                else {
                    panic!("a wrong cached output must not validate");
                };
                assert!(
                    format!("{refused:?}").contains("CachedExecutionRefused"),
                    "the verdict is on the output, not on the block: {refused:?}"
                );
                assert!(
                    reth_evm::payload_cache::take_payload_execution::<Cached>(&wire_hash).is_none(),
                    "the refused output is consumed"
                );
                assert_eq!(store.root_for(wire_hash)?, None);
            }
            // The follower's import: QMDB-only reads of the sealed parent.
            let reads_before = qmdb_state_reader::status(Some(parent))?.counters.account_reads;
            let open_provider = follower_provider.clone();
            let import = n42_node::el::FollowerImport::new(config.clone(), move |hash| {
                open_provider.state_by_block_hash(hash).ok()
            });
            assert!(import.prepare(payload), "the transfer block is the batches'");
            assert!(
                qmdb_state_reader::status(Some(parent))?.counters.account_reads > reads_before,
                "the parallel import read the parent from QMDB"
            );
            // An output already filed stands: a second pass does not run.
            assert!(!import.prepare(payload));
            let (filed, filed_senders) =
                reth_evm::payload_cache::take_payload_execution::<Cached>(&wire_hash)
                    .expect("the import's output is filed under the block's hash");
            assert_eq!(filed_senders, block_senders);
            let mut sorted = filed.clone();
            for reverts in sorted.state.reverts.iter_mut() {
                reverts.sort_unstable_by_key(|(address, _)| *address);
            }
            assert_eq!(sorted.state, serial.state);
            assert_eq!(sorted.result, serial.result);
            reth_evm::payload_cache::store_payload_execution(wire_hash, (filed, filed_senders));
        }
        let checked = engine
            .validate_block_with_state::<n42_node::engine_types::N42EngineTypes>(
                if let Some(payload) = &native_payload {
                    BlockOrPayload::Payload(payload.clone())
                } else {
                    BlockOrPayload::Block(engine_block.clone().into())
                },
                TreeCtx::new(&mut state, &canonical),
            )
        .map_err(|e| eyre::eyre!("Engine import failed: {e:?}"))?;
        if follower {
            assert!(
                reth_evm::payload_cache::take_payload_execution::<Cached>(&engine_block.hash())
                    .is_none(),
                "the engine took the follower's output"
            );
        }
        let mut engine_output = checked.executed_block.execution_outcome().clone();
        for reverts in engine_output.state.reverts.iter_mut() {
            reverts.sort_unstable_by_key(|(address, _)| *address);
        }
        assert_eq!(engine_output.state, serial.state);
        assert_eq!(engine_output.result, serial.result);
        assert_eq!(
            canonical.get_canonical_head().hash(),
            parent,
            "validation does not advance H2 forkchoice"
        );
        assert_eq!(
            checked.executed_block.recovered_block().hash(),
            engine_block.hash()
        );
        assert_eq!(
            store.root_for(engine_block.hash())?,
            Some(B256::from(tree.root()))
        );
        Ok(())
    })?;
    Ok(())
}
