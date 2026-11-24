//! A leader's block built on its own sealed block's output
//! (`N42_BUILD_ON_OUTPUT=1`) is the block a build on the engine's state of
//! that block gives: the actual payload builder over QMDB-only state, three
//! consecutive native H2 blocks, the second and the third built both ways.
//! The third is built on the second's output while neither the first nor
//! the second is in the engine and the second's QMDB candidate is only
//! prepared, as when a leader builds two blocks ahead.
use alloy_consensus::transaction::TxHashRef as _;
use alloy_consensus::{Header, TxLegacy};
use alloy_primitives::{Address, B256, Bytes, TxKind, U256};
use n42_execution::{N42EvmConfig, N42EvmFactory};
use n42_node::native_pool::N42PooledTransaction;
use n42_node::{
    build_on_output,
    consensus_state::SharedConsensusState,
    exec_cache::{ExecutionOutputCache, RethExecutionOutputCache},
    payload::N42InnerPayloadBuilder,
    qmdb_state_reader::{self, QmdbReadsMode},
    qmdb_state_root::Gov5QmdbStateRootStore,
};
use n42_twig_core::qmdb_compat::{QmdbCompatTree, encode_gov5_account_value, gov5_account_key};
use n42_tx_types::{Block, N42TxEnvelope};
use reth_basic_payload_builder::{BuildArguments, PayloadBuilder, PayloadConfig};
use reth_ethereum_payload_builder::EthereumBuilderConfig;
use reth_ethereum_primitives::Transaction;
use reth_evm::execute::BlockExecutionOutput;
use reth_primitives_traits::{Block as _, SealedHeader, SignedTransaction};
use reth_provider::{
    providers::BlockchainProvider, test_utils::create_test_provider_factory_with_node_types,
};
use reth_storage_api::{BlockWriter, HeaderProvider as _};
use reth_testing_utils::generators::sign_tx_with_key_pair;
use reth_transaction_pool::{
    CoinbaseTipOrdering, Pool, PoolConfig, PoolTransaction, TransactionOrigin, TransactionPool,
    blobstore::InMemoryBlobStore, noop::MockTransactionValidator,
};
use secp256k1::{Keypair, SecretKey};
use std::sync::Arc;
use std::time::Duration;

type Cached = (BlockExecutionOutput<n42_tx_types::Receipt>, Vec<Address>);

/// Transfers per sender; each of the six senders' transfers fill half of
/// a block.
const PER_SENDER: u64 = 1_024;
const GAS_LIMIT: u64 = 2 * PER_SENDER * 21_000;

/// The gas price of the transfers that fill block `block` (0-based): each
/// block's above the next one's, and all above the base fee, which full
/// blocks raise by an eighth (7, 8, 9).
fn price(block: usize) -> u128 {
    30 - block as u128
}

fn account(tree: &mut QmdbCompatTree, address: Address, balance: U256) {
    tree.set(
        gov5_account_key(address.as_ref()),
        encode_gov5_account_value(0, &balance.to_be_bytes(), &B256::ZERO.0),
    );
}

fn attributes(timestamp: u64, beneficiary: Address) -> alloy_rpc_types_engine::PayloadAttributes {
    alloy_rpc_types_engine::PayloadAttributes {
        timestamp,
        prev_randao: B256::repeat_byte(1),
        suggested_fee_recipient: beneficiary,
        parent_beacon_block_root: Some(B256::ZERO),
        withdrawals: Some(Default::default()),
        ..Default::default()
    }
}

/// What a build leaves for the block's import and proposal.
struct Built {
    hash: B256,
    transactions: usize,
    output: Cached,
    /// QMDB state root and gov5 receipts root.
    roots: (B256, B256),
    execution: alloy_rpc_types_engine::ExecutionData,
}

fn take_built(
    payload: n42_node::engine_types::N42BuiltPayload,
    cache: &RethExecutionOutputCache,
) -> eyre::Result<Built> {
    let hash = payload.block().hash();
    let transactions = payload.block().body().transactions.len();
    let execution = payload.into_execution_data();
    let roots = cache
        .take_gov5_normalization(&execution)
        .ok_or_else(|| eyre::eyre!("no gov5 roots for {hash}"))?;
    let mut output = reth_evm::payload_cache::take_payload_execution::<Cached>(&hash)
        .ok_or_else(|| eyre::eyre!("no execution output for {hash}"))?;
    for reverts in output.0.state.reverts.iter_mut() {
        reverts.sort_unstable_by_key(|(address, _)| *address);
    }
    Ok(Built {
        hash,
        transactions,
        output,
        roots,
        execution,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_block_built_on_the_sealed_output_is_the_block_built_on_the_engine_state()
-> eyre::Result<()> {
    // SAFETY: set before any thread of this test binary reads the switch.
    unsafe { std::env::set_var("N42_BUILD_ON_OUTPUT", "1") };
    assert!(build_on_output::enabled());
    let chain = n42_chainspec::n42_dev_chainspec();
    let keys: Vec<_> = [1u8, 2, 3, 4, 5, 6]
        .into_iter()
        .map(|n| Keypair::from_secret_key(&SecretKey::from_secret_bytes([n; 32]).unwrap()))
        .collect();
    let senders: Vec<_> = keys
        .iter()
        .map(|key| Address::from_raw_public_key(&key.public_key().serialize_uncompressed()[1..]))
        .collect();
    let recipient = Address::repeat_byte(0x33);
    let beneficiary = Address::repeat_byte(0x44);
    let funding = U256::from(1_000_000_000_000_000u64);
    let mut tree = QmdbCompatTree::new();
    for sender in &senders {
        account(&mut tree, *sender, funding);
    }
    account(&mut tree, recipient, U256::from(1));
    account(&mut tree, beneficiary, U256::from(1));
    let base_root = B256::from(tree.root());
    let genesis = Block {
        header: Header {
            state_root: base_root,
            gas_limit: GAS_LIMIT,
            gas_used: 0,
            withdrawals_root: Some(alloy_consensus::EMPTY_ROOT_HASH),
            parent_beacon_block_root: Some(B256::ZERO),
            base_fee_per_gas: Some(7),
            timestamp: 1_700_000_000,
            blob_gas_used: Some(0),
            excess_blob_gas: Some(0),
            ..Default::default()
        },
        ..Default::default()
    }
    .seal_slow();
    let native_header = n42_consensus::Gov5NativeHeader {
        header: genesis.header().clone(),
        mobile_registry_root: None,
    };
    let genesis_hash = n42_consensus::remember_gov5_native_header(&native_header.encode());
    let genesis = genesis.into_block().seal_unchecked(genesis_hash);
    let factory = create_test_provider_factory_with_node_types::<n42_node::N42Node>(chain.clone());
    let writer = factory.provider_rw()?;
    writer.insert_block(&genesis.clone().try_recover()?)?;
    writer.commit()?;
    let provider = BlockchainProvider::new(factory.clone())?;
    let directory = tempfile::tempdir()?;
    let store = Arc::new(Gov5QmdbStateRootStore::persistent(
        genesis_hash,
        base_root,
        tree.snapshot(),
        100,
        directory.path().join("state.bin"),
    )?);
    qmdb_state_reader::register(store.clone(), QmdbReadsMode::Only)?;

    // The first two senders' transfers, at the highest price, fill the first
    // block; the next two's pay the first sender, whose balance the first
    // block changed, and fill the second; the last two's pay the third
    // sender, whose balance the second block changed, and fill the third.
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
    let mut first_block_transactions = Vec::new();
    let mut second_block_transactions = Vec::new();
    for nonce in 0..PER_SENDER {
        for (index, key) in keys.iter().enumerate() {
            let block = index / 2;
            let signed = sign_tx_with_key_pair(
                *key,
                Transaction::Legacy(TxLegacy {
                    chain_id: Some(chain.chain.id()),
                    nonce,
                    gas_price: price(block),
                    gas_limit: 21_000,
                    to: TxKind::Call([recipient, senders[0], senders[2]][block]),
                    value: U256::from(1),
                    input: Bytes::new(),
                }),
            );
            let recovered = N42TxEnvelope::from(signed)
                .try_into_recovered()
                .map_err(|_| eyre::eyre!("signature recovery failed"))?;
            match block {
                0 => first_block_transactions.push(*recovered.tx_hash()),
                1 => second_block_transactions.push(recovered.clone()),
                _ => {}
            }
            pool.add_transaction(
                TransactionOrigin::Local,
                N42PooledTransaction::try_from_consensus(recovered)?,
            )
            .await?;
        }
    }
    let config =
        N42EvmConfig::with_evm_factory(chain.clone(), N42EvmFactory::with_fast_transfers(false));
    n42_node::payload::set_builder_state_root_replaced(true);
    n42_node::payload::set_builder_qmdb_store(store.clone());
    let builder = N42InnerPayloadBuilder::new(
        provider.clone(),
        pool.clone(),
        config,
        EthereumBuilderConfig::new()
            .with_gas_limit(GAS_LIMIT)
            .with_skip_state_root(true),
        Arc::new(SharedConsensusState::new(n42_consensus::ValidatorSet::new(
            &[],
            0,
        ))),
    )
    .with_parallel_transfers(true);
    let cache = RethExecutionOutputCache::new(Some(store.clone()), chain.clone());

    // Block 1, normalized as the leader proposes it.
    let args = BuildArguments::new(
        Default::default(),
        Default::default(),
        None,
        PayloadConfig::new(
            Arc::new(genesis.clone_sealed_header()),
            attributes(1_700_000_001, beneficiary),
            Default::default(),
        ),
        Default::default(),
        None,
    );
    let first = builder
        .try_build(args)?
        .into_payload()
        .ok_or_else(|| eyre::eyre!("no first payload"))?;
    assert_eq!(first.block().body().transactions.len(), 2 * PER_SENDER as usize);
    let original_hash = first.block().hash();
    let execution = first.into_execution_data();
    let (state_root, receipts_root) = cache
        .take_gov5_normalization(&execution)
        .ok_or_else(|| eyre::eyre!("no gov5 roots for the first block"))?;
    let transactions_root = cache.built_transactions_root(original_hash);
    let normalized = n42_network::normalize_built_payload_for_gov5_h2(
        execution,
        1,
        state_root,
        receipts_root,
        transactions_root,
    )?;
    let first_hash = normalized.payload.block_hash();
    assert!(cache.rekey_gov5_built(original_hash, first_hash));
    let first_block = {
        use reth_engine_primitives::PayloadValidator;
        <n42_node::engine_validator::N42EngineValidator<_> as PayloadValidator<
            n42_node::engine_types::N42EngineTypes,
        >>::convert_payload_to_block(
            &n42_node::engine_validator::N42EngineValidator::new(
                chain.clone(),
                n42_consensus::N42HeaderProfile::Gov5H2,
            ),
            normalized.payload.clone(),
        )?
    };
    assert_eq!(first_block.hash(), first_hash);
    let first_header: SealedHeader = first_block.clone_sealed_header();
    let (first_output, _) = reth_evm::payload_cache::take_payload_execution::<Cached>(&first_hash)
        .ok_or_else(|| eyre::eyre!("no output for the first block"))?;
    let first_output = Arc::new(first_output);
    // The first block's transactions leave the pool as when it is canonical.
    assert_eq!(
        pool.remove_transactions(first_block_transactions).len(),
        2 * PER_SENDER as usize
    );

    // Neither in the provider nor published: nothing to build on.
    assert!(
        builder
            .build_on(first_header.clone(), attributes(1_700_000_002, beneficiary))
            .is_err(),
        "a block on an unknown parent"
    );
    assert!(
        build_on_output::build(
            B256::repeat_byte(0x99),
            attributes(1_700_000_002, beneficiary),
            Duration::from_millis(20),
        )
        .is_err(),
        "no builder and no published parent"
    );

    // Block 2 on block 1's output, published while block 1's QMDB candidate is
    // still prepared and not committed: the child's root job must wait for
    // the commit, which would otherwise find its candidate discarded.
    build_on_output::publish(first_header.clone(), first_output.clone());
    let on_output = {
        let builder = builder.clone();
        let parent = first_header.clone();
        std::thread::spawn(move || {
            builder.build_on(parent, attributes(1_700_000_002, beneficiary))
        })
    };
    std::thread::sleep(Duration::from_millis(300));
    let binding = store
        .prepared_binding(genesis_hash, state_root)?
        .ok_or_else(|| eyre::eyre!("the first block's candidate was discarded"))?;
    assert_eq!(
        store.commit_bound_candidate(genesis_hash, first_hash, state_root, binding)?,
        Some(state_root),
        "the first block commits its own candidate"
    );
    let on_output = take_built(
        on_output
            .join()
            .map_err(|_| eyre::eyre!("build panicked"))??,
        &cache,
    )?;
    assert_eq!(on_output.transactions, 2 * PER_SENDER as usize);

    // Block 2 normalized as the leader proposes it, then block 3 on its
    // output. Block 1 is not in the provider either, so
    // block 3 reads block 2's output over block 1's over the genesis state.
    // Block 2's candidate is prepared and not committed when block 3's root
    // job runs; that job waits for block 2's commit as block 2's waited for
    // block 1's.
    let second_normalized = n42_network::normalize_built_payload_for_gov5_h2(
        on_output.execution.clone(),
        2,
        on_output.roots.0,
        on_output.roots.1,
        cache.built_transactions_root(on_output.hash),
    )?;
    let second_block = {
        use reth_engine_primitives::PayloadValidator;
        <n42_node::engine_validator::N42EngineValidator<_> as PayloadValidator<
            n42_node::engine_types::N42EngineTypes,
        >>::convert_payload_to_block(
            &n42_node::engine_validator::N42EngineValidator::new(
                chain.clone(),
                n42_consensus::N42HeaderProfile::Gov5H2,
            ),
            second_normalized.payload.clone(),
        )?
    };
    let second_hash = second_block.hash();
    assert_eq!(second_hash, second_normalized.payload.block_hash());
    let second_header: SealedHeader = second_block.clone_sealed_header();
    let second_hashes: Vec<B256> = second_block_transactions
        .iter()
        .map(|tx| *tx.tx_hash())
        .collect();
    assert_eq!(
        pool.remove_transactions(second_hashes.clone()).len(),
        2 * PER_SENDER as usize
    );
    assert!(
        provider.header(first_hash)?.is_none(),
        "block 1 is not in the engine while block 3 is built"
    );
    build_on_output::publish(second_header.clone(), Arc::new(on_output.output.0.clone()));
    let third_on_output = {
        let builder = builder.clone();
        let parent = second_header.clone();
        std::thread::spawn(move || {
            builder.build_on(parent, attributes(1_700_000_003, beneficiary))
        })
    };
    std::thread::sleep(Duration::from_millis(300));
    let second_binding = store
        .prepared_binding(first_hash, on_output.roots.0)?
        .ok_or_else(|| eyre::eyre!("the second block's candidate was discarded"))?;
    assert_eq!(
        store.commit_bound_candidate(first_hash, second_hash, on_output.roots.0, second_binding)?,
        Some(on_output.roots.0),
        "the second block commits its own candidate"
    );
    let third_on_output = take_built(
        third_on_output
            .join()
            .map_err(|_| eyre::eyre!("third build panicked"))??,
        &cache,
    )?;
    assert_eq!(third_on_output.transactions, 2 * PER_SENDER as usize);

    // Block 2 on the engine's state of block 1, from the same pool.
    build_on_output::forget(first_hash);
    build_on_output::forget(second_hash);
    for transaction in &second_block_transactions {
        pool.add_transaction(
            TransactionOrigin::Local,
            N42PooledTransaction::try_from_consensus(transaction.clone())?,
        )
        .await?;
    }
    let writer = factory.provider_rw()?;
    writer.insert_block(&first_block.clone().try_recover()?)?;
    writer.commit()?;
    let args = BuildArguments::new(
        Default::default(),
        Default::default(),
        None,
        PayloadConfig::new(
            Arc::new(first_header.clone()),
            attributes(1_700_000_002, beneficiary),
            Default::default(),
        ),
        Default::default(),
        None,
    );
    let serial = take_built(
        builder
            .try_build(args)?
            .into_payload()
            .ok_or_else(|| eyre::eyre!("no serial payload"))?,
        &cache,
    )?;
    assert_same(&on_output, &serial, "second block")?;
    // The second block read the first sender's balance as the first block left it.
    let paid = serial
        .output
        .0
        .state
        .state
        .get(&senders[0])
        .and_then(|account| account.original_info.as_ref())
        .map(|info| info.balance)
        .ok_or_else(|| eyre::eyre!("first sender untouched"))?;
    assert_eq!(
        paid,
        funding - U256::from(PER_SENDER) * U256::from(21_000 * price(0) + 1),
        "pre-state of block 2 is block 1's post-state"
    );

    // Block 3 on the engine's state of block 2.
    assert_eq!(
        pool.remove_transactions(second_hashes).len(),
        2 * PER_SENDER as usize
    );
    let writer = factory.provider_rw()?;
    writer.insert_block(&second_block.clone().try_recover()?)?;
    writer.commit()?;
    let args = BuildArguments::new(
        Default::default(),
        Default::default(),
        None,
        PayloadConfig::new(
            Arc::new(second_header.clone()),
            attributes(1_700_000_003, beneficiary),
            Default::default(),
        ),
        Default::default(),
        None,
    );
    let third_serial = take_built(
        builder
            .try_build(args)?
            .into_payload()
            .ok_or_else(|| eyre::eyre!("no serial third payload"))?,
        &cache,
    )?;
    assert_same(&third_on_output, &third_serial, "third block")?;
    let paid = third_serial
        .output
        .0
        .state
        .state
        .get(&senders[2])
        .and_then(|account| account.original_info.as_ref())
        .map(|info| info.balance)
        .ok_or_else(|| eyre::eyre!("third sender untouched"))?;
    assert_eq!(
        paid,
        funding - U256::from(PER_SENDER) * U256::from(21_000 * price(1) + 1),
        "pre-state of block 3 is block 2's post-state"
    );
    Ok(())
}

/// The block built on the output is the block built on the engine's state.
fn assert_same(on_output: &Built, serial: &Built, which: &str) -> eyre::Result<()> {
    assert_eq!(on_output.hash, serial.hash, "{which}: block hash");
    assert_eq!(
        on_output.output.0.state, serial.output.0.state,
        "{which}: bundle and rollback originals"
    );
    assert_eq!(
        on_output.output.0.result, serial.output.0.result,
        "{which}: receipts and gas"
    );
    assert_eq!(on_output.output.1, serial.output.1, "{which}: senders");
    assert_eq!(
        on_output.roots, serial.roots,
        "{which}: QMDB state root and receipts root"
    );
    assert_eq!(
        serde_json::to_value(&on_output.execution)?,
        serde_json::to_value(&serial.execution)?,
        "{which}: payload"
    );
    Ok(())
}
