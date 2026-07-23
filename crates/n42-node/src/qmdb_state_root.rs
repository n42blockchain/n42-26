//! Branch-safe, correctness-first QMDB state-root tracking for Gov5 Engine imports.
//!
//! The store retains per-block operation deltas and reconstructs each candidate from an
//! authenticated base snapshot. This is intentionally bounded and slower than gov5's persistent
//! undo/index implementation, but it never mutates a canonical global tree before the candidate's
//! header root has matched. It is the safe execution-follower bridge for bounded replay ranges.

use alloy_primitives::B256;
use alloy_rpc_types_engine::ExecutionData;
use n42_twig_core::qmdb_compat::{
    QmdbCompatTree, QmdbOperation, QmdbOperationError, QmdbProof, QmdbSnapshot, QmdbSnapshotError,
};
use reth_chain_state::StateTrieOverlayManager;
use reth_engine_tree::tree::state_root_strategy::{
    LazyHashedPostState, PreparedStateRootJob, StateRootJob, StateRootJobContext,
    StateRootJobOutcome, StateRootStrategy,
};
use reth_engine_tree::tree::{BasicEngineValidator, TreeConfig};
use reth_ethereum_primitives::{EthPrimitives, Receipt};
use reth_evm::{ConfigureEngineEvm, ConfigureEvm};
use reth_node_api::FullNodeComponents;
use reth_node_builder::rpc::{BasicEngineValidatorBuilder, ChangesetCache, EngineValidatorBuilder};
use reth_primitives_traits::RecoveredBlock;
use reth_provider::{BlockExecutionOutput, ProviderError, ProviderResult};
use reth_trie::updates::TrieUpdates;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use crate::{
    engine_validator::{N42EngineValidator, N42EngineValidatorBuilder},
    node::N42Node,
    qmdb_state::gov5_qmdb_operations_from_output,
};

/// Maximum ancestry replay accepted by the bounded interoperability strategy.
pub const DEFAULT_QMDB_REPLAY_DEPTH: usize = 4096;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredQmdbBlock {
    parent_hash: B256,
    root: B256,
    operations: Vec<QmdbOperation>,
}

#[derive(Debug, Serialize, Deserialize)]
struct QmdbBranchState {
    blocks: HashMap<B256, StoredQmdbBlock>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedQmdbBranchState {
    version: u32,
    base_block_hash: B256,
    base_root: B256,
    base_snapshot: QmdbSnapshot,
    blocks: HashMap<B256, StoredQmdbBlock>,
}

/// Thread-safe QMDB candidate store rooted at one authenticated checkpoint.
#[derive(Debug)]
pub struct Gov5QmdbStateRootStore {
    base_block_hash: B256,
    base_root: B256,
    base_snapshot: QmdbSnapshot,
    max_replay_depth: usize,
    persistence_path: Option<PathBuf>,
    state: Mutex<QmdbBranchState>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Gov5QmdbStateRootError {
    #[error("QMDB base snapshot is invalid: {0}")]
    InvalidBaseSnapshot(#[from] QmdbSnapshotError),
    #[error("QMDB base root mismatch: rebuilt {got}, expected {expected}")]
    BaseRootMismatch { got: B256, expected: B256 },
    #[error("QMDB parent {0} is not descended from the configured base checkpoint")]
    MissingParent(B256),
    #[error("QMDB ancestry exceeds the configured replay depth {0}")]
    ReplayDepthExceeded(usize),
    #[error("QMDB stored branch root diverged for block {block_hash}: got {got}, stored {stored}")]
    StoredRootDivergence {
        block_hash: B256,
        got: B256,
        stored: B256,
    },
    #[error("QMDB block {block_hash} root mismatch: computed {got}, header {expected}")]
    RootMismatch {
        block_hash: B256,
        got: B256,
        expected: B256,
    },
    #[error("QMDB block mutation is invalid: {0}")]
    InvalidOperations(#[from] QmdbOperationError),
    #[error("QMDB state-root store lock is poisoned")]
    LockPoisoned,
    #[error("QMDB branch-state persistence failed: {0}")]
    Persistence(String),
    #[error("persisted QMDB branch state does not match the authenticated base")]
    PersistedBaseMismatch,
}

impl Gov5QmdbStateRootStore {
    /// Create a bounded branch store only after rebuilding and authenticating the supplied base.
    pub fn new(
        base_block_hash: B256,
        base_root: B256,
        base_snapshot: QmdbSnapshot,
    ) -> Result<Self, Gov5QmdbStateRootError> {
        Self::with_max_replay_depth(
            base_block_hash,
            base_root,
            base_snapshot,
            DEFAULT_QMDB_REPLAY_DEPTH,
        )
    }

    pub fn with_max_replay_depth(
        base_block_hash: B256,
        base_root: B256,
        base_snapshot: QmdbSnapshot,
        max_replay_depth: usize,
    ) -> Result<Self, Gov5QmdbStateRootError> {
        let rebuilt = B256::from(QmdbCompatTree::from_snapshot(&base_snapshot)?.root());
        if rebuilt != base_root {
            return Err(Gov5QmdbStateRootError::BaseRootMismatch {
                got: rebuilt,
                expected: base_root,
            });
        }
        Ok(Self {
            base_block_hash,
            base_root,
            base_snapshot,
            max_replay_depth,
            persistence_path: None,
            state: Mutex::new(QmdbBranchState {
                blocks: HashMap::new(),
            }),
        })
    }

    /// Opens a crash-safe branch store. Existing state must be bound to the
    /// exact authenticated base and every retained block root is replayed
    /// before the store is accepted.
    pub fn persistent(
        base_block_hash: B256,
        base_root: B256,
        base_snapshot: QmdbSnapshot,
        max_replay_depth: usize,
        path: PathBuf,
    ) -> Result<Self, Gov5QmdbStateRootError> {
        let mut store = Self::with_max_replay_depth(
            base_block_hash,
            base_root,
            base_snapshot,
            max_replay_depth,
        )?;
        store.persistence_path = Some(path.clone());
        match std::fs::read(&path) {
            Ok(bytes) => {
                let persisted: PersistedQmdbBranchState = bincode::deserialize(&bytes)
                    .map_err(|error| Gov5QmdbStateRootError::Persistence(error.to_string()))?;
                if persisted.version != 1
                    || persisted.base_block_hash != store.base_block_hash
                    || persisted.base_root != store.base_root
                    || persisted.base_snapshot != store.base_snapshot
                {
                    return Err(Gov5QmdbStateRootError::PersistedBaseMismatch);
                }
                validate_persisted_blocks(
                    store.base_block_hash,
                    store.base_root,
                    &store.base_snapshot,
                    store.max_replay_depth,
                    &persisted.blocks,
                )?;
                store.state = Mutex::new(QmdbBranchState {
                    blocks: persisted.blocks,
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(Gov5QmdbStateRootError::Persistence(error.to_string()));
            }
        }
        Ok(store)
    }

    pub const fn base_block_hash(&self) -> B256 {
        self.base_block_hash
    }

    pub const fn base_root(&self) -> B256 {
        self.base_root
    }

    pub fn retained_block_count(&self) -> Result<usize, Gov5QmdbStateRootError> {
        Ok(self
            .state
            .lock()
            .map_err(|_| Gov5QmdbStateRootError::LockPoisoned)?
            .blocks
            .len())
    }

    /// Compute a candidate from its exact parent branch and publish its delta only after its root
    /// equals the hash-authenticated header commitment. A mismatch leaves the store unchanged.
    pub fn compute_candidate(
        &self,
        parent_hash: B256,
        operations: &[QmdbOperation],
    ) -> Result<B256, Gov5QmdbStateRootError> {
        let state = self
            .state
            .lock()
            .map_err(|_| Gov5QmdbStateRootError::LockPoisoned)?;
        self.compute_candidate_locked(&state, parent_hash, operations)
    }

    pub fn compute_and_commit(
        &self,
        parent_hash: B256,
        block_hash: B256,
        expected_root: B256,
        operations: Vec<QmdbOperation>,
    ) -> Result<B256, Gov5QmdbStateRootError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| Gov5QmdbStateRootError::LockPoisoned)?;
        if let Some(stored) = state.blocks.get(&block_hash) {
            return if stored.parent_hash == parent_hash
                && stored.root == expected_root
                && stored.operations == operations
            {
                Ok(stored.root)
            } else {
                Err(Gov5QmdbStateRootError::RootMismatch {
                    block_hash,
                    got: stored.root,
                    expected: expected_root,
                })
            };
        }

        let root = self.compute_candidate_locked(&state, parent_hash, &operations)?;
        if root != expected_root {
            return Err(Gov5QmdbStateRootError::RootMismatch {
                block_hash,
                got: root,
                expected: expected_root,
            });
        }
        state.blocks.insert(
            block_hash,
            StoredQmdbBlock {
                parent_hash,
                root,
                operations,
            },
        );
        if let Err(error) = self.persist_locked(&state) {
            state.blocks.remove(&block_hash);
            return Err(error);
        }
        qualification_abort_at("qmdb_committed");
        Ok(root)
    }

    fn compute_candidate_locked(
        &self,
        state: &QmdbBranchState,
        parent_hash: B256,
        operations: &[QmdbOperation],
    ) -> Result<B256, Gov5QmdbStateRootError> {
        let mut tree = self.reconstruct_tree_locked(state, parent_hash)?;
        Ok(B256::from(
            tree.apply_sorted_ops(operations.iter().cloned())?,
        ))
    }

    fn reconstruct_tree_locked(
        &self,
        state: &QmdbBranchState,
        block_hash: B256,
    ) -> Result<QmdbCompatTree, Gov5QmdbStateRootError> {
        let mut lineage = Vec::new();
        let mut cursor = block_hash;
        while cursor != self.base_block_hash {
            if lineage.len() >= self.max_replay_depth {
                return Err(Gov5QmdbStateRootError::ReplayDepthExceeded(
                    self.max_replay_depth,
                ));
            }
            let stored = state
                .blocks
                .get(&cursor)
                .ok_or(Gov5QmdbStateRootError::MissingParent(cursor))?;
            lineage.push((cursor, stored));
            cursor = stored.parent_hash;
        }

        let mut tree = QmdbCompatTree::from_snapshot(&self.base_snapshot)?;
        for (hash, stored) in lineage.into_iter().rev() {
            let root = B256::from(tree.apply_sorted_ops(stored.operations.iter().cloned())?);
            if root != stored.root {
                return Err(Gov5QmdbStateRootError::StoredRootDivergence {
                    block_hash: hash,
                    got: root,
                    stored: stored.root,
                });
            }
        }
        Ok(tree)
    }

    pub fn contains(&self, block_hash: B256) -> Result<bool, Gov5QmdbStateRootError> {
        Ok(self
            .state
            .lock()
            .map_err(|_| Gov5QmdbStateRootError::LockPoisoned)?
            .blocks
            .contains_key(&block_hash))
    }

    pub fn root_for(&self, block_hash: B256) -> Result<Option<B256>, Gov5QmdbStateRootError> {
        if block_hash == self.base_block_hash {
            return Ok(Some(self.base_root));
        }
        Ok(self
            .state
            .lock()
            .map_err(|_| Gov5QmdbStateRootError::LockPoisoned)?
            .blocks
            .get(&block_hash)
            .map(|block| block.root))
    }

    /// Reconstruct an immutable historical snapshot for an exact retained
    /// block. Unknown hashes return `None`; corrupt retained ancestry fails
    /// closed instead of serving an unauthenticated state.
    pub fn snapshot_for(
        &self,
        block_hash: B256,
    ) -> Result<Option<QmdbSnapshot>, Gov5QmdbStateRootError> {
        let state = self
            .state
            .lock()
            .map_err(|_| Gov5QmdbStateRootError::LockPoisoned)?;
        if block_hash != self.base_block_hash && !state.blocks.contains_key(&block_hash) {
            return Ok(None);
        }
        Ok(Some(
            self.reconstruct_tree_locked(&state, block_hash)?.snapshot(),
        ))
    }

    /// Generate a gov5-compatible QMDB membership proof at an exact retained
    /// historical block. `None` covers an unknown block or an absent key.
    pub fn proof_for(
        &self,
        block_hash: B256,
        key: [u8; 32],
    ) -> Result<Option<QmdbProof>, Gov5QmdbStateRootError> {
        let state = self
            .state
            .lock()
            .map_err(|_| Gov5QmdbStateRootError::LockPoisoned)?;
        if block_hash != self.base_block_hash && !state.blocks.contains_key(&block_hash) {
            return Ok(None);
        }
        Ok(self
            .reconstruct_tree_locked(&state, block_hash)?
            .prove(&key))
    }

    /// Returns the number of parent edges from the authenticated base to an
    /// exact retained block. This lets restart recovery bind a QMDB-proven
    /// side branch to its execution block number without trusting a stale
    /// canonical hash index.
    pub fn distance_from_base(
        &self,
        block_hash: B256,
    ) -> Result<Option<usize>, Gov5QmdbStateRootError> {
        if block_hash == self.base_block_hash {
            return Ok(Some(0));
        }
        let state = self
            .state
            .lock()
            .map_err(|_| Gov5QmdbStateRootError::LockPoisoned)?;
        let mut distance = 0usize;
        let mut cursor = block_hash;
        while cursor != self.base_block_hash {
            if distance >= self.max_replay_depth {
                return Err(Gov5QmdbStateRootError::ReplayDepthExceeded(
                    self.max_replay_depth,
                ));
            }
            let Some(block) = state.blocks.get(&cursor) else {
                return Ok(None);
            };
            distance += 1;
            cursor = block.parent_hash;
        }
        Ok(Some(distance))
    }

    pub fn parent_for(&self, block_hash: B256) -> Result<Option<B256>, Gov5QmdbStateRootError> {
        if block_hash == self.base_block_hash {
            return Ok(None);
        }
        Ok(self
            .state
            .lock()
            .map_err(|_| Gov5QmdbStateRootError::LockPoisoned)?
            .blocks
            .get(&block_hash)
            .map(|block| block.parent_hash))
    }

    fn persist_locked(&self, state: &QmdbBranchState) -> Result<(), Gov5QmdbStateRootError> {
        let Some(path) = &self.persistence_path else {
            return Ok(());
        };
        let persisted = PersistedQmdbBranchState {
            version: 1,
            base_block_hash: self.base_block_hash,
            base_root: self.base_root,
            base_snapshot: self.base_snapshot.clone(),
            blocks: state.blocks.clone(),
        };
        let bytes = bincode::serialize(&persisted)
            .map_err(|error| Gov5QmdbStateRootError::Persistence(error.to_string()))?;
        atomic_write(path, &bytes)
    }
}

fn qualification_abort_at(point: &str) {
    if std::env::var("N42_QUALIFICATION_ABORT_AT").ok().as_deref() == Some(point) {
        eprintln!("N42_QUALIFICATION_ABORT_AT={point}: aborting after durable boundary");
        std::process::abort();
    }
}

fn validate_persisted_blocks(
    base_block_hash: B256,
    base_root: B256,
    base_snapshot: &QmdbSnapshot,
    max_replay_depth: usize,
    blocks: &HashMap<B256, StoredQmdbBlock>,
) -> Result<(), Gov5QmdbStateRootError> {
    let mut validated = HashMap::from([(base_block_hash, base_root)]);
    let mut pending: Vec<_> = blocks.keys().copied().collect();
    for _ in 0..=blocks.len() {
        let before = pending.len();
        pending.retain(|hash| {
            let block = &blocks[hash];
            let Some(_) = validated.get(&block.parent_hash) else {
                return true;
            };
            let mut lineage = Vec::new();
            let mut cursor = *hash;
            while cursor != base_block_hash {
                if lineage.len() >= max_replay_depth {
                    return true;
                }
                let Some(stored) = blocks.get(&cursor) else {
                    return true;
                };
                lineage.push((cursor, stored));
                cursor = stored.parent_hash;
            }
            let Ok(mut tree) = QmdbCompatTree::from_snapshot(base_snapshot) else {
                return true;
            };
            for (lineage_hash, stored) in lineage.into_iter().rev() {
                let Ok(root) = tree.apply_sorted_ops(stored.operations.iter().cloned()) else {
                    return true;
                };
                if B256::from(root) != stored.root {
                    return true;
                }
                validated.insert(lineage_hash, stored.root);
            }
            false
        });
        if pending.is_empty() {
            return Ok(());
        }
        if pending.len() == before {
            break;
        }
    }
    Err(Gov5QmdbStateRootError::Persistence(format!(
        "{} retained QMDB blocks have missing ancestry, excessive depth, or divergent roots",
        pending.len()
    )))
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), Gov5QmdbStateRootError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| Gov5QmdbStateRootError::Persistence(error.to_string()))?;
    }
    let tmp = path.with_extension("tmp");
    let mut file = std::fs::File::create(&tmp)
        .map_err(|error| Gov5QmdbStateRootError::Persistence(error.to_string()))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| Gov5QmdbStateRootError::Persistence(error.to_string()))?;
    std::fs::rename(&tmp, path)
        .map_err(|error| Gov5QmdbStateRootError::Persistence(error.to_string()))
}

/// Reth 2.4.1 Engine Tree strategy that replaces only state-root computation. Execution itself,
/// receipt validation, gas accounting, and Reth's final header-root comparison remain mandatory.
#[derive(Debug, Clone)]
pub struct Gov5QmdbStateRootStrategy {
    store: Arc<Gov5QmdbStateRootStore>,
}

impl Gov5QmdbStateRootStrategy {
    pub const fn new(store: Arc<Gov5QmdbStateRootStore>) -> Self {
        Self { store }
    }
}

impl<P, Evm> StateRootStrategy<EthPrimitives, P, Evm> for Gov5QmdbStateRootStrategy
where
    Evm: ConfigureEvm<Primitives = EthPrimitives>,
{
    fn prepare(
        &self,
        _ctx: StateRootJobContext<'_, EthPrimitives, P, Evm>,
    ) -> ProviderResult<PreparedStateRootJob<EthPrimitives>> {
        Ok(PreparedStateRootJob::new(
            Box::new(Gov5QmdbStateRootJob {
                store: self.store.clone(),
            }),
            None,
        ))
    }
}

#[derive(Debug)]
struct Gov5QmdbStateRootJob {
    store: Arc<Gov5QmdbStateRootStore>,
}

impl StateRootJob<EthPrimitives> for Gov5QmdbStateRootJob {
    fn name(&self) -> &'static str {
        "gov5-qmdb"
    }

    fn finish(
        &mut self,
        block: &RecoveredBlock<reth_ethereum_primitives::Block>,
        output: Arc<BlockExecutionOutput<Receipt>>,
        _hashed_state: &LazyHashedPostState,
    ) -> ProviderResult<StateRootJobOutcome> {
        let operations = gov5_qmdb_operations_from_output(&output);
        if std::env::var_os("N42_QMDB_TRACE_OPERATIONS").is_some() {
            for operation in &operations {
                tracing::info!(
                    target: "n42::qmdb",
                    block = block.number,
                    key = %B256::from(operation.key),
                    value = operation.value.as_ref().map(hex::encode).as_deref().unwrap_or("<delete>"),
                    "QMDB execution mutation"
                );
            }
        }
        let root = match self.store.compute_and_commit(
            block.parent_hash,
            block.hash(),
            block.state_root,
            operations,
        ) {
            Ok(root) => root,
            // Return the independently computed root so Reth classifies the payload as
            // deterministically Invalid through its normal BodyStateRootDiff path. The store did
            // not publish the mismatching candidate.
            Err(Gov5QmdbStateRootError::RootMismatch { got, .. }) => got,
            Err(error) => return Err(ProviderError::other(error)),
        };
        Ok(StateRootJobOutcome::new(
            root,
            Arc::new(TrieUpdates::default()),
        ))
    }
}

/// Engine-tree validator builder that installs the QMDB strategy only when an authenticated base
/// store was explicitly supplied. With `None`, it returns Reth's stock validator unchanged.
#[derive(Clone)]
pub struct N42EngineTreeValidatorBuilder {
    inner: BasicEngineValidatorBuilder<N42EngineValidatorBuilder>,
    qmdb_store: Option<Arc<Gov5QmdbStateRootStore>>,
}

impl std::fmt::Debug for N42EngineTreeValidatorBuilder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("N42EngineTreeValidatorBuilder")
            .field("inner", &self.inner)
            .field("has_qmdb_store", &self.qmdb_store.is_some())
            .finish()
    }
}

impl N42EngineTreeValidatorBuilder {
    pub const fn new(
        payload_validator: N42EngineValidatorBuilder,
        qmdb_store: Option<Arc<Gov5QmdbStateRootStore>>,
    ) -> Self {
        Self {
            inner: BasicEngineValidatorBuilder::new(payload_validator),
            qmdb_store,
        }
    }
}

impl<Node> EngineValidatorBuilder<Node> for N42EngineTreeValidatorBuilder
where
    Node: FullNodeComponents<Types = N42Node, Evm: ConfigureEngineEvm<ExecutionData>>,
{
    type EngineValidator = BasicEngineValidator<
        Node::Provider,
        Node::Evm,
        N42EngineValidator<reth_chainspec::ChainSpec>,
    >;

    async fn build_tree_validator(
        self,
        ctx: &reth_node_api::AddOnsContext<'_, Node>,
        tree_config: TreeConfig,
        changeset_cache: ChangesetCache,
        state_trie_overlays: StateTrieOverlayManager<EthPrimitives>,
    ) -> eyre::Result<Self::EngineValidator> {
        let validator = self
            .inner
            .build_tree_validator(ctx, tree_config, changeset_cache, state_trie_overlays)
            .await?;
        let Some(store) = self.qmdb_store else {
            return Ok(validator);
        };
        let strategy: Arc<dyn StateRootStrategy<EthPrimitives, Node::Provider, Node::Evm>> =
            Arc::new(Gov5QmdbStateRootStrategy::new(store));
        Ok(validator.with_state_root_strategy(strategy))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn operation(key: u8, value: u8) -> QmdbOperation {
        QmdbOperation {
            key: [key; 32],
            value: Some(vec![value]),
        }
    }

    fn store() -> (Gov5QmdbStateRootStore, QmdbSnapshot) {
        let mut base = QmdbCompatTree::new();
        base.set([1; 32], vec![1]);
        let snapshot = base.snapshot();
        (
            Gov5QmdbStateRootStore::new(
                B256::repeat_byte(0x10),
                B256::from(base.root()),
                snapshot.clone(),
            )
            .unwrap(),
            snapshot,
        )
    }

    fn expected_root(snapshot: &QmdbSnapshot, blocks: &[Vec<QmdbOperation>]) -> B256 {
        let mut tree = QmdbCompatTree::from_snapshot(snapshot).unwrap();
        for operations in blocks {
            tree.apply_sorted_ops(operations.iter().cloned()).unwrap();
        }
        B256::from(tree.root())
    }

    #[test]
    fn candidate_is_published_only_after_root_match() {
        let (store, snapshot) = store();
        let block = B256::repeat_byte(0x20);
        let operations = vec![operation(2, 2)];
        let expected = expected_root(&snapshot, std::slice::from_ref(&operations));
        assert_eq!(
            store
                .compute_candidate(store.base_block_hash(), &operations)
                .unwrap(),
            expected
        );
        assert!(!store.contains(block).unwrap());
        assert!(matches!(
            store.compute_and_commit(
                store.base_block_hash(),
                block,
                B256::repeat_byte(0xff),
                operations.clone(),
            ),
            Err(Gov5QmdbStateRootError::RootMismatch { .. })
        ));
        assert!(!store.contains(block).unwrap());

        assert_eq!(
            store
                .compute_and_commit(store.base_block_hash(), block, expected, operations)
                .unwrap(),
            expected
        );
        assert!(store.contains(block).unwrap());
    }

    #[test]
    fn sibling_candidates_reconstruct_from_their_exact_parent_branch() {
        let (store, snapshot) = store();
        let left = B256::repeat_byte(0x21);
        let right = B256::repeat_byte(0x22);
        let left_ops = vec![operation(2, 2)];
        let right_ops = vec![operation(3, 3)];
        let left_root = expected_root(&snapshot, std::slice::from_ref(&left_ops));
        let right_root = expected_root(&snapshot, std::slice::from_ref(&right_ops));
        store
            .compute_and_commit(store.base_block_hash(), left, left_root, left_ops.clone())
            .unwrap();
        store
            .compute_and_commit(store.base_block_hash(), right, right_root, right_ops)
            .unwrap();

        let child = B256::repeat_byte(0x31);
        let child_ops = vec![operation(4, 4)];
        let child_root = expected_root(&snapshot, &[left_ops, child_ops.clone()]);
        assert_eq!(
            store
                .compute_and_commit(left, child, child_root, child_ops)
                .unwrap(),
            child_root
        );
        assert_eq!(
            store.distance_from_base(store.base_block_hash()).unwrap(),
            Some(0)
        );
        assert_eq!(store.distance_from_base(left).unwrap(), Some(1));
        assert_eq!(store.distance_from_base(child).unwrap(), Some(2));
        assert_eq!(store.parent_for(child).unwrap(), Some(left));
        assert_eq!(store.parent_for(store.base_block_hash()).unwrap(), None);
        assert_eq!(
            store.distance_from_base(B256::repeat_byte(0xFE)).unwrap(),
            None
        );
        assert_ne!(left_root, right_root);
    }

    #[test]
    fn historical_snapshot_and_proof_are_bound_to_exact_block() {
        let (store, snapshot) = store();
        let first = B256::repeat_byte(0x61);
        let second = B256::repeat_byte(0x62);
        let first_ops = vec![operation(2, 2)];
        let second_ops = vec![operation(2, 3), operation(3, 3)];
        let first_root = expected_root(&snapshot, std::slice::from_ref(&first_ops));
        let second_root = expected_root(&snapshot, &[first_ops.clone(), second_ops.clone()]);
        store
            .compute_and_commit(store.base_block_hash(), first, first_root, first_ops)
            .unwrap();
        store
            .compute_and_commit(first, second, second_root, second_ops)
            .unwrap();

        let first_snapshot = store.snapshot_for(first).unwrap().unwrap();
        let first_tree = QmdbCompatTree::from_snapshot(&first_snapshot).unwrap();
        assert_eq!(B256::from(first_tree.root()), first_root);
        assert_eq!(first_tree.get(&[2; 32]), Some([2].as_slice()));

        let first_proof = store.proof_for(first, [2; 32]).unwrap().unwrap();
        assert!(first_proof.verify_for_key(first_root.as_ref(), &[2; 32]));
        assert_eq!(first_proof.value, vec![2]);
        let second_proof = store.proof_for(second, [2; 32]).unwrap().unwrap();
        assert!(second_proof.verify_for_key(second_root.as_ref(), &[2; 32]));
        assert_eq!(second_proof.value, vec![3]);
        assert!(store.proof_for(first, [3; 32]).unwrap().is_none());
        assert!(
            store
                .snapshot_for(B256::repeat_byte(0xff))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn missing_parent_and_depth_limit_fail_closed() {
        let (store, snapshot) = store();
        assert_eq!(
            store.compute_and_commit(
                B256::repeat_byte(0xee),
                B256::repeat_byte(0x20),
                B256::ZERO,
                Vec::new(),
            ),
            Err(Gov5QmdbStateRootError::MissingParent(B256::repeat_byte(
                0xee
            )))
        );

        let shallow = Gov5QmdbStateRootStore::with_max_replay_depth(
            store.base_block_hash(),
            store.base_root(),
            snapshot.clone(),
            1,
        )
        .unwrap();
        let first = B256::repeat_byte(0x40);
        let first_ops = vec![operation(5, 5)];
        let first_root = expected_root(&snapshot, std::slice::from_ref(&first_ops));
        shallow
            .compute_and_commit(
                shallow.base_block_hash(),
                first,
                first_root,
                first_ops.clone(),
            )
            .unwrap();
        let second_ops = vec![operation(6, 6)];
        let second_root = expected_root(&snapshot, &[first_ops, second_ops.clone()]);
        shallow
            .compute_and_commit(first, B256::repeat_byte(0x41), second_root, second_ops)
            .unwrap();
        assert!(matches!(
            shallow.compute_and_commit(
                B256::repeat_byte(0x41),
                B256::repeat_byte(0x42),
                B256::ZERO,
                Vec::new(),
            ),
            Err(Gov5QmdbStateRootError::ReplayDepthExceeded(1))
        ));
    }

    #[test]
    fn persistent_store_replays_and_rejects_wrong_base() {
        let (store, snapshot) = store();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("branches.bin");
        let persistent = Gov5QmdbStateRootStore::persistent(
            store.base_block_hash(),
            store.base_root(),
            snapshot.clone(),
            8,
            path.clone(),
        )
        .unwrap();
        let block = B256::repeat_byte(0x51);
        let operations = vec![operation(7, 7)];
        let root = expected_root(&snapshot, std::slice::from_ref(&operations));
        persistent
            .compute_and_commit(persistent.base_block_hash(), block, root, operations)
            .unwrap();

        let reopened = Gov5QmdbStateRootStore::persistent(
            store.base_block_hash(),
            store.base_root(),
            snapshot.clone(),
            8,
            path.clone(),
        )
        .unwrap();
        assert_eq!(reopened.root_for(block).unwrap(), Some(root));
        assert_eq!(
            Gov5QmdbStateRootStore::persistent(
                B256::repeat_byte(0xff),
                store.base_root(),
                snapshot,
                8,
                path,
            )
            .unwrap_err(),
            Gov5QmdbStateRootError::PersistedBaseMismatch
        );
    }
}
