//! Production provider hook + actual payload builder + native QMDB root preparation.
//! The write-once adapter registry lives in this separate test process.
use alloy_consensus::{Header, TxLegacy};
use alloy_primitives::{Address, B256, Bytes, TxKind, U256};
use n42_execution::{N42EvmConfig, N42EvmFactory};
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
use reth_basic_payload_builder::{BuildArguments, PayloadBuilder, PayloadConfig};
use reth_ethereum_payload_builder::EthereumBuilderConfig;
use reth_ethereum_primitives::{Block, Transaction};
use reth_evm::execute::{BasicBlockExecutor, BlockExecutionOutput, Executor};
use reth_primitives_traits::{
    Block as _, SignedTransaction, crypto::secp256k1::public_key_to_address,
};
use reth_provider::{
    providers::BlockchainProvider, test_utils::create_test_provider_factory_with_chain_spec,
};
use reth_storage_api::{BlockWriter, StateProvider, StateProviderFactory};
use reth_testing_utils::generators::sign_tx_with_key_pair;
use reth_transaction_pool::{
    CoinbaseTipOrdering, EthPooledTransaction, Pool, PoolConfig, PoolTransaction,
    TransactionOrigin, TransactionPool, blobstore::InMemoryBlobStore,
    noop::MockTransactionValidator,
};
use secp256k1::{Keypair, Secp256k1, SecretKey};
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
    let secp = Secp256k1::new();
    let keys: Vec<_> = [1u8, 2]
        .into_iter()
        .map(|n| Keypair::from_secret_key(&secp, &SecretKey::from_slice(&[n; 32]).unwrap()))
        .collect();
    let senders: Vec<_> = keys
        .iter()
        .map(|key| public_key_to_address(key.public_key()))
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
    let factory = create_test_provider_factory_with_chain_spec(chain.clone());
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
        MockTransactionValidator::<EthPooledTransaction>::default(),
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
            let recovered = signed
                .try_into_recovered()
                .map_err(|_| eyre::eyre!("signature recovery failed"))?;
            assert_eq!(recovered.signer(), *sender);
            pool.add_transaction(
                TransactionOrigin::Local,
                EthPooledTransaction::try_from_consensus(recovered)?,
            )
            .await?;
        }
    }
    assert_eq!(pool.pool_size().pending, 4_096);
    let config =
        N42EvmConfig::with_evm_factory(chain.clone(), N42EvmFactory::with_fast_transfers(false));
    let builder = N42InnerPayloadBuilder::new(
        provider.clone(),
        pool,
        config.clone(),
        EthereumBuilderConfig::new().with_gas_limit(200_000_000),
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
    type Cached = (
        BlockExecutionOutput<reth_ethereum_primitives::Receipt>,
        Vec<Address>,
    );
    let (mut cached, cached_senders) =
        reth_evm::payload_cache::take_payload_execution::<Cached>(&hash).unwrap();
    let db = reth_revm::database::StateProviderDatabase::new(
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
    if reuse_builder_output {
        // The fixture consumes the builder cache to compare it with serial
        // execution. Restore that verified output for the Engine cache-hit test.
        reth_evm::payload_cache::store_payload_execution(hash, (cached, cached_senders));
    }
    // The ordinary import executor uses the same production parent provider;
    // the factory's single-transaction shortcut remains disabled.
    let import_provider = provider.clone();
    let import_config = config.with_parallel_import_provider(move |hash| {
        assert_eq!(hash, parent, "every worker opens the exact sealed parent");
        import_provider.state_by_block_hash(hash)
    });
    let updates = Arc::new(std::sync::Mutex::new(revm::state::EvmState::default()));
    let sink = updates.clone();
    let db = reth_revm::database::StateProviderDatabase::new(
        provider
            .state_by_block_hash(parent)?
            .into_evm_state_provider(),
    );
    let mut importer = BasicBlockExecutor::new(import_config.clone(), db);
    let import_recorder = metrics_util::debugging::DebuggingRecorder::new();
    let result = metrics::with_local_recorder(&import_recorder, || {
        importer.execute_one_with_state_hook(
            payload.recovered_block(),
            move |changes: revm::state::EvmState| {
                sink.lock()
                    .unwrap()
                    .extend(changes.into_iter().filter(|(_, a)| a.is_touched()));
            },
        )
    })?;
    let mut imported = importer.into_state().take_bundle();
    for reverts in imported.reverts.iter_mut() {
        reverts.sort_unstable_by_key(|(address, _)| *address);
    }
    assert_eq!(
        imported, serial.state,
        "import complete bundle and rollback originals"
    );
    assert_eq!(
        result, serial.result,
        "import receipts and gas retain sealed order"
    );
    assert!(
        import_recorder
            .snapshotter()
            .snapshot()
            .into_vec()
            .iter()
            .any(|(key, _, _, value)| {
                key.key().name() == "n42_parallel_import_executed_total"
                    && matches!(value, metrics_util::debugging::DebugValue::Counter(4_096))
            })
    );
    let updates = updates.lock().unwrap();
    for (address, account) in &imported.state {
        assert_eq!(
            updates.get(address).map(|a| &a.info),
            account.info.as_ref(),
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
            reth_ethereum_engine_primitives::EthEngineTypes,
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
            import_config,
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
                .validate_block_with_state::<reth_ethereum_engine_primitives::EthEngineTypes>(
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
        let fallback_recorder = metrics_util::debugging::DebuggingRecorder::new();
        assert!(
            metrics::with_local_recorder(&fallback_recorder, || {
                engine.validate_block_with_state::<reth_ethereum_engine_primitives::EthEngineTypes>(
                    BlockOrPayload::Block(bad_nonce.clone().into()),
                    TreeCtx::new(&mut state, &canonical),
                )
            })
            .is_err(),
            "a signed duplicate nonce must fail ordered serial fallback"
        );
        assert_eq!(store.root_for(bad_nonce.hash())?, None);
        assert!(
            !fallback_recorder
                .snapshotter()
                .snapshot()
                .into_vec()
                .iter()
                .any(|(key, _, _, _)| { key.key().name() == "n42_parallel_import_executed_total" }),
            "a sealed batch cannot silently omit a refused transaction"
        );
        let engine_recorder = metrics_util::debugging::DebuggingRecorder::new();
        let checked = metrics::with_local_recorder(&engine_recorder, || {
            engine.validate_block_with_state::<reth_ethereum_engine_primitives::EthEngineTypes>(
                if let Some(payload) = &native_payload {
                    BlockOrPayload::Payload(payload.clone())
                } else {
                    BlockOrPayload::Block(engine_block.clone().into())
                },
                TreeCtx::new(&mut state, &canonical),
            )
        })
        .map_err(|e| eyre::eyre!("Engine import failed: {e:?}"))?;
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
        if !reuse_builder_output {
            assert!(
                engine_recorder
                    .snapshotter()
                    .snapshot()
                    .into_vec()
                    .iter()
                    .any(|(key, _, _, value)| {
                        key.key().name() == "n42_parallel_import_executed_total"
                            && matches!(value, metrics_util::debugging::DebugValue::Counter(4_096))
                    }),
                "Engine must execute the local parallel batch"
            );
        }
        assert_eq!(
            store.root_for(engine_block.hash())?,
            Some(B256::from(tree.root()))
        );
        Ok(())
    })?;
    Ok(())
}
