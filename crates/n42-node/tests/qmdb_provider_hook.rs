//! Exercise the patched production provider entry point in its own process;
//! the adapter registry is deliberately process-global and write-once.
use alloy_primitives::{Address, B256, U256};
use n42_node::{
    qmdb_state_reader::{QmdbReadsMode, register},
    qmdb_state_root::{Gov5QmdbStateRootStore, QmdbBaseIdentity},
    rpc::{N42ApiServer, N42RpcServer},
};
use n42_twig_core::qmdb_compat::{
    QmdbCompatTree, encode_gov5_account_value, gov5_account_key, gov5_storage_key,
};
use reth_primitives_traits::Block;
use reth_provider::{providers::BlockchainProvider, test_utils::create_test_provider_factory};
use reth_storage_api::{BlockWriter, StateProviderFactory};
use std::sync::Arc;

#[tokio::test]
async fn blockchain_provider_reads_the_exact_qmdb_version() -> eyre::Result<()> {
    let address = Address::repeat_byte(0x41);
    let slot = B256::repeat_byte(0x42);
    let mut tree = QmdbCompatTree::new();
    tree.set(
        gov5_account_key(address.as_ref()),
        encode_gov5_account_value(3, &U256::from(1234).to_be_bytes(), &B256::ZERO.0),
    );
    tree.set(
        gov5_storage_key(address.as_ref(), &slot.0),
        U256::from(42).to_be_bytes::<32>().to_vec(),
    );
    let mut block = reth_ethereum_primitives::Block::default();
    block.header.state_root = B256::from(tree.root());
    let genesis = block.seal_slow();
    let hash = genesis.hash();
    let factory = create_test_provider_factory();
    let writer = factory.provider_rw()?;
    writer.insert_block(&genesis.try_recover()?)?;
    writer.commit()?;
    let provider = BlockchainProvider::new(factory)?;
    // The real database deliberately lacks these values. This proves that
    // after registration the production hook answers from QMDB.
    assert_eq!(
        provider
            .state_by_block_hash(hash)?
            .basic_account(&address)?,
        None
    );
    let directory = tempfile::tempdir()?;
    let store = Arc::new(
        Gov5QmdbStateRootStore::persistent(
            hash,
            B256::from(tree.root()),
            tree.snapshot(),
            100,
            directory.path().join("qmdb.bin"),
        )?
        .with_identity(QmdbBaseIdentity {
            chain_id: 1,
            genesis_hash: hash,
            block_number: 0,
        }),
    );
    let key = n42_primitives::BlsSecretKey::key_gen(&[7; 32])?.public_key();
    let expected_key = hex::encode(key.to_bytes());
    let rpc = N42RpcServer::new(Arc::new(
        n42_node::consensus_state::SharedConsensusState::new(n42_consensus::ValidatorSet::new(
            &[],
            0,
        )),
    ))
    .with_validator_public_key(key);
    let disabled = rpc.state_read_status(Some(hash)).await?;
    assert_eq!(disabled.reads.mode, QmdbReadsMode::Off);
    assert!(!disabled.reads.wal_enabled);
    register(store.clone(), QmdbReadsMode::Only)?;
    let before = rpc.state_read_status(None).await?;
    let state = provider.state_by_block_hash(hash)?;
    let account = state.basic_account(&address)?.unwrap();
    assert_eq!(account.nonce, 3);
    assert_eq!(account.balance, U256::from(1234));
    assert_eq!(state.storage(address, slot)?, Some(U256::from(42)));
    let after = rpc.state_read_status(Some(hash)).await?;
    assert_eq!(after.reads.instance_id, before.reads.instance_id);
    assert_eq!(after.reads.mode, QmdbReadsMode::Only);
    assert!(after.reads.wal_enabled);
    assert_eq!(after.reads.chain_id, Some(1));
    assert_eq!(after.reads.genesis_hash, Some(hash));
    assert_eq!(after.reads.durable_root, Some(B256::from(tree.root())));
    assert_eq!(
        after.validator_public_key.as_deref(),
        Some(expected_key.as_str())
    );
    assert_eq!(
        after.reads.counters.account_reads - before.reads.counters.account_reads,
        1
    );
    assert_eq!(
        after.reads.counters.storage_reads - before.reads.counters.storage_reads,
        1
    );
    let request = serde_json::json!({"jsonrpc":"2.0", "id":1,
        "method":"n42_stateReadStatus", "params":[hash]})
    .to_string();
    let (response, _) = rpc.into_rpc().raw_json_request(&request, 1).await?;
    let response: serde_json::Value = serde_json::from_str(response.get())?;
    assert_eq!(response["result"]["mode"], "only");
    assert_eq!(response["result"]["backend"], "gov5_qmdb_binary");
    assert_eq!(response["result"]["validatorPublicKey"], expected_key);
    assert!(
        provider
            .state_by_block_hash(B256::repeat_byte(0xff))
            .is_err()
    );
    assert!(register(store, QmdbReadsMode::Only).is_err());
    Ok(())
}
