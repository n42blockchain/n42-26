//! Builds a leader's next block on its own block as soon as that block is
//! sealed (`N42_BUILD_ON_OUTPUT=1`), instead of after the engine has it.
//!
//! The own block's insert publishes the block's normalized header and the
//! execution output its builder produced, before the QMDB commit and the
//! engine insert run. A child built on it reads the block's state from that
//! output, laid over the state of the block's parent; reads of anything the
//! block did not touch go to the parent's provider as before. The child is
//! the block a build on the engine's state would give: the same parent
//! header, the same pre-state, the same transactions.
//!
//! The child's QMDB candidate is prepared on its parent's committed state,
//! so its root job waits for the parent's commit. The store holds a single
//! prepared candidate; preparing the child's first would discard the
//! parent's before the parent's insert commits it.

use alloy_primitives::{Address, B256, U256};
use alloy_rpc_types_engine::PayloadAttributes;
use n42_tx_types::Receipt;
use reth_execution_types::BlockExecutionOutput;
use reth_primitives_traits::{Account, Bytecode, SealedHeader};
use reth_storage_api::{
    AccountReader, BlockHashReader, BytecodeReader, HashedPostStateProvider, StateProofProvider,
    StateProvider, StateProviderBox, StateProviderFactory, StateRootProvider, StorageRootProvider,
};
use reth_provider::{ProviderError, ProviderResult};
use reth_trie_common::{
    AccountProof, DecodedMultiProofV2, ExecutionWitnessMode, HashedPostState, HashedStorage,
    MultiProof, MultiProofTargets, MultiProofTargetsV2, StorageMultiProof, StorageProof,
    TrieInput, bundle_state::BundleState, updates::TrieUpdates,
};
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Whether a leader builds on its own sealed block's output.
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("N42_BUILD_ON_OUTPUT").is_ok_and(|v| v == "1"))
}

/// How long a build waits for its parent's output to be published.
pub const SEALED_WAIT: Duration = Duration::from_millis(500);

/// How long a child's QMDB root job waits for its parent's commit.
const COMMIT_WAIT: Duration = Duration::from_secs(2);

/// Own blocks kept; a child reads at most a short chain of them.
const KEPT: usize = 4;

/// An own block, sealed under its normalized header, with its execution output.
#[derive(Debug)]
pub struct SealedOutput {
    pub header: SealedHeader,
    pub output: Arc<BlockExecutionOutput<Receipt>>,
}

#[derive(Default)]
struct Registry {
    /// Published blocks, oldest first. `None`: the block's insert refused it.
    blocks: VecDeque<(B256, Option<Arc<SealedOutput>>)>,
}

fn registry() -> &'static (Mutex<Registry>, Condvar) {
    static REGISTRY: OnceLock<(Mutex<Registry>, Condvar)> = OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

fn record(hash: B256, entry: Option<Arc<SealedOutput>>) {
    let (lock, published) = registry();
    let mut registry = lock.lock().unwrap_or_else(|error| error.into_inner());
    registry.blocks.retain(|(held, _)| *held != hash);
    registry.blocks.push_back((hash, entry));
    while registry.blocks.len() > KEPT {
        registry.blocks.pop_front();
    }
    published.notify_all();
}

/// Publishes an own block for a child built on it. Its header must hash to
/// the block's hash, and `output` must be the output of exactly its
/// transactions on its parent.
pub fn publish(header: SealedHeader, output: Arc<BlockExecutionOutput<Receipt>>) {
    if !enabled() {
        return;
    }
    record(header.hash(), Some(Arc::new(SealedOutput { header, output })));
}

/// Records that the own block `hash` will not be published, so a build
/// waiting for it gives up at once.
pub fn refuse(hash: B256) {
    if !enabled() || sealed(hash).is_some() {
        return;
    }
    record(hash, None);
}

/// Drops `hash`, so nothing is built on it from its output any more.
pub fn forget(hash: B256) {
    let (lock, _) = registry();
    let mut registry = lock.lock().unwrap_or_else(|error| error.into_inner());
    registry.blocks.retain(|(held, _)| *held != hash);
}

/// The published own block `hash`.
pub fn sealed(hash: B256) -> Option<Arc<SealedOutput>> {
    let (lock, _) = registry();
    let registry = lock.lock().unwrap_or_else(|error| error.into_inner());
    registry
        .blocks
        .iter()
        .find(|(held, _)| *held == hash)
        .and_then(|(_, entry)| entry.clone())
}

/// The own block `hash` once it is published, if it is within `timeout`
/// and its insert did not refuse it.
pub fn wait_sealed(hash: B256, timeout: Duration) -> Option<Arc<SealedOutput>> {
    let (lock, published) = registry();
    let started = Instant::now();
    let mut registry = lock.lock().unwrap_or_else(|error| error.into_inner());
    loop {
        if let Some((_, entry)) = registry.blocks.iter().find(|(held, _)| *held == hash) {
            return entry.clone();
        }
        let left = timeout.checked_sub(started.elapsed())?;
        registry = published
            .wait_timeout(registry, left)
            .unwrap_or_else(|error| error.into_inner())
            .0;
    }
}

/// The state after `parent`: from its published output when it is an own
/// block, otherwise from `client`.
pub fn parent_state<C>(client: &C, parent: B256) -> ProviderResult<StateProviderBox>
where
    C: StateProviderFactory + ?Sized,
{
    let open = |hash| client.state_by_block_hash(hash);
    match enabled().then(|| sealed(parent)).flatten() {
        Some(sealed) => overlay(&open, sealed),
        None => open(parent),
    }
}

/// `sealed`'s output over its parent's state, opened by `open`. A parent
/// `open` does not have yet is read from its own published output in turn.
fn overlay(
    open: &dyn Fn(B256) -> ProviderResult<StateProviderBox>,
    sealed: Arc<SealedOutput>,
) -> ProviderResult<StateProviderBox> {
    let grandparent = sealed.header.parent_hash;
    let inner = match open(grandparent) {
        Ok(state) => state,
        Err(error) => match self::sealed(grandparent) {
            Some(grandparent) => {
                metrics::counter!("n42_build_on_output_chained_parents_total").increment(1);
                overlay(open, grandparent)?
            }
            None => return Err(error),
        },
    };
    Ok(Box::new(SealedParentState { inner, sealed }))
}

/// Waits until a candidate can be prepared on `parent`, when `parent` is an
/// own block whose commit may still be under way. Returns whether it can.
pub fn wait_parent_committed(
    store: &crate::qmdb_state_root::Gov5QmdbStateRootStore,
    parent: B256,
) -> bool {
    let committed = || store.root_for(parent).is_ok_and(|root| root.is_some());
    if !enabled() || sealed(parent).is_none() || committed() {
        return true;
    }
    let started = Instant::now();
    let mut done = false;
    while started.elapsed() < COMMIT_WAIT {
        std::thread::sleep(Duration::from_micros(200));
        if committed() {
            done = true;
            break;
        }
    }
    metrics::histogram!("n42_build_on_output_wait_ms", "stage" => "qmdb_parent")
        .record(started.elapsed().as_secs_f64() * 1000.0);
    done
}

/// Builds a block on a published own block. Set once by the payload builder.
pub type DirectBuild = Box<
    dyn Fn(
            SealedHeader,
            PayloadAttributes,
        ) -> Result<crate::engine_types::N42BuiltPayload, String>
        + Send
        + Sync,
>;

static BUILDER: OnceLock<DirectBuild> = OnceLock::new();

/// Installs the builder a build on a sealed block runs.
pub fn register_builder(build: DirectBuild) {
    let _ = BUILDER.set(build);
}

/// Builds a child of the own block `parent` with `attributes`, once the
/// block is published.
pub fn build(
    parent: B256,
    attributes: PayloadAttributes,
    wait: Duration,
) -> Result<crate::engine_types::N42BuiltPayload, String> {
    let builder = BUILDER.get().ok_or("no builder for a build on a sealed block")?;
    let started = Instant::now();
    let sealed = wait_sealed(parent, wait);
    let waited_ms = started.elapsed().as_secs_f64() * 1000.0;
    metrics::histogram!("n42_build_on_output_wait_ms", "stage" => "sealed_parent").record(waited_ms);
    tracing::debug!(target: "n42::payload", %parent, waited_ms, "N42_BUILD_ON_OUTPUT: parent published");
    let sealed = sealed.ok_or_else(|| format!("own block {parent} was not published"))?;
    check_attributes(&sealed.header, &attributes)?;
    builder(sealed.header.clone(), attributes)
}

/// What the engine checks of attributes on a forkchoice update, for the
/// parts that depend on the parent.
fn check_attributes(parent: &SealedHeader, attributes: &PayloadAttributes) -> Result<(), String> {
    if attributes.timestamp <= parent.timestamp {
        return Err(format!(
            "timestamp {} is not after the parent's {}",
            attributes.timestamp, parent.timestamp
        ));
    }
    Ok(())
}

/// A sealed block's state: its output over its parent's state.
struct SealedParentState {
    inner: StateProviderBox,
    sealed: Arc<SealedOutput>,
}

impl SealedParentState {
    fn bundle(&self) -> &BundleState {
        &self.sealed.output.state
    }
}

fn unavailable() -> ProviderError {
    ProviderError::other(std::io::Error::other(
        "state commitments are not available on a sealed block's output",
    ))
}

impl AccountReader for SealedParentState {
    fn basic_account(&self, address: &Address) -> ProviderResult<Option<Account>> {
        match self.bundle().account(address) {
            Some(account) => Ok(account.info.as_ref().map(Into::into)),
            None => self.inner.basic_account(address),
        }
    }
}

impl StateProvider for SealedParentState {
    fn storage(&self, address: Address, slot: B256) -> ProviderResult<Option<U256>> {
        match self.bundle().storage(&address, U256::from_be_bytes(slot.0)) {
            Some(value) => Ok((!value.is_zero()).then_some(value)),
            None => self.inner.storage(address, slot),
        }
    }
}

impl BytecodeReader for SealedParentState {
    fn bytecode_by_hash(&self, hash: &B256) -> ProviderResult<Option<Bytecode>> {
        match self.bundle().bytecode(hash) {
            Some(code) => Ok(Some(Bytecode(code))),
            None => self.inner.bytecode_by_hash(hash),
        }
    }
}

impl BlockHashReader for SealedParentState {
    fn block_hash(&self, number: u64) -> ProviderResult<Option<B256>> {
        if number == self.sealed.header.number {
            return Ok(Some(self.sealed.header.hash()));
        }
        self.inner.block_hash(number)
    }

    fn canonical_hashes_range(&self, start: u64, end: u64) -> ProviderResult<Vec<B256>> {
        let number = self.sealed.header.number;
        let mut hashes = if start < end.min(number) {
            self.inner.canonical_hashes_range(start, end.min(number))?
        } else {
            Vec::new()
        };
        if (start..end).contains(&number) {
            hashes.push(self.sealed.header.hash());
        }
        Ok(hashes)
    }
}

impl HashedPostStateProvider for SealedParentState {
    fn hashed_post_state(&self, state: &BundleState) -> ProviderResult<HashedPostState> {
        self.inner.hashed_post_state(state)
    }
}

// A block is built on a sealed block only where its state root is replaced
// by the QMDB root; no commitment is ever asked of these providers.
impl StateRootProvider for SealedParentState {
    fn state_root(&self, _state: HashedPostState) -> ProviderResult<B256> {
        Err(unavailable())
    }

    fn state_root_from_nodes(&self, _input: TrieInput) -> ProviderResult<B256> {
        Err(unavailable())
    }

    fn state_root_with_updates(
        &self,
        _state: HashedPostState,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        Err(unavailable())
    }

    fn state_root_from_nodes_with_updates(
        &self,
        _input: TrieInput,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        Err(unavailable())
    }
}

impl StorageRootProvider for SealedParentState {
    fn storage_root(&self, _address: Address, _storage: HashedStorage) -> ProviderResult<B256> {
        Err(unavailable())
    }

    fn storage_proof(
        &self,
        _address: Address,
        _slot: B256,
        _storage: HashedStorage,
    ) -> ProviderResult<StorageProof> {
        Err(unavailable())
    }

    fn storage_multiproof(
        &self,
        _address: Address,
        _slots: &[B256],
        _storage: HashedStorage,
    ) -> ProviderResult<StorageMultiProof> {
        Err(unavailable())
    }
}

impl StateProofProvider for SealedParentState {
    fn proof(
        &self,
        _input: TrieInput,
        _address: Address,
        _slots: &[B256],
    ) -> ProviderResult<AccountProof> {
        Err(unavailable())
    }

    fn multiproof(
        &self,
        _input: TrieInput,
        _targets: MultiProofTargets,
    ) -> ProviderResult<MultiProof> {
        Err(unavailable())
    }

    fn multiproof_v2(
        &self,
        _targets: MultiProofTargetsV2,
        _requires_masks: bool,
    ) -> ProviderResult<DecodedMultiProofV2> {
        Err(unavailable())
    }

    fn witness(
        &self,
        _input: TrieInput,
        _target: HashedPostState,
        _mode: ExecutionWitnessMode,
    ) -> ProviderResult<Vec<alloy_primitives::Bytes>> {
        Err(unavailable())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::Header;
    use evm2::evm::AccountInfo;
    use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};

    /// Opens the state of `known` only, as a provider that has no later block.
    fn opener(
        provider: MockEthProvider,
        known: B256,
    ) -> impl Fn(B256) -> ProviderResult<StateProviderBox> {
        move |hash| {
            if hash == known {
                provider.state_by_block_hash(hash)
            } else {
                Err(ProviderError::StateForHashNotFound(hash))
            }
        }
    }

    fn sealed_on(parent: B256, number: u64, bundle: BundleState) -> Arc<SealedOutput> {
        let header = SealedHeader::seal_slow(Header {
            parent_hash: parent,
            number,
            timestamp: 10,
            ..Default::default()
        });
        Arc::new(SealedOutput {
            header,
            output: Arc::new(BlockExecutionOutput {
                state: bundle,
                result: Default::default(),
            }),
        })
    }

    fn info(nonce: u64, balance: u64) -> AccountInfo {
        AccountInfo {
            nonce,
            balance: U256::from(balance),
            ..Default::default()
        }
    }

    /// The parent's provider, and an own block on it that changes account 1
    /// and slot 7 of account 2, creates account 3 and destroys account 4.
    fn fixture() -> (MockEthProvider, Arc<SealedOutput>) {
        let provider = MockEthProvider::default();
        let parent = Header {
            number: 4,
            ..Default::default()
        };
        let parent_hash = parent.hash_slow();
        provider.add_header(parent_hash, parent);
        provider.add_account(
            Address::with_last_byte(1),
            ExtendedAccount::new(1, U256::from(100)),
        );
        provider.add_account(
            Address::with_last_byte(2),
            ExtendedAccount::new(0, U256::from(5)).extend_storage([
                (B256::with_last_byte(7), U256::from(70)),
                (B256::with_last_byte(8), U256::from(80)),
            ]),
        );
        provider.add_account(
            Address::with_last_byte(4),
            ExtendedAccount::new(9, U256::from(9))
                .extend_storage([(B256::with_last_byte(1), U256::from(11))]),
        );
        provider.add_account(
            Address::with_last_byte(5),
            ExtendedAccount::new(3, U256::from(33)),
        );
        let mut bundle = BundleState::builder(5..=5)
            .state_present_account_info(Address::with_last_byte(1), info(2, 90))
            .state_present_account_info(Address::with_last_byte(2), info(0, 5))
            .state_storage(
                Address::with_last_byte(2),
                [(U256::from(7), (U256::from(70), U256::from(71)))]
                    .into_iter()
                    .collect(),
            )
            .state_present_account_info(Address::with_last_byte(3), info(0, 10))
            .state_address(Address::with_last_byte(4))
            .build();
        bundle
            .state
            .get_mut(&Address::with_last_byte(4))
            .expect("destroyed account")
            .status = evm2::block::bundle_state::AccountStatus::Destroyed;
        (provider, sealed_on(parent_hash, 5, bundle))
    }

    #[test]
    fn the_sealed_output_is_laid_over_the_parent_state() {
        let (provider, sealed) = fixture();
        let state = overlay(&opener(provider, sealed.header.parent_hash), sealed.clone()).unwrap();
        let account = |n: u8| state.basic_account(&Address::with_last_byte(n)).unwrap();
        // Changed by the block.
        assert_eq!(account(1).map(|a| (a.nonce, a.balance)), Some((2, U256::from(90))));
        // Created by the block.
        assert_eq!(account(3).map(|a| a.balance), Some(U256::from(10)));
        // Destroyed by the block.
        assert_eq!(account(4), None);
        // Untouched by the block: the parent's.
        assert_eq!(account(5).map(|a| a.nonce), Some(3));
        let slot = |n: u8, s: u8| {
            state
                .storage(Address::with_last_byte(n), B256::with_last_byte(s))
                .unwrap()
        };
        assert_eq!(slot(2, 7), Some(U256::from(71)), "slot written by the block");
        assert_eq!(slot(2, 8), Some(U256::from(80)), "slot the block did not write");
        assert_eq!(slot(4, 1), None, "slot of a destroyed account");
        assert_eq!(
            state.block_hash(5).unwrap(),
            Some(sealed.header.hash()),
            "BLOCKHASH of the sealed block"
        );
        assert_eq!(
            state.canonical_hashes_range(5, 6).unwrap(),
            vec![sealed.header.hash()]
        );
        assert!(state.state_root(HashedPostState::default()).is_err());
    }

    #[test]
    fn a_parent_the_provider_lacks_is_read_from_its_own_output() {
        let (provider, first) = fixture();
        let second = sealed_on(
            first.header.hash(),
            6,
            BundleState::builder(6..=6)
                .state_present_account_info(Address::with_last_byte(5), info(4, 30))
                .build(),
        );
        let open = opener(provider, first.header.parent_hash);
        // Without the first block published there is no state to read.
        assert!(overlay(&open, second.clone()).is_err());
        record(first.header.hash(), Some(first.clone()));
        let state = overlay(&open, second.clone()).unwrap();
        assert_eq!(
            state
                .basic_account(&Address::with_last_byte(1))
                .unwrap()
                .map(|a| a.nonce),
            Some(2),
            "the first block's change"
        );
        assert_eq!(
            state
                .basic_account(&Address::with_last_byte(5))
                .unwrap()
                .map(|a| a.nonce),
            Some(4),
            "the second block's change"
        );
        assert_eq!(state.block_hash(5).unwrap(), Some(first.header.hash()));
        assert_eq!(state.block_hash(6).unwrap(), Some(second.header.hash()));
        assert_eq!(
            state.canonical_hashes_range(5, 7).unwrap(),
            vec![first.header.hash(), second.header.hash()]
        );
        forget(first.header.hash());
        assert!(sealed(first.header.hash()).is_none());
    }

    #[test]
    fn a_refused_block_ends_the_wait_and_an_unknown_one_times_out() {
        let refused = B256::repeat_byte(0xE1);
        record(refused, None);
        let started = Instant::now();
        assert!(wait_sealed(refused, Duration::from_secs(5)).is_none());
        assert!(started.elapsed() < Duration::from_secs(1));
        let started = Instant::now();
        assert!(wait_sealed(B256::repeat_byte(0xE2), Duration::from_millis(20)).is_none());
        assert!(started.elapsed() >= Duration::from_millis(20));
    }

    #[test]
    fn a_waiting_build_wakes_when_the_block_is_published() {
        let (_, sealed) = fixture();
        let hash = sealed.header.hash();
        let publisher = {
            let sealed = sealed.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(20));
                record(hash, Some(sealed));
            })
        };
        let got = wait_sealed(hash, Duration::from_secs(5)).expect("published");
        assert_eq!(got.header.hash(), hash);
        publisher.join().unwrap();
        forget(hash);
    }

    #[test]
    fn attributes_must_be_later_than_the_sealed_parent() {
        let (_, sealed) = fixture();
        let at = |timestamp| PayloadAttributes {
            timestamp,
            ..Default::default()
        };
        assert!(check_attributes(&sealed.header, &at(10)).is_err());
        assert!(check_attributes(&sealed.header, &at(9)).is_err());
        assert!(check_attributes(&sealed.header, &at(11)).is_ok());
    }
}
