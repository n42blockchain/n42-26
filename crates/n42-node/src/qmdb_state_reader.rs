//! Exact-block QMDB account/storage reads in reth's execution provider path.

#[cfg(test)]
#[path = "qmdb_canonical_bench.rs"]
mod canonical_bench;

use crate::qmdb_read_status::{QmdbReadCounters, QmdbReadStatus, ReadEvent, process_instance_id};
use crate::{qmdb_read_view::QmdbReadView, qmdb_state_root::Gov5QmdbStateRootStore};
use alloy_primitives::{Address, B256, U256};
use reth_primitives_traits::Account;
use reth_provider::{ProviderError, ProviderResult};
use reth_storage_api::{
    AccountReader, BlockHashReader, BytecodeReader, HashedPostStateProvider, StateProofProvider,
    StateProvider, StateProviderBox, StateRootProvider, StorageRootProvider,
    n42_state::BlockStateAdapter,
};
use std::sync::{Arc, OnceLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QmdbReadsMode {
    Off,
    /// Compare QMDB and the exact same reth provider. Mismatches/errors fail.
    Verify,
    /// Answer available block versions from QMDB; count unavailable fallbacks.
    On,
    /// Refuse a state provider when its exact QMDB version is unavailable.
    Only,
}

impl std::str::FromStr for QmdbReadsMode {
    type Err = &'static str;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "off" => Ok(Self::Off),
            "verify" => Ok(Self::Verify),
            "on" => Ok(Self::On),
            "only" => Ok(Self::Only),
            _ => Err("N42_QMDB_READS must be off, verify, on, or only"),
        }
    }
}

/// Factory scoped to one node's authenticated forest. Call `wrap` once per
/// provider, never per key; the returned view is immutable for its lifetime.
pub struct QmdbStateAdapter {
    store: Arc<Gov5QmdbStateRootStore>,
    mode: QmdbReadsMode,
    counters: Arc<QmdbReadCounters>,
}

impl QmdbStateAdapter {
    pub fn new(store: Arc<Gov5QmdbStateRootStore>, mode: QmdbReadsMode) -> ProviderResult<Self> {
        if mode != QmdbReadsMode::Off {
            store
                .enable_read_views(std::num::NonZeroUsize::new(64).unwrap())
                .map_err(ProviderError::other)?;
        }
        Ok(Self {
            store,
            mode,
            counters: Arc::new(QmdbReadCounters::default()),
        })
    }
}

impl BlockStateAdapter for QmdbStateAdapter {
    fn wrap(&self, hash: B256, inner: StateProviderBox) -> ProviderResult<StateProviderBox> {
        if self.mode == QmdbReadsMode::Off {
            return Ok(inner);
        }
        let view = self.store.prepare_read_view(hash).map_err(|error| {
            self.counters.increment(ReadEvent::ProviderError);
            ProviderError::other(error)
        })?;
        let Some(view) = view else {
            self.counters.increment(ReadEvent::UnavailableProvider);
            metrics::counter!("n42_qmdb_read_providers_total", "outcome" => "unavailable")
                .increment(1);
            if self.mode == QmdbReadsMode::Only {
                self.counters.increment(ReadEvent::ProviderError);
                return Err(ProviderError::other(std::io::Error::other(format!(
                    "QMDB state unavailable at {hash}"
                ))));
            }
            return Ok(inner);
        };
        self.counters.increment(ReadEvent::PinnedProvider);
        metrics::counter!("n42_qmdb_read_providers_total", "outcome" => "pinned").increment(1);
        Ok(Box::new(QmdbStateProvider {
            view,
            inner,
            mode: self.mode,
            counters: self.counters.clone(),
        }))
    }
}

static REGISTERED: OnceLock<Arc<QmdbStateAdapter>> = OnceLock::new();

/// Install the adapter before the node's services start. The process registry
/// rejects duplicate registrations rather than silently serving another chain.
pub fn register(store: Arc<Gov5QmdbStateRootStore>, mode: QmdbReadsMode) -> ProviderResult<()> {
    if mode == QmdbReadsMode::Off {
        return Ok(());
    }
    let adapter = Arc::new(QmdbStateAdapter::new(store, mode)?);
    if !reth_storage_api::n42_state::register(adapter.clone()) {
        return Err(ProviderError::other(std::io::Error::other(
            "QMDB state adapter already registered",
        )));
    }
    REGISTERED.set(adapter).map_err(|_| {
        ProviderError::other(std::io::Error::other("QMDB read status already registered"))
    })?;
    Ok(())
}

/// Report the actual registered adapter, never an environment-variable guess.
/// `root_for` excludes a block whose WAL append is still in flight.
pub fn status(requested_block_hash: Option<B256>) -> ProviderResult<QmdbReadStatus> {
    let adapter = REGISTERED.get();
    let identity = adapter.map(|adapter| adapter.store.base_identity());
    let durable_root = match (adapter, requested_block_hash) {
        (Some(adapter), Some(hash)) => {
            adapter.store.root_for(hash).map_err(ProviderError::other)?
        }
        _ => None,
    };
    Ok(QmdbReadStatus {
        schema: 1,
        instance_id: process_instance_id(),
        process_id: std::process::id(),
        mode: adapter.map_or(QmdbReadsMode::Off, |adapter| adapter.mode),
        backend: if adapter.is_some() {
            "gov5_qmdb_binary"
        } else {
            "disabled"
        }
        .to_owned(),
        coverage: "exact_block_provider".to_owned(),
        wal_enabled: adapter.is_some_and(|adapter| adapter.store.wal_enabled()),
        chain_id: identity.map(|identity| identity.chain_id),
        genesis_hash: identity.map(|identity| identity.genesis_hash),
        base_block_hash: adapter.map(|adapter| adapter.store.base_block_hash()),
        base_root: adapter.map(|adapter| adapter.store.base_root()),
        requested_block_hash,
        durable_root,
        counters: adapter
            .map(|adapter| adapter.counters.snapshot())
            .unwrap_or_default(),
    })
}

struct QmdbStateProvider {
    view: Arc<QmdbReadView>,
    inner: StateProviderBox,
    mode: QmdbReadsMode,
    counters: Arc<QmdbReadCounters>,
}

impl QmdbStateProvider {
    fn as_ref(&self) -> &dyn StateProvider {
        self.inner.as_ref()
    }

    fn check<T: PartialEq>(&self, kind: &'static str, qmdb: T, database: T) -> ProviderResult<T> {
        self.counters.increment(if kind == "account" {
            ReadEvent::AccountComparison
        } else {
            ReadEvent::StorageComparison
        });
        let outcome = if qmdb == database {
            "matched"
        } else {
            "mismatch"
        };
        metrics::counter!("n42_qmdb_read_comparisons_total", "kind" => kind, "outcome" => outcome)
            .increment(1);
        if qmdb != database {
            self.counters.increment(ReadEvent::Mismatch);
            return Err(ProviderError::other(std::io::Error::other(format!(
                "QMDB {kind} differs from execution state at {}",
                self.view.block_hash()
            ))));
        }
        Ok(database)
    }
}

impl AccountReader for QmdbStateProvider {
    fn basic_account(&self, address: &Address) -> ProviderResult<Option<Account>> {
        let got = self.view.account(address).map_err(|error| {
            self.counters.increment(ReadEvent::ReadError);
            metrics::counter!("n42_qmdb_read_errors_total", "kind" => "account").increment(1);
            ProviderError::other(error)
        })?;
        self.counters.increment(ReadEvent::Account);
        metrics::counter!("n42_qmdb_state_reads_total", "kind" => "account").increment(1);
        if self.mode == QmdbReadsMode::Verify {
            let database = self.inner.basic_account(address).inspect_err(|_| {
                self.counters.increment(ReadEvent::ReadError);
                metrics::counter!("n42_qmdb_read_errors_total", "kind" => "database_account")
                    .increment(1);
            })?;
            // Gov5 omits the empty-code hash, while reth can store it
            // explicitly. Compare the same EVM value without hiding any
            // nonce, balance or nonempty-code difference.
            let comparable = database.map(|mut account| {
                if account.bytecode_hash == Some(alloy_primitives::KECCAK256_EMPTY) {
                    account.bytecode_hash = None;
                }
                account
            });
            self.check("account", got, comparable)?;
            Ok(database)
        } else {
            Ok(got)
        }
    }
}

impl StateProvider for QmdbStateProvider {
    fn storage(&self, address: Address, slot: B256) -> ProviderResult<Option<U256>> {
        let got = self.view.storage(&address, &slot).map_err(|error| {
            self.counters.increment(ReadEvent::ReadError);
            metrics::counter!("n42_qmdb_read_errors_total", "kind" => "storage").increment(1);
            ProviderError::other(error)
        })?;
        self.counters.increment(ReadEvent::Storage);
        metrics::counter!("n42_qmdb_state_reads_total", "kind" => "storage").increment(1);
        if self.mode == QmdbReadsMode::Verify {
            let value = self.check(
                "storage",
                got.unwrap_or_default(),
                self.inner.storage(address, slot).inspect_err(|_| {
                    self.counters.increment(ReadEvent::ReadError);
                    metrics::counter!("n42_qmdb_read_errors_total", "kind" => "database_storage").increment(1);
                })?.unwrap_or_default(),
            )?;
            Ok((!value.is_zero()).then_some(value))
        } else {
            Ok(got)
        }
    }
}

// QMDB replaces point reads only. Bytecode, block hashes, historical metadata
// and the existing compatibility proof/root interfaces keep their provider.
reth_storage_api::macros::delegate_impls_to_as_ref!(
    for QmdbStateProvider =>
    BlockHashReader {
        fn block_hash(&self, number: u64) -> ProviderResult<Option<B256>>;
        fn canonical_hashes_range(&self, start: u64, end: u64) -> ProviderResult<Vec<B256>>;
    }
    BytecodeReader {
        fn bytecode_by_hash(&self, hash: &B256) -> ProviderResult<Option<reth_primitives_traits::Bytecode>>;
    }
    HashedPostStateProvider {
        fn hashed_post_state(&self, state: &revm::database::BundleState) -> ProviderResult<reth_trie_common::HashedPostState>;
    }
    StateRootProvider {
        fn state_root(&self, state: reth_trie_common::HashedPostState) -> ProviderResult<B256>;
        fn state_root_from_nodes(&self, input: reth_trie_common::TrieInput) -> ProviderResult<B256>;
        fn state_root_with_updates(&self, state: reth_trie_common::HashedPostState) -> ProviderResult<(B256, reth_trie_common::updates::TrieUpdates)>;
        fn state_root_from_nodes_with_updates(&self, input: reth_trie_common::TrieInput) -> ProviderResult<(B256, reth_trie_common::updates::TrieUpdates)>;
    }
    StorageRootProvider {
        fn storage_root(&self, address: Address, state: reth_trie_common::HashedStorage) -> ProviderResult<B256>;
        fn storage_proof(&self, address: Address, slot: B256, state: reth_trie_common::HashedStorage) -> ProviderResult<reth_trie_common::StorageProof>;
        fn storage_multiproof(&self, address: Address, slots: &[B256], state: reth_trie_common::HashedStorage) -> ProviderResult<reth_trie_common::StorageMultiProof>;
    }
    StateProofProvider {
        fn proof(&self, input: reth_trie_common::TrieInput, address: Address, slots: &[B256]) -> ProviderResult<reth_trie_common::AccountProof>;
        fn multiproof(&self, input: reth_trie_common::TrieInput, targets: reth_trie_common::MultiProofTargets) -> ProviderResult<reth_trie_common::MultiProof>;
        fn multiproof_v2(&self, input: reth_trie_common::TrieInput, targets: reth_trie_common::MultiProofTargetsV2) -> ProviderResult<reth_trie_common::DecodedMultiProofV2>;
        fn witness(&self, input: reth_trie_common::TrieInput, target: reth_trie_common::HashedPostState, mode: reth_trie_common::ExecutionWitnessMode) -> ProviderResult<Vec<alloy_primitives::Bytes>>;
    }
);

#[cfg(test)]
mod tests {
    use super::*;
    use n42_twig_core::qmdb_compat::{
        QmdbCompatTree, encode_gov5_account_value, gov5_account_key, gov5_storage_key,
    };
    use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};
    use reth_storage_api::StateProviderFactory;

    fn fixture() -> (
        Arc<Gov5QmdbStateRootStore>,
        MockEthProvider,
        Address,
        Address,
    ) {
        let sender = Address::repeat_byte(0x11);
        let contract = Address::repeat_byte(0x22);
        // SLOAD(0), store in memory, return one word.
        let code = alloy_primitives::bytes!("60005460005260206000f3");
        let mut tree = QmdbCompatTree::new();
        tree.set(
            gov5_account_key(sender.as_ref()),
            encode_gov5_account_value(0, &U256::from(1_000_000_000).to_be_bytes(), &B256::ZERO.0),
        );
        tree.set(
            gov5_account_key(contract.as_ref()),
            encode_gov5_account_value(
                1,
                &U256::ZERO.to_be_bytes(),
                &alloy_primitives::keccak256(&code).0,
            ),
        );
        tree.set(
            gov5_storage_key(contract.as_ref(), &B256::ZERO.0),
            U256::from(42).to_be_bytes::<32>().to_vec(),
        );
        let store = Arc::new(
            Gov5QmdbStateRootStore::new(
                B256::repeat_byte(0x77),
                B256::from(tree.root()),
                tree.snapshot(),
            )
            .unwrap(),
        );
        let provider = MockEthProvider::default();
        provider.add_account(sender, ExtendedAccount::new(0, U256::from(1_000_000_000)));
        provider.add_account(
            contract,
            ExtendedAccount::new(1, U256::ZERO)
                .with_bytecode(code)
                .extend_storage([(B256::ZERO, U256::from(42))]),
        );
        (store, provider, sender, contract)
    }

    #[test]
    fn verify_compares_reads_and_rejects_divergent_state() {
        let (store, provider, sender, contract) = fixture();
        let adapter = QmdbStateAdapter::new(store.clone(), QmdbReadsMode::Verify).unwrap();
        let state = adapter
            .wrap(store.base_block_hash(), provider.latest().unwrap())
            .unwrap();
        assert_eq!(
            state.basic_account(&sender).unwrap().unwrap().balance,
            U256::from(1_000_000_000)
        );
        assert_eq!(
            state.storage(contract, B256::ZERO).unwrap(),
            Some(U256::from(42))
        );
        assert_eq!(
            state.basic_account(&Address::repeat_byte(0x33)).unwrap(),
            None
        );
        assert_eq!(state.storage(contract, B256::repeat_byte(1)).unwrap(), None);
        provider.add_account(
            sender,
            ExtendedAccount::new(0, U256::from(1_000_000_000))
                .with_bytecode(alloy_primitives::Bytes::new()),
        );
        assert_eq!(
            state.basic_account(&sender).unwrap().unwrap().bytecode_hash,
            Some(alloy_primitives::KECCAK256_EMPTY)
        );
        provider.add_account(sender, ExtendedAccount::new(3, U256::from(1)));
        assert!(state.basic_account(&sender).is_err());
        provider.add_account(
            contract,
            ExtendedAccount::new(1, U256::ZERO).extend_storage([(B256::ZERO, U256::from(7))]),
        );
        assert!(state.storage(contract, B256::ZERO).is_err());
        let counts = adapter.counters.snapshot();
        assert_eq!(counts.account_reads, 4);
        assert_eq!(counts.storage_reads, 3);
        assert_eq!(counts.account_comparisons, 4);
        assert_eq!(counts.storage_comparisons, 3);
        assert_eq!(counts.mismatches, 2);
        assert_eq!(counts.read_errors, 0);
    }

    #[test]
    fn only_mode_runs_evm_using_qmdb_accounts_and_slots() {
        use revm::{
            Context, MainBuilder,
            context::{BlockEnv, CfgEnv, TxEnv},
            handler::ExecuteEvm,
            primitives::hardfork::SpecId,
        };
        let (store, provider, sender, contract) = fixture();
        let adapter = QmdbStateAdapter::new(store.clone(), QmdbReadsMode::Only).unwrap();
        // The fallback has the contract's bytecode but wrong balance/nonce/slot.
        // Successful execution returning 42 must have read QMDB, not fallback.
        provider.add_account(sender, ExtendedAccount::new(9, U256::ZERO));
        provider.add_account(
            contract,
            ExtendedAccount::new(1, U256::ZERO)
                .with_bytecode(alloy_primitives::bytes!("60005460005260206000f3"))
                .extend_storage([(B256::ZERO, U256::from(7))]),
        );
        let state = adapter
            .wrap(store.base_block_hash(), provider.latest().unwrap())
            .unwrap();
        let database =
            reth_revm::database::StateProviderDatabase::new(state.into_evm_state_provider());
        let mut context: Context<BlockEnv, TxEnv, CfgEnv, _, revm::Journal<_>, ()> =
            Context::new(database, SpecId::CANCUN);
        context.block.basefee = 0;
        let mut evm = context.build_mainnet();
        let tx = TxEnv::builder()
            .caller(sender)
            .to(contract)
            .gas_limit(100_000)
            .gas_price(1)
            .nonce(0)
            .build()
            .unwrap();
        let output = evm.transact(tx).unwrap();
        assert!(output.result.is_success());
        assert_eq!(
            output.result.output().unwrap().as_ref(),
            &U256::from(42).to_be_bytes::<32>()
        );
        assert_eq!(output.state[&sender].info.nonce, 1);
        let counts = adapter.counters.snapshot();
        assert!(counts.account_reads > 0 && counts.storage_reads > 0);
        assert_eq!(counts.account_comparisons + counts.storage_comparisons, 0);
        assert!(
            adapter
                .wrap(B256::repeat_byte(0xff), provider.latest().unwrap())
                .is_err()
        );
        assert_eq!(adapter.counters.snapshot().unavailable_providers, 1);
        assert_eq!(adapter.counters.snapshot().provider_errors, 1);
    }

    #[test]
    fn malformed_qmdb_slot_never_becomes_zero_or_falls_back() {
        let (store, provider, _, contract) = fixture();
        let key = gov5_storage_key(contract.as_ref(), &B256::ZERO.0);
        let ops = vec![n42_twig_core::qmdb_compat::QmdbOperation {
            key,
            value: Some(vec![1]),
        }];
        let hash = B256::repeat_byte(0x78);
        let root = store
            .compute_candidate(store.base_block_hash(), &ops)
            .unwrap();
        store
            .compute_and_commit(store.base_block_hash(), hash, root, ops)
            .unwrap();
        for mode in [
            QmdbReadsMode::Verify,
            QmdbReadsMode::On,
            QmdbReadsMode::Only,
        ] {
            let adapter = QmdbStateAdapter::new(store.clone(), mode).unwrap();
            let state = adapter.wrap(hash, provider.latest().unwrap()).unwrap();
            assert!(state.storage(contract, B256::ZERO).is_err());
            assert_eq!(adapter.counters.snapshot().read_errors, 1);
            assert_eq!(adapter.counters.snapshot().storage_reads, 0);
        }
    }
}
