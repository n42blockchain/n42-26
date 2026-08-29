//! Branch-safe, correctness-first QMDB state-root tracking for Gov5 Engine imports.
//!
//! The store keeps one split QMDB commitment ([`QmdbLeafTree`]: sealed twigs as leaf root plus
//! bits, the open twig in full, the live entries by key) and moves it along the block graph
//! in place. A candidate is priced by applying its operations under an undo record, reading
//! the root and reverting; a committed block leaves the tree at itself and keeps its undo
//! record so the tree can walk back to a sibling's parent. Nothing is ever cloned per block.
//! Per-block operation deltas are retained (and appended to a checksummed WAL) so every
//! retained block can be reached again from the authenticated base, and the tree itself is
//! written out in leaf form as a new base every `N42_QMDB_REBASE_BLOCKS` commits.

use alloy_primitives::B256;
use alloy_rpc_types_engine::ExecutionData;
use n42_twig_core::qmdb_compat::{
    BlockUndo, QmdbCompatTree, QmdbOperation, QmdbOperationError, QmdbProof, QmdbSnapshot,
    QmdbSnapshotError, QmdbUndoError,
};
use n42_twig_core::qmdb_leaf_tree::{
    QmdbLeafFormHeader, QmdbLeafFormIoError, QmdbLeafTree, QmdbLeafTreeError,
};
use reth_engine_tree::tree::state_root_strategy::{
    LazyHashedPostState, PreparedStateRootJob, StateRootJob, StateRootJobContext,
    StateRootJobOutcome, StateRootStrategy,
};
use reth_engine_tree::tree::{BasicEngineValidator, TreeConfig};
use reth_ethereum_primitives::{EthPrimitives, Receipt};
use reth_evm::{ConfigureEngineEvm, ConfigureEvm};
use reth_node_api::FullNodeComponents;
use reth_node_builder::rpc::{BasicEngineValidatorBuilder, EngineValidatorBuilder};
use reth_primitives_traits::RecoveredBlock;
use reth_provider::{BlockExecutionOutput, ProviderError, ProviderResult};
use reth_storage_overlay::OverlayManager;
use reth_trie::updates::TrieUpdates;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use crate::{
    engine_validator::{N42EngineValidator, N42EngineValidatorBuilder},
    node::N42Node,
    qmdb_state::{
        gov5_qmdb_operations_with_restored, gov5_restored_slots_key, with_gov5_prague_system_caller,
    },
};

/// Maximum ancestry replay accepted by the bounded interoperability strategy.
///
/// The production Gov5 bridge retains an authenticated lineage from its
/// bootstrap checkpoint. The previous participant default (65,536, duplicated
/// in the CLI) made a healthy node deterministically fail at block 65,538.
/// Keep one shared default with enough runway for qualification and production
/// replacement windows. Operators can still set a smaller explicit bound for
/// fail-closed testing or a larger audited bound for longer archive horizons.
pub const DEFAULT_QMDB_REPLAY_DEPTH: usize = 1_048_576;

/// Undo records kept behind the tree's position. The tree can walk back this
/// many committed blocks without a rebuild; a sibling deeper than that is
/// reached from the base by replaying the retained operations instead.
const RETAINED_UNDO_RECORDS: usize = 8_192;

/// Committed blocks between two background rewrites of the base file.
pub const DEFAULT_QMDB_REBASE_BLOCKS: u64 = 20_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoredQmdbBlock {
    parent_hash: B256,
    root: B256,
    operations: Vec<QmdbOperation>,
}

/// One undo record on the tree's path: the block it reverts and the block
/// the tree stands at after reverting it.
struct AppliedUndo {
    block_hash: B256,
    parent_hash: B256,
    undo: BlockUndo,
}

// In-memory only: persistence goes through `PersistedQmdbBranchState` and the
// base file; the tree is a derived accelerator whose position is transient.
struct QmdbBranchState {
    blocks: HashMap<B256, StoredQmdbBlock>,
    /// The one tree, moved in place along the graph.
    tree: QmdbLeafTree,
    /// The block the tree currently represents (the base, or a block in
    /// `blocks`).
    tree_at: B256,
    /// Parent edges from the base to `tree_at`.
    tree_depth: usize,
    /// Undo records of the blocks applied to reach `tree_at`, oldest first.
    /// Popping the newest moves the tree to that block's parent.
    applied: VecDeque<AppliedUndo>,
    /// A block whose delta is already in `blocks` but whose WAL frame is still
    /// being written and fsynced outside this lock. Commits are serialized, so
    /// at most one block is ever in flight, and it is always the newest tip.
    /// Archive readers treat it as absent until it is durable; speculative
    /// candidates may build on it, exactly as they could build on a block
    /// whose commit later fails.
    pending_durable: Option<B256>,
}

// Hand-written so the tree's contents never land in a log line; its identity
// and depth are the only parts worth seeing.
impl std::fmt::Debug for QmdbBranchState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QmdbBranchState")
            .field("blocks", &self.blocks.len())
            .field("tree_at", &self.tree_at)
            .field("tree_depth", &self.tree_depth)
            .field("applied", &self.applied.len())
            .field("pending_durable", &self.pending_durable)
            .finish()
    }
}

/// The checkpoint file: the base's identity and every retained block. The
/// base tree itself lives next to it in leaf form (`<path>.base.qmdb`).
#[derive(Debug, Serialize, Deserialize)]
struct PersistedQmdbBranchState {
    version: u32,
    base_block_hash: B256,
    base_root: B256,
    blocks: HashMap<B256, StoredQmdbBlock>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedQmdbWalRecord {
    block_hash: B256,
    block: StoredQmdbBlock,
}

/// Borrowing twin of [`PersistedQmdbWalRecord`]: bincode encodes a reference
/// exactly like the owned value, so a commit can frame its WAL record without
/// cloning the operations it is about to move into `blocks`.
#[derive(Serialize)]
struct PersistedQmdbWalRecordRef<'a> {
    block_hash: B256,
    block: &'a StoredQmdbBlock,
}

/// Persistent WAL handle. Appends are serialized through their own mutex,
/// independent of `state`, so a block's write and fsync never block candidate
/// computation or archive reads.
#[derive(Debug)]
struct QmdbWalFile {
    file: std::fs::File,
    /// Length after the last fully written frame; a torn append is rolled
    /// back to it.
    len: u64,
    /// Set once a torn append could not be rolled back. `len` then no longer
    /// matches the file, so a later append would land behind garbage and a
    /// later rollback could cut into a durable frame; every further commit is
    /// refused instead, and the next open recovers the file from disk.
    poisoned: Option<String>,
}

/// Test-only WAL fault injection, applied to the next append.
#[cfg(test)]
#[derive(Clone, Copy, Debug)]
enum WalFault {
    /// Stall inside the append, outside the `state` lock.
    Delay(std::time::Duration),
    /// Write half the frame, then fail as an I/O error would.
    FailWrite,
    /// Like `FailWrite`, but the rollback truncation fails as well, leaving
    /// the torn half-frame on disk.
    FailWriteAndRollback,
}

const QMDB_WAL_MAX_RECORD_BYTES: usize = 64 * 1024 * 1024;
const QMDB_WAL_CHECKSUM_BYTES: usize = 32;
const PERSISTED_BRANCH_STATE_VERSION: u32 = 2;

/// What a leaf-form base file identifies itself as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QmdbBaseIdentity {
    pub chain_id: u64,
    pub genesis_hash: B256,
    pub block_number: u64,
}

/// Thread-safe QMDB candidate store rooted at one authenticated checkpoint.
#[derive(Debug)]
pub struct Gov5QmdbStateRootStore {
    base_block_hash: B256,
    base_root: B256,
    /// Chain identity and base height, written into the base file's header.
    identity: QmdbBaseIdentity,
    max_replay_depth: usize,
    persistence_path: Option<PathBuf>,
    rebase_every: u64,
    commits_since_rebase: AtomicU64,
    rebase_in_flight: Arc<AtomicBool>,
    state: Mutex<QmdbBranchState>,
    /// Serializes commits end to end (tree work, insert, WAL append). WAL order
    /// then equals insertion order and a child can never be published while
    /// its parent's durability is still in flight.
    commit: Mutex<()>,
    wal: Mutex<Option<QmdbWalFile>>,
    #[cfg(test)]
    wal_fault: Mutex<Option<WalFault>>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Gov5QmdbStateRootError {
    #[error("QMDB base snapshot is invalid: {0}")]
    InvalidBaseSnapshot(#[from] QmdbSnapshotError),
    #[error("QMDB base leaf form is invalid: {0}")]
    InvalidBaseLeafForm(#[from] QmdbLeafTreeError),
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
    #[error("QMDB tree could not be reverted: {0}")]
    Undo(#[from] QmdbUndoError),
    #[error("QMDB state-root store lock is poisoned")]
    LockPoisoned,
    #[error("QMDB branch-state persistence failed: {0}")]
    Persistence(String),
    #[error("persisted QMDB branch state does not match the authenticated base")]
    PersistedBaseMismatch,
}

impl From<QmdbLeafFormIoError> for Gov5QmdbStateRootError {
    fn from(error: QmdbLeafFormIoError) -> Self {
        Self::Persistence(error.to_string())
    }
}

/// Where the leaf-form base file of a checkpoint lives.
pub fn base_file_path(checkpoint_path: &Path) -> PathBuf {
    let mut name = checkpoint_path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_default();
    name.push(".base.qmdb");
    checkpoint_path.with_file_name(name)
}

/// Turns a positional snapshot into the split tree, checking its root.
fn leaf_tree_from_snapshot(
    snapshot: &QmdbSnapshot,
    expected_root: B256,
) -> Result<QmdbLeafTree, Gov5QmdbStateRootError> {
    let full = QmdbCompatTree::from_snapshot(snapshot)?;
    let rebuilt = B256::from(full.root());
    if rebuilt != expected_root {
        return Err(Gov5QmdbStateRootError::BaseRootMismatch {
            got: rebuilt,
            expected: expected_root,
        });
    }
    let mut tree = QmdbLeafTree::from_leaf_form(&full.leaf_form())?;
    let split = B256::from(tree.root());
    if split != expected_root {
        return Err(Gov5QmdbStateRootError::BaseRootMismatch {
            got: split,
            expected: expected_root,
        });
    }
    Ok(tree)
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
        let tree = leaf_tree_from_snapshot(&base_snapshot, base_root)?;
        Self::from_leaf_tree(base_block_hash, base_root, tree, max_replay_depth)
    }

    /// A store around an already verified split tree standing at the base.
    pub fn from_leaf_tree(
        base_block_hash: B256,
        base_root: B256,
        mut tree: QmdbLeafTree,
        max_replay_depth: usize,
    ) -> Result<Self, Gov5QmdbStateRootError> {
        let rebuilt = B256::from(tree.root());
        if rebuilt != base_root {
            return Err(Gov5QmdbStateRootError::BaseRootMismatch {
                got: rebuilt,
                expected: base_root,
            });
        }
        Ok(Self {
            base_block_hash,
            base_root,
            identity: QmdbBaseIdentity {
                chain_id: 0,
                genesis_hash: B256::ZERO,
                block_number: 0,
            },
            max_replay_depth,
            persistence_path: None,
            rebase_every: std::env::var("N42_QMDB_REBASE_BLOCKS")
                .ok()
                .and_then(|value| value.parse().ok())
                .filter(|value| *value > 0)
                .unwrap_or(DEFAULT_QMDB_REBASE_BLOCKS),
            commits_since_rebase: AtomicU64::new(0),
            rebase_in_flight: Arc::new(AtomicBool::new(false)),
            state: Mutex::new(QmdbBranchState {
                blocks: HashMap::new(),
                tree,
                tree_at: base_block_hash,
                tree_depth: 0,
                applied: VecDeque::new(),
                pending_durable: None,
            }),
            commit: Mutex::new(()),
            wal: Mutex::new(None),
            #[cfg(test)]
            wal_fault: Mutex::new(None),
        })
    }

    /// Records the chain identity and base height written into base files.
    pub fn with_identity(mut self, identity: QmdbBaseIdentity) -> Self {
        self.identity = identity;
        self
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
        let tree = leaf_tree_from_snapshot(&base_snapshot, base_root)?;
        Self::persistent_from_leaf_tree(base_block_hash, base_root, tree, max_replay_depth, path)
    }

    /// Like [`Self::persistent`], around a verified split tree at the base.
    pub fn persistent_from_leaf_tree(
        base_block_hash: B256,
        base_root: B256,
        tree: QmdbLeafTree,
        max_replay_depth: usize,
        path: PathBuf,
    ) -> Result<Self, Gov5QmdbStateRootError> {
        let mut store = Self::from_leaf_tree(base_block_hash, base_root, tree, max_replay_depth)?;
        store.persistence_path = Some(path.clone());
        match std::fs::read(&path) {
            Ok(bytes) => {
                let persisted: PersistedQmdbBranchState = bincode::deserialize(&bytes)
                    .map_err(|error| Gov5QmdbStateRootError::Persistence(error.to_string()))?;
                if persisted.version != PERSISTED_BRANCH_STATE_VERSION
                    || persisted.base_block_hash != store.base_block_hash
                    || persisted.base_root != store.base_root
                {
                    return Err(Gov5QmdbStateRootError::PersistedBaseMismatch);
                }
                let mut blocks = persisted.blocks;
                load_wal(&wal_path(&path), &mut blocks)?;
                let state = store
                    .state
                    .get_mut()
                    .map_err(|_| Gov5QmdbStateRootError::LockPoisoned)?;
                validate_persisted_blocks(
                    &mut state.tree,
                    store.base_block_hash,
                    store.max_replay_depth,
                    &mut blocks,
                )?;
                state.blocks = blocks;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                store.persist_checkpoint_locked(&HashMap::new())?;
            }
            Err(error) => {
                return Err(Gov5QmdbStateRootError::Persistence(error.to_string()));
            }
        }
        // Open once, after recovery has truncated any torn tail, and keep the
        // handle for the store's lifetime instead of re-opening per block.
        store.wal = Mutex::new(Some(open_wal_file(&wal_path(&path))?));
        Ok(store)
    }

    /// Reads a leaf-form base file and its header, checking the root it
    /// claims. The caller binds the header's block to its chain.
    pub fn read_base_file(
        path: &Path,
    ) -> Result<(QmdbLeafTree, QmdbLeafFormHeader), Gov5QmdbStateRootError> {
        let file = std::fs::File::open(path).map_err(|error| {
            Gov5QmdbStateRootError::Persistence(format!(
                "failed to open QMDB base {}: {error}",
                path.display()
            ))
        })?;
        let (mut tree, header) =
            QmdbLeafTree::read_leaf_form_v2(std::io::BufReader::with_capacity(1 << 20, file))?;
        let rebuilt = B256::from(tree.root());
        if rebuilt != B256::from(header.root) {
            return Err(Gov5QmdbStateRootError::BaseRootMismatch {
                got: rebuilt,
                expected: B256::from(header.root),
            });
        }
        Ok((tree, header))
    }

    pub const fn base_block_hash(&self) -> B256 {
        self.base_block_hash
    }

    pub const fn base_root(&self) -> B256 {
        self.base_root
    }

    pub fn retained_block_count(&self) -> Result<usize, Gov5QmdbStateRootError> {
        let state = self
            .state
            .lock()
            .map_err(|_| Gov5QmdbStateRootError::LockPoisoned)?;
        Ok(state
            .blocks
            .len()
            .saturating_sub(usize::from(state.pending_durable.is_some())))
    }

    /// Live entries and append cursor of the tree, for start-up logging.
    pub fn tree_stats(&self) -> Result<(usize, u64, usize), Gov5QmdbStateRootError> {
        let state = self
            .state
            .lock()
            .map_err(|_| Gov5QmdbStateRootError::LockPoisoned)?;
        Ok((
            state.tree.len(),
            state.tree.next_slot(),
            state.tree.twig_count(),
        ))
    }

    /// A retained block as archive readers see it: durable, or absent.
    fn durable_block(state: &QmdbBranchState, block_hash: B256) -> Option<&StoredQmdbBlock> {
        if state.pending_durable == Some(block_hash) {
            return None;
        }
        state.blocks.get(&block_hash)
    }

    #[cfg(test)]
    fn inject_wal_fault(&self, fault: WalFault) {
        *self.wal_fault.lock().unwrap() = Some(fault);
    }

    #[cfg(test)]
    fn wal_in_flight(&self) -> bool {
        self.state.lock().unwrap().pending_durable.is_some()
    }

    #[cfg(test)]
    fn tree_position(&self) -> (B256, usize, usize) {
        let state = self.state.lock().unwrap();
        (state.tree_at, state.tree_depth, state.applied.len())
    }

    /// Compute a candidate from its exact parent branch and publish its delta only after its root
    /// equals the hash-authenticated header commitment. A mismatch leaves the store unchanged.
    pub fn compute_candidate(
        &self,
        parent_hash: B256,
        operations: &[QmdbOperation],
    ) -> Result<B256, Gov5QmdbStateRootError> {
        let lock_started = std::time::Instant::now();
        let mut state = self
            .state
            .lock()
            .map_err(|_| Gov5QmdbStateRootError::LockPoisoned)?;
        let lock_acquired = std::time::Instant::now();
        metrics::histogram!("n42_qmdb_lock_wait_ms", "operation" => "candidate")
            .record(lock_acquired.duration_since(lock_started).as_secs_f64() * 1_000.0);
        let compute_started = std::time::Instant::now();
        let result = self
            .compute_candidate_locked(&mut state, parent_hash, operations)
            .map(|(root, _)| root);
        metrics::histogram!("n42_qmdb_candidate_compute_ms", "operation" => "candidate")
            .record(compute_started.elapsed().as_secs_f64() * 1_000.0);
        metrics::histogram!("n42_qmdb_lock_hold_ms", "operation" => "candidate")
            .record(lock_acquired.elapsed().as_secs_f64() * 1_000.0);
        result
    }

    /// Commit a block: price its candidate under the `state` lock, publish the
    /// delta, then write and fsync the WAL frame with the lock released. A
    /// block whose WAL append fails is rolled back — from `blocks` and from
    /// the tree — and reported as an error, so it is never considered
    /// committed.
    pub fn compute_and_commit(
        &self,
        parent_hash: B256,
        block_hash: B256,
        expected_root: B256,
        operations: Vec<QmdbOperation>,
    ) -> Result<B256, Gov5QmdbStateRootError> {
        let operation_count = operations.len();
        let _commit = self
            .commit
            .lock()
            .map_err(|_| Gov5QmdbStateRootError::LockPoisoned)?;
        let lock_started = std::time::Instant::now();
        let mut state = self
            .state
            .lock()
            .map_err(|_| Gov5QmdbStateRootError::LockPoisoned)?;
        let lock_acquired = std::time::Instant::now();
        metrics::histogram!("n42_qmdb_lock_wait_ms", "operation" => "commit")
            .record(lock_acquired.duration_since(lock_started).as_secs_f64() * 1_000.0);
        metrics::histogram!("n42_qmdb_operations_per_block").record(operation_count as f64);
        if let Some(stored) = state.blocks.get(&block_hash) {
            let result = if stored.parent_hash == parent_hash
                && stored.root == expected_root
                && stored.operations == operations
            {
                metrics::counter!("n42_qmdb_commit_outcomes_total", "outcome" => "cache_hit")
                    .increment(1);
                Ok(stored.root)
            } else {
                metrics::counter!("n42_qmdb_commit_outcomes_total", "outcome" => "cache_conflict")
                    .increment(1);
                Err(Gov5QmdbStateRootError::RootMismatch {
                    block_hash,
                    got: stored.root,
                    expected: expected_root,
                })
            };
            metrics::histogram!("n42_qmdb_lock_hold_ms", "operation" => "commit")
                .record(lock_acquired.elapsed().as_secs_f64() * 1_000.0);
            return result;
        }

        let compute_started = std::time::Instant::now();
        let candidate = self.compute_candidate_locked(&mut state, parent_hash, &operations);
        metrics::histogram!("n42_qmdb_candidate_compute_ms", "operation" => "commit")
            .record(compute_started.elapsed().as_secs_f64() * 1_000.0);
        let (root, undo) = match candidate {
            Ok(candidate) => candidate,
            Err(error) => {
                metrics::counter!("n42_qmdb_commit_outcomes_total", "outcome" => "compute_error")
                    .increment(1);
                metrics::histogram!("n42_qmdb_lock_hold_ms", "operation" => "commit")
                    .record(lock_acquired.elapsed().as_secs_f64() * 1_000.0);
                return Err(error);
            }
        };
        if root != expected_root {
            metrics::counter!("n42_qmdb_commit_outcomes_total", "outcome" => "root_mismatch")
                .increment(1);
            metrics::histogram!("n42_qmdb_lock_hold_ms", "operation" => "commit")
                .record(lock_acquired.elapsed().as_secs_f64() * 1_000.0);
            return Err(Gov5QmdbStateRootError::RootMismatch {
                block_hash,
                got: root,
                expected: expected_root,
            });
        }
        let stored = StoredQmdbBlock {
            parent_hash,
            root,
            operations,
        };
        // Frame the record while the operations are still ours to borrow, so
        // a record that cannot be encoded never enters `blocks`. The write and
        // fsync happen after the lock is released.
        let frame = match self.encode_wal_frame(block_hash, &stored) {
            Ok(frame) => frame,
            Err(error) => {
                metrics::counter!("n42_qmdb_commit_outcomes_total", "outcome" => "wal_error")
                    .increment(1);
                metrics::histogram!("n42_qmdb_lock_hold_ms", "operation" => "commit")
                    .record(lock_acquired.elapsed().as_secs_f64() * 1_000.0);
                return Err(error);
            }
        };
        // Leave the tree at the new block: re-apply the priced operations
        // (the candidate was reverted) and keep the undo record.
        if let Some(undo) = undo {
            let reapplied = match state
                .tree
                .apply_sorted_ops_recorded(stored.operations.iter().cloned())
            {
                Ok((reapplied_root, undo)) => {
                    debug_assert_eq!(B256::from(reapplied_root), root);
                    undo
                }
                Err(error) => {
                    metrics::counter!("n42_qmdb_commit_outcomes_total", "outcome" => "compute_error")
                        .increment(1);
                    return Err(error.into());
                }
            };
            debug_assert_eq!(undo.prev_next_slot, reapplied.prev_next_slot);
            Self::push_applied(&mut state, block_hash, parent_hash, reapplied);
        } else {
            // Empty block: the tree's contents are its parent's; only the
            // position moves.
            let undo = BlockUndo {
                prev_next_slot: state.tree.next_slot(),
                entries: Vec::new(),
                appended_keys: Vec::new(),
            };
            Self::push_applied(&mut state, block_hash, parent_hash, undo);
        }
        state.blocks.insert(block_hash, stored);
        state.pending_durable = frame.is_some().then_some(block_hash);
        let mut lock_hold = lock_acquired.elapsed();
        drop(state);

        let wal_started = std::time::Instant::now();
        let wal_result = match &frame {
            Some(frame) => self.append_wal_frame(frame),
            None => Ok(()),
        };
        metrics::histogram!("n42_qmdb_wal_append_ms")
            .record(wal_started.elapsed().as_secs_f64() * 1_000.0);

        let publish_started = std::time::Instant::now();
        let mut state = self
            .state
            .lock()
            .map_err(|_| Gov5QmdbStateRootError::LockPoisoned)?;
        state.pending_durable = None;
        if let Err(error) = wal_result {
            metrics::counter!("n42_qmdb_commit_outcomes_total", "outcome" => "wal_error")
                .increment(1);
            state.blocks.remove(&block_hash);
            // The tree still stands at the failed block: step back off it.
            if state.tree_at == block_hash {
                Self::pop_applied(&mut state)?;
            }
            lock_hold += publish_started.elapsed();
            metrics::histogram!("n42_qmdb_lock_hold_ms", "operation" => "commit")
                .record(lock_hold.as_secs_f64() * 1_000.0);
            return Err(error);
        }
        lock_hold += publish_started.elapsed();
        drop(state);
        qualification_abort_at("qmdb_committed");
        metrics::counter!("n42_qmdb_commit_outcomes_total", "outcome" => "committed").increment(1);
        metrics::histogram!("n42_qmdb_lock_hold_ms", "operation" => "commit")
            .record(lock_hold.as_secs_f64() * 1_000.0);
        self.maybe_rebase_in_background();
        Ok(root)
    }

    fn push_applied(
        state: &mut QmdbBranchState,
        block_hash: B256,
        parent_hash: B256,
        undo: BlockUndo,
    ) {
        debug_assert_eq!(state.tree_at, parent_hash);
        state.applied.push_back(AppliedUndo {
            block_hash,
            parent_hash,
            undo,
        });
        while state.applied.len() > RETAINED_UNDO_RECORDS {
            state.applied.pop_front();
        }
        state.tree_at = block_hash;
        state.tree_depth = state.tree_depth.saturating_add(1);
    }

    /// Reverts the newest applied block; the tree then stands at its parent.
    fn pop_applied(state: &mut QmdbBranchState) -> Result<(), Gov5QmdbStateRootError> {
        let applied = state
            .applied
            .pop_back()
            .ok_or(Gov5QmdbStateRootError::MissingParent(state.tree_at))?;
        debug_assert_eq!(applied.block_hash, state.tree_at);
        state.tree.apply_undo(&applied.undo)?;
        state.tree_at = applied.parent_hash;
        state.tree_depth = state.tree_depth.saturating_sub(1);
        Ok(())
    }

    /// Returns the candidate root and, for a non-empty candidate, the undo
    /// record of its (already reverted) application. The tree is left at
    /// `parent_hash`.
    fn compute_candidate_locked(
        &self,
        state: &mut QmdbBranchState,
        parent_hash: B256,
        operations: &[QmdbOperation],
    ) -> Result<(B256, Option<BlockUndo>), Gov5QmdbStateRootError> {
        // Applying no operations cannot alter a QMDB root. Avoid walking the
        // graph for the common empty-block case when the total retained graph
        // proves the configured depth cannot have been exceeded. Once the
        // graph is larger than that bound, fall back to exact ancestry
        // reconstruction so heavily branched stores still fail closed.
        if operations.is_empty() && state.blocks.len() <= self.max_replay_depth {
            let root = if parent_hash == self.base_block_hash {
                self.base_root
            } else {
                state
                    .blocks
                    .get(&parent_hash)
                    .map(|block| block.root)
                    .ok_or(Gov5QmdbStateRootError::MissingParent(parent_hash))?
            };
            // The committer wants the tree at the parent all the same.
            self.move_tree_to(state, parent_hash)?;
            return Ok((root, None));
        }
        let parent_depth = self.move_tree_to(state, parent_hash)?;
        if parent_depth.saturating_add(1) > self.max_replay_depth.saturating_add(1) {
            return Err(Gov5QmdbStateRootError::ReplayDepthExceeded(
                self.max_replay_depth,
            ));
        }
        let (root, undo) = state
            .tree
            .apply_sorted_ops_recorded(operations.iter().cloned())?;
        state.tree.apply_undo(&undo)?;
        Ok((B256::from(root), Some(undo)))
    }

    /// Moves the tree to `target` — the base or a retained block — and
    /// reports the target's depth from the base.
    ///
    /// Walks the target's ancestry back to the nearest block the tree can
    /// reach by reverting (its current position or any parent on its applied
    /// path), reverts down to it, then replays the retained operations
    /// forward, root-checking each block against what was stored when it was
    /// committed. Every step is bounded by `max_replay_depth`.
    fn move_tree_to(
        &self,
        state: &mut QmdbBranchState,
        target: B256,
    ) -> Result<usize, Gov5QmdbStateRootError> {
        if target == state.tree_at {
            metrics::counter!("n42_qmdb_tip_cache_total", "outcome" => "hit").increment(1);
            return Ok(state.tree_depth);
        }
        metrics::counter!("n42_qmdb_tip_cache_total", "outcome" => "miss").increment(1);
        // Blocks the tree can stand at by popping `n` applied records.
        let mut reachable: HashMap<B256, usize> = HashMap::with_capacity(state.applied.len() + 1);
        reachable.insert(state.tree_at, 0);
        for (pops, applied) in state.applied.iter().rev().enumerate() {
            reachable.entry(applied.parent_hash).or_insert(pops + 1);
        }
        let mut lineage: Vec<B256> = Vec::new();
        let mut cursor = target;
        let pops = loop {
            if let Some(pops) = reachable.get(&cursor) {
                break *pops;
            }
            if cursor == self.base_block_hash {
                // The base fell off the applied path; the tree cannot return
                // there without a rebuild from its base file.
                return Err(Gov5QmdbStateRootError::ReplayDepthExceeded(
                    self.max_replay_depth,
                ));
            }
            // Also bounds a cycle in `blocks`: the walk cannot run forever.
            if lineage.len() >= self.max_replay_depth {
                return Err(Gov5QmdbStateRootError::ReplayDepthExceeded(
                    self.max_replay_depth,
                ));
            }
            let stored = state
                .blocks
                .get(&cursor)
                .ok_or(Gov5QmdbStateRootError::MissingParent(cursor))?;
            lineage.push(cursor);
            cursor = stored.parent_hash;
        };
        metrics::histogram!("n42_qmdb_tree_moves", "direction" => "revert").record(pops as f64);
        metrics::histogram!("n42_qmdb_tree_moves", "direction" => "replay")
            .record(lineage.len() as f64);
        for _ in 0..pops {
            Self::pop_applied(state)?;
        }
        debug_assert_eq!(state.tree_at, cursor);
        let depth = state.tree_depth.saturating_add(lineage.len());
        if depth > self.max_replay_depth.saturating_add(1) {
            return Err(Gov5QmdbStateRootError::ReplayDepthExceeded(
                self.max_replay_depth,
            ));
        }
        for hash in lineage.into_iter().rev() {
            let QmdbBranchState { blocks, tree, .. } = state;
            let stored = blocks
                .get(&hash)
                .ok_or(Gov5QmdbStateRootError::MissingParent(hash))?;
            let (root, undo) = tree.apply_sorted_ops_recorded(stored.operations.iter().cloned())?;
            let root = B256::from(root);
            if root != stored.root {
                // Put the tree back where it was before failing.
                let stored_root = stored.root;
                tree.apply_undo(&undo)?;
                return Err(Gov5QmdbStateRootError::StoredRootDivergence {
                    block_hash: hash,
                    got: root,
                    stored: stored_root,
                });
            }
            let parent_hash = stored.parent_hash;
            Self::push_applied(state, hash, parent_hash, undo);
        }
        Ok(state.tree_depth)
    }

    pub fn contains(&self, block_hash: B256) -> Result<bool, Gov5QmdbStateRootError> {
        let state = self
            .state
            .lock()
            .map_err(|_| Gov5QmdbStateRootError::LockPoisoned)?;
        Ok(Self::durable_block(&state, block_hash).is_some())
    }

    pub fn root_for(&self, block_hash: B256) -> Result<Option<B256>, Gov5QmdbStateRootError> {
        if block_hash == self.base_block_hash {
            return Ok(Some(self.base_root));
        }
        let state = self
            .state
            .lock()
            .map_err(|_| Gov5QmdbStateRootError::LockPoisoned)?;
        Ok(Self::durable_block(&state, block_hash).map(|block| block.root))
    }

    /// Reconstruct an immutable historical snapshot, in leaf form, for an
    /// exact retained block. Unknown hashes return `None`; corrupt retained
    /// ancestry fails closed instead of serving an unauthenticated state.
    pub fn snapshot_for(
        &self,
        block_hash: B256,
    ) -> Result<Option<QmdbSnapshot>, Gov5QmdbStateRootError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| Gov5QmdbStateRootError::LockPoisoned)?;
        if block_hash != self.base_block_hash && Self::durable_block(&state, block_hash).is_none() {
            return Ok(None);
        }
        self.move_tree_to(&mut state, block_hash)?;
        Ok(Some(QmdbSnapshot {
            next_slot: state.tree.next_slot(),
            entries: Vec::new(),
            leaf_form: Some(state.tree.leaf_form()),
        }))
    }

    /// Generate a gov5-compatible QMDB membership proof at an exact retained
    /// historical block. `None` covers an unknown block, an absent key, and a
    /// key whose leaf sits in a sealed twig (the split tree holds no leaf
    /// siblings there).
    pub fn proof_for(
        &self,
        block_hash: B256,
        key: [u8; 32],
    ) -> Result<Option<QmdbProof>, Gov5QmdbStateRootError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| Gov5QmdbStateRootError::LockPoisoned)?;
        if block_hash != self.base_block_hash && Self::durable_block(&state, block_hash).is_none() {
            return Ok(None);
        }
        self.move_tree_to(&mut state, block_hash)?;
        Ok(state.tree.prove(&key))
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
            let Some(block) = Self::durable_block(&state, cursor) else {
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
        let state = self
            .state
            .lock()
            .map_err(|_| Gov5QmdbStateRootError::LockPoisoned)?;
        Ok(Self::durable_block(&state, block_hash).map(|block| block.parent_hash))
    }

    /// Writes the tree, standing at a durable committed block, as a new base
    /// file. The next open of this checkpoint starts from that block and
    /// replays only the retained blocks above it. The file is written from a
    /// copy of the tree taken under the lock, so imports continue meanwhile;
    /// returns the block the base was taken at.
    pub fn write_base_file(&self) -> Result<Option<(B256, u64)>, Gov5QmdbStateRootError> {
        let Some(path) = &self.persistence_path else {
            return Ok(None);
        };
        let (tree, at, depth) = {
            let state = self
                .state
                .lock()
                .map_err(|_| Gov5QmdbStateRootError::LockPoisoned)?;
            if state.pending_durable.is_some() && state.pending_durable == Some(state.tree_at) {
                return Ok(None);
            }
            (state.tree.clone(), state.tree_at, state.tree_depth)
        };
        let root = self
            .root_for(at)?
            .ok_or(Gov5QmdbStateRootError::MissingParent(at))?;
        let block_number = self.identity.block_number.saturating_add(depth as u64);
        write_base_file(path, &tree, self.identity, block_number, at, root)?;
        Ok(Some((at, block_number)))
    }

    fn maybe_rebase_in_background(&self) {
        if self.persistence_path.is_none() {
            return;
        }
        let commits = self.commits_since_rebase.fetch_add(1, Ordering::Relaxed) + 1;
        if commits < self.rebase_every {
            return;
        }
        if self
            .rebase_in_flight
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        self.commits_since_rebase.store(0, Ordering::Relaxed);
        let (tree, at, depth) = match self.state.lock() {
            Ok(state) => (state.tree.clone(), state.tree_at, state.tree_depth),
            Err(_) => {
                self.rebase_in_flight.store(false, Ordering::Release);
                return;
            }
        };
        let root = match self.root_for(at) {
            Ok(Some(root)) => root,
            _ => {
                self.rebase_in_flight.store(false, Ordering::Release);
                return;
            }
        };
        let path = self.persistence_path.clone().expect("checked above");
        let identity = self.identity;
        let in_flight = self.rebase_in_flight.clone();
        let block_number = identity.block_number.saturating_add(depth as u64);
        std::thread::Builder::new()
            .name("qmdb-rebase".into())
            .spawn(move || {
                let started = std::time::Instant::now();
                match write_base_file(&path, &tree, identity, block_number, at, root) {
                    Ok(()) => tracing::info!(
                        target: "n42::qmdb",
                        block_number,
                        %at,
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        live = tree.len(),
                        "QMDB base file rewritten in the background"
                    ),
                    Err(error) => tracing::error!(
                        target: "n42::qmdb",
                        %error,
                        "QMDB base file rewrite failed; the previous base stays in use"
                    ),
                }
                in_flight.store(false, Ordering::Release);
            })
            .map(|_| ())
            .unwrap_or_else(|_| self.rebase_in_flight.store(false, Ordering::Release));
    }

    fn persist_checkpoint_locked(
        &self,
        blocks: &HashMap<B256, StoredQmdbBlock>,
    ) -> Result<(), Gov5QmdbStateRootError> {
        let Some(path) = &self.persistence_path else {
            return Ok(());
        };
        let persisted = PersistedQmdbBranchState {
            version: PERSISTED_BRANCH_STATE_VERSION,
            base_block_hash: self.base_block_hash,
            base_root: self.base_root,
            blocks: blocks.clone(),
        };
        let bytes = bincode::serialize(&persisted)
            .map_err(|error| Gov5QmdbStateRootError::Persistence(error.to_string()))?;
        atomic_write(path, &bytes)
    }

    /// Frame a WAL record (`len || payload || blake3`) for `block_hash`, or
    /// `None` when the store is not persistent.
    fn encode_wal_frame(
        &self,
        block_hash: B256,
        block: &StoredQmdbBlock,
    ) -> Result<Option<Vec<u8>>, Gov5QmdbStateRootError> {
        if self.persistence_path.is_none() {
            return Ok(None);
        }
        let payload = bincode::serialize(&PersistedQmdbWalRecordRef { block_hash, block })
            .map_err(|error| Gov5QmdbStateRootError::Persistence(error.to_string()))?;
        if payload.len() > QMDB_WAL_MAX_RECORD_BYTES {
            return Err(Gov5QmdbStateRootError::Persistence(format!(
                "QMDB WAL record is {} bytes, exceeding {}",
                payload.len(),
                QMDB_WAL_MAX_RECORD_BYTES
            )));
        }
        let payload_len = u32::try_from(payload.len()).map_err(|_| {
            Gov5QmdbStateRootError::Persistence("QMDB WAL record exceeds u32 length".into())
        })?;
        let checksum = blake3::hash(&payload);
        let mut frame = Vec::with_capacity(4 + payload.len() + QMDB_WAL_CHECKSUM_BYTES);
        frame.extend_from_slice(&payload_len.to_le_bytes());
        frame.extend_from_slice(&payload);
        frame.extend_from_slice(checksum.as_bytes());
        Ok(Some(frame))
    }

    /// Append one framed record through the persistent handle and make it
    /// durable.
    ///
    /// Durability uses `sync_data` (`fdatasync`) rather than `sync_all`. The
    /// append only extends the file, and POSIX requires `fdatasync` to flush
    /// every piece of metadata needed to read the written data back, which
    /// includes the new size: both ext4 and XFS journal a size extension as
    /// part of `fdatasync`, and only timestamps and similar bookkeeping are
    /// left behind. The directory entry of a freshly created WAL is made
    /// durable once, in [`open_wal_file`], so a first append after open needs
    /// no `sync_all` either.
    ///
    /// A failed or torn write is rolled back to the previous length. If that
    /// rollback itself fails the handle is poisoned and every later commit is
    /// refused, because `len` would no longer describe the file.
    fn append_wal_frame(&self, frame: &[u8]) -> Result<(), Gov5QmdbStateRootError> {
        let mut guard = self
            .wal
            .lock()
            .map_err(|_| Gov5QmdbStateRootError::LockPoisoned)?;
        let Some(wal) = guard.as_mut() else {
            return Ok(());
        };
        if let Some(reason) = &wal.poisoned {
            return Err(Gov5QmdbStateRootError::Persistence(format!(
                "QMDB WAL refuses appends after a failed rollback: {reason}"
            )));
        }
        #[cfg(test)]
        let (injected, fail_rollback) = self.apply_wal_fault(wal, frame);
        #[cfg(not(test))]
        let (injected, fail_rollback): (Option<std::io::Error>, bool) = (None, false);
        let write_result = match injected {
            Some(error) => Err(error),
            None => wal.file.write_all(frame).and_then(|()| {
                let fsync_started = std::time::Instant::now();
                let synced = wal.file.sync_data();
                metrics::histogram!("n42_qmdb_wal_fsync_ms")
                    .record(fsync_started.elapsed().as_secs_f64() * 1_000.0);
                synced
            }),
        };
        if let Err(error) = write_result {
            let rollback = if fail_rollback {
                Err(std::io::Error::other("injected QMDB WAL rollback failure"))
            } else {
                wal.file.set_len(wal.len).and_then(|()| wal.file.sync_all())
            };
            if let Err(rollback_error) = rollback {
                let reason = format!(
                    "append failed ({error}) and rollback to {} bytes failed ({rollback_error})",
                    wal.len
                );
                metrics::counter!("n42_qmdb_wal_poisoned_total").increment(1);
                tracing::error!(target: "n42::qmdb", %reason, "QMDB WAL poisoned; refusing further commits until restart");
                wal.poisoned = Some(reason.clone());
                return Err(Gov5QmdbStateRootError::Persistence(reason));
            }
            return Err(Gov5QmdbStateRootError::Persistence(error.to_string()));
        }
        wal.len = wal.len.saturating_add(frame.len() as u64);
        Ok(())
    }

    #[cfg(test)]
    fn apply_wal_fault(
        &self,
        wal: &mut QmdbWalFile,
        frame: &[u8],
    ) -> (Option<std::io::Error>, bool) {
        let Some(fault) = self.wal_fault.lock().unwrap().take() else {
            return (None, false);
        };
        match fault {
            WalFault::Delay(duration) => {
                std::thread::sleep(duration);
                (None, false)
            }
            WalFault::FailWrite | WalFault::FailWriteAndRollback => {
                let _ = wal.file.write_all(&frame[..frame.len() / 2]);
                (
                    Some(std::io::Error::other("injected QMDB WAL write failure")),
                    matches!(fault, WalFault::FailWriteAndRollback),
                )
            }
        }
    }
}

fn write_base_file(
    checkpoint_path: &Path,
    tree: &QmdbLeafTree,
    identity: QmdbBaseIdentity,
    block_number: u64,
    block_hash: B256,
    root: B256,
) -> Result<(), Gov5QmdbStateRootError> {
    let path = base_file_path(checkpoint_path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| Gov5QmdbStateRootError::Persistence(error.to_string()))?;
    }
    let tmp = path.with_extension("qmdb.tmp");
    let file = std::fs::File::create(&tmp)
        .map_err(|error| Gov5QmdbStateRootError::Persistence(error.to_string()))?;
    let mut writer = std::io::BufWriter::with_capacity(1 << 20, file);
    tree.write_leaf_form_v2(
        &mut writer,
        identity.chain_id,
        &identity.genesis_hash.0,
        block_number,
        &block_hash.0,
        &root.0,
    )?;
    writer
        .into_inner()
        .map_err(|error| Gov5QmdbStateRootError::Persistence(error.to_string()))?
        .sync_all()
        .map_err(|error| Gov5QmdbStateRootError::Persistence(error.to_string()))?;
    std::fs::rename(&tmp, &path)
        .map_err(|error| Gov5QmdbStateRootError::Persistence(error.to_string()))
}

fn wal_path(checkpoint_path: &Path) -> PathBuf {
    checkpoint_path.with_extension("wal")
}

fn open_wal_file(path: &Path) -> Result<QmdbWalFile, Gov5QmdbStateRootError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| Gov5QmdbStateRootError::Persistence(error.to_string()))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(path)
        .map_err(|error| Gov5QmdbStateRootError::Persistence(error.to_string()))?;
    let len = file
        .metadata()
        .map_err(|error| Gov5QmdbStateRootError::Persistence(error.to_string()))?
        .len();
    // A newly created WAL is only durable once its directory entry is; sync
    // the directory here, once per open, so per-block appends can rely on
    // `fdatasync` alone.
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)
            .and_then(|dir| dir.sync_all())
            .map_err(|error| Gov5QmdbStateRootError::Persistence(error.to_string()))?;
    }
    Ok(QmdbWalFile {
        file,
        len,
        poisoned: None,
    })
}

fn load_wal(
    path: &Path,
    blocks: &mut HashMap<B256, StoredQmdbBlock>,
) -> Result<(), Gov5QmdbStateRootError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(Gov5QmdbStateRootError::Persistence(error.to_string())),
    };
    let mut offset = 0usize;
    while offset < bytes.len() {
        let record_start = offset;
        let Some(length_bytes) = bytes.get(offset..offset.saturating_add(4)) else {
            truncate_incomplete_wal(path, record_start)?;
            break;
        };
        let payload_len = u32::from_le_bytes(length_bytes.try_into().expect("four bytes")) as usize;
        if payload_len > QMDB_WAL_MAX_RECORD_BYTES {
            return Err(Gov5QmdbStateRootError::Persistence(format!(
                "QMDB WAL record at offset {record_start} declares invalid length {payload_len}"
            )));
        }
        offset += 4;
        let Some(frame_end) = offset
            .checked_add(payload_len)
            .and_then(|end| end.checked_add(QMDB_WAL_CHECKSUM_BYTES))
        else {
            return Err(Gov5QmdbStateRootError::Persistence(
                "QMDB WAL frame length overflow".into(),
            ));
        };
        if frame_end > bytes.len() {
            truncate_incomplete_wal(path, record_start)?;
            break;
        }
        let payload = &bytes[offset..offset + payload_len];
        offset += payload_len;
        let checksum = &bytes[offset..frame_end];
        if blake3::hash(payload).as_bytes() != checksum {
            return Err(Gov5QmdbStateRootError::Persistence(format!(
                "QMDB WAL checksum mismatch at offset {record_start}"
            )));
        }
        let record: PersistedQmdbWalRecord = bincode::deserialize(payload)
            .map_err(|error| Gov5QmdbStateRootError::Persistence(error.to_string()))?;
        if let Some(existing) = blocks.insert(record.block_hash, record.block.clone())
            && existing != record.block
        {
            return Err(Gov5QmdbStateRootError::Persistence(format!(
                "QMDB WAL redefines block {}",
                record.block_hash
            )));
        }
        offset = frame_end;
    }
    Ok(())
}

fn truncate_incomplete_wal(path: &Path, valid_len: usize) -> Result<(), Gov5QmdbStateRootError> {
    OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|file| file.set_len(valid_len as u64))
        .map_err(|error| Gov5QmdbStateRootError::Persistence(error.to_string()))
}

fn qualification_abort_at(point: &str) {
    if std::env::var("N42_QUALIFICATION_ABORT_AT").ok().as_deref() == Some(point) {
        eprintln!("N42_QUALIFICATION_ABORT_AT={point}: aborting after durable boundary");
        std::process::abort();
    }
}

/// Replays every retained block over the base tree, depth first, reverting
/// each branch on the way back, and checks each stored root. Blocks that do
/// not descend from the base (retained below a base that has since moved
/// forward) are dropped from `blocks` with a warning; a block whose ancestry
/// is missing or cyclic is an error. The tree is left at the base.
fn validate_persisted_blocks(
    tree: &mut QmdbLeafTree,
    base_block_hash: B256,
    max_replay_depth: usize,
    blocks: &mut HashMap<B256, StoredQmdbBlock>,
) -> Result<(), Gov5QmdbStateRootError> {
    let mut children: HashMap<B256, Vec<B256>> = HashMap::new();
    for (hash, block) in blocks.iter() {
        children.entry(block.parent_hash).or_default().push(*hash);
    }
    let mut validated: HashSet<B256> = HashSet::with_capacity(blocks.len());
    // (block, depth, remaining children, undo of this block on the tree)
    let mut stack: Vec<(B256, usize, Vec<B256>, Option<BlockUndo>)> = vec![(
        base_block_hash,
        0,
        children.remove(&base_block_hash).unwrap_or_default(),
        None,
    )];
    while let Some(frame) = stack.last_mut() {
        let Some(child) = frame.2.pop() else {
            let (_, _, _, undo) = stack.pop().expect("frame exists");
            if let Some(undo) = undo {
                tree.apply_undo(&undo)?;
            }
            continue;
        };
        let depth = frame.1.checked_add(1).ok_or_else(|| {
            Gov5QmdbStateRootError::Persistence("QMDB ancestry depth overflow".into())
        })?;
        // Candidate computation replays the parent and then applies the
        // child's operations, so a stored child may be one level beyond the
        // parent replay bound.
        if depth > max_replay_depth.saturating_add(1) {
            return Err(Gov5QmdbStateRootError::Persistence(format!(
                "QMDB block {child} exceeds replay depth {max_replay_depth}"
            )));
        }
        let block = &blocks[&child];
        let (root, undo) = tree.apply_sorted_ops_recorded(block.operations.iter().cloned())?;
        let root = B256::from(root);
        if root != block.root {
            return Err(Gov5QmdbStateRootError::Persistence(format!(
                "QMDB stored root diverged for block {child}: got {root}, stored {}",
                block.root
            )));
        }
        validated.insert(child);
        let grandchildren = children.remove(&child).unwrap_or_default();
        stack.push((child, depth, grandchildren, Some(undo)));
    }
    if validated.len() != blocks.len() {
        // Anything not reached from the base: below a base that moved on
        // (harmless, dropped) or an orphan/cycle (refused).
        let unreachable: Vec<B256> = blocks
            .keys()
            .filter(|hash| !validated.contains(*hash))
            .copied()
            .collect();
        let orphaned = unreachable
            .iter()
            .filter(|hash| {
                let mut cursor = **hash;
                let mut steps = 0usize;
                loop {
                    let Some(block) = blocks.get(&cursor) else {
                        // Ancestry leaves the retained set without reaching
                        // the base: below the base, or truly missing. A block
                        // below the base has an ancestor chain that ends at a
                        // hash we never retained; treat as droppable.
                        return false;
                    };
                    if block.parent_hash == base_block_hash {
                        return true;
                    }
                    cursor = block.parent_hash;
                    steps += 1;
                    if steps > max_replay_depth {
                        return true;
                    }
                }
            })
            .count();
        if orphaned > 0 {
            return Err(Gov5QmdbStateRootError::Persistence(format!(
                "{orphaned} retained QMDB blocks have missing or cyclic ancestry"
            )));
        }
        tracing::warn!(
            target: "n42::qmdb",
            dropped = unreachable.len(),
            "dropping retained QMDB blocks below the current base"
        );
        for hash in unreachable {
            blocks.remove(&hash);
        }
    }
    Ok(())
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
    /// The chain's fork schedule: a Prague block writes the system caller's
    /// leaf (see [`crate::qmdb_state::with_gov5_prague_system_caller`]).
    chain_spec: Option<Arc<reth_chainspec::ChainSpec>>,
}

impl Gov5QmdbStateRootStrategy {
    pub const fn new(store: Arc<Gov5QmdbStateRootStore>) -> Self {
        Self {
            store,
            chain_spec: None,
        }
    }

    /// Decides Prague per block; without a chain spec no block is Prague.
    pub fn with_chain_spec(mut self, chain_spec: Arc<reth_chainspec::ChainSpec>) -> Self {
        self.chain_spec = Some(chain_spec);
        self
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
                chain_spec: self.chain_spec.clone(),
            }),
            None,
        ))
    }
}

#[derive(Debug)]
struct Gov5QmdbStateRootJob {
    store: Arc<Gov5QmdbStateRootStore>,
    chain_spec: Option<Arc<reth_chainspec::ChainSpec>>,
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
        // gov5 rewrites every slot the block's journal marked dirty, including
        // one changed and restored within the block, which revm drops from
        // the bundle; the executor wrapper recorded those for this block.
        let restored =
            n42_execution::restored_slots_for(gov5_restored_slots_key(block)).unwrap_or_default();
        let mut operations = gov5_qmdb_operations_with_restored(&output.state, &restored);
        if self.chain_spec.as_ref().is_some_and(|chain_spec| {
            reth_chainspec::EthereumHardforks::is_prague_active_at_timestamp(
                chain_spec.as_ref(),
                block.timestamp,
            )
        }) {
            with_gov5_prague_system_caller(&mut operations);
        }
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
/// Accepts the proposer's state root without recomputing it.
///
/// Used only for a member whose QMDB forest cannot be rebuilt locally yet
/// (chain 94's 63 million-slot log is larger than the portable snapshot
/// format carries). Transactions, receipts, gas and rewards are still fully
/// executed and checked; the state root alone is taken on trust, and the
/// node says so at startup.
#[derive(Debug, Clone, Copy, Default)]
pub struct Gov5TrustedStateRootStrategy;

impl<P, Evm> StateRootStrategy<EthPrimitives, P, Evm> for Gov5TrustedStateRootStrategy
where
    Evm: ConfigureEvm<Primitives = EthPrimitives>,
{
    fn prepare(
        &self,
        _ctx: StateRootJobContext<'_, EthPrimitives, P, Evm>,
    ) -> ProviderResult<PreparedStateRootJob<EthPrimitives>> {
        Ok(PreparedStateRootJob::new(
            Box::new(Gov5TrustedStateRootJob),
            None,
        ))
    }
}

#[derive(Debug)]
struct Gov5TrustedStateRootJob;

impl StateRootJob<EthPrimitives> for Gov5TrustedStateRootJob {
    fn name(&self) -> &'static str {
        "gov5-trusted"
    }

    fn finish(
        &mut self,
        block: &RecoveredBlock<reth_ethereum_primitives::Block>,
        _output: Arc<BlockExecutionOutput<Receipt>>,
        _hashed_state: &LazyHashedPostState,
    ) -> ProviderResult<StateRootJobOutcome> {
        metrics::counter!("n42_qmdb_trusted_state_roots_total").increment(1);
        Ok(StateRootJobOutcome::new(
            block.state_root,
            Arc::new(TrieUpdates::default()),
        ))
    }
}

#[derive(Clone)]
pub struct N42EngineTreeValidatorBuilder {
    inner: BasicEngineValidatorBuilder<N42EngineValidatorBuilder>,
    qmdb_store: Option<Arc<Gov5QmdbStateRootStore>>,
    trusted_state_root: bool,
    eof_guard: bool,
}

impl std::fmt::Debug for N42EngineTreeValidatorBuilder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("N42EngineTreeValidatorBuilder")
            .field("inner", &self.inner)
            .field("has_qmdb_store", &self.qmdb_store.is_some())
            .field("trusted_state_root", &self.trusted_state_root)
            .field("eof_guard", &self.eof_guard)
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
            trusted_state_root: false,
            eof_guard: false,
        }
    }

    /// Refuse every executed block that carries EOF code; see [`crate::eof_guard`].
    /// Only the gov5 strategies are wrapped: reth's default job carries execution
    /// hooks a wrapper cannot re-attach, and on a standard chain reth's own rules apply.
    pub const fn with_eof_guard(mut self, enabled: bool) -> Self {
        self.eof_guard = enabled;
        self
    }

    /// Take proposers' state roots on trust; see [`Gov5TrustedStateRootStrategy`].
    pub const fn with_trusted_state_root(mut self, trusted: bool) -> Self {
        self.trusted_state_root = trusted;
        self
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
        overlay_manager: OverlayManager<EthPrimitives>,
    ) -> eyre::Result<Self::EngineValidator> {
        let validator = self
            .inner
            .build_tree_validator(ctx, tree_config, overlay_manager)
            .await?;
        let guard = |strategy: Arc<dyn StateRootStrategy<EthPrimitives, Node::Provider, Node::Evm>>| -> Arc<dyn StateRootStrategy<EthPrimitives, Node::Provider, Node::Evm>> {
            if self.eof_guard {
                Arc::new(crate::eof_guard::EofGuardedStateRootStrategy::new(strategy))
            } else {
                strategy
            }
        };
        let Some(store) = self.qmdb_store else {
            if self.trusted_state_root {
                return Ok(validator
                    .with_state_root_strategy(guard(Arc::new(Gov5TrustedStateRootStrategy))));
            }
            if self.eof_guard {
                tracing::warn!(
                    target: "n42::eof_guard",
                    "EOF guard requested without a gov5 state-root strategy: imported blocks are not screened for EOF code"
                );
            }
            return Ok(validator);
        };
        let strategy =
            Gov5QmdbStateRootStrategy::new(store).with_chain_spec(ctx.config.chain.clone());
        Ok(validator.with_state_root_strategy(guard(Arc::new(strategy))))
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
    fn production_default_exceeds_legacy_65536_boundary() {
        const {
            assert!(DEFAULT_QMDB_REPLAY_DEPTH > 65_537);
        }
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

    #[test]
    fn persistent_empty_chain_uses_linear_wal_and_recovers_torn_tail() {
        let (store, snapshot) = store();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty-chain.bin");
        let persistent = Gov5QmdbStateRootStore::persistent(
            store.base_block_hash(),
            store.base_root(),
            snapshot.clone(),
            512,
            path.clone(),
        )
        .unwrap();
        let mut parent = persistent.base_block_hash();
        for number in 1u64..=256 {
            let mut hash = [0u8; 32];
            hash[..8].copy_from_slice(&number.to_le_bytes());
            let hash = B256::from(hash);
            assert_eq!(
                persistent
                    .compute_and_commit(parent, hash, store.base_root(), Vec::new())
                    .unwrap(),
                store.base_root()
            );
            parent = hash;
        }

        let wal = wal_path(&path);
        let valid_len = std::fs::metadata(&wal).unwrap().len();
        assert!(valid_len < 64 * 1024, "WAL unexpectedly large: {valid_len}");
        OpenOptions::new()
            .append(true)
            .open(&wal)
            .unwrap()
            .write_all(&[0xaa, 0xbb, 0xcc])
            .unwrap();

        let reopened = Gov5QmdbStateRootStore::persistent(
            store.base_block_hash(),
            store.base_root(),
            snapshot,
            512,
            path,
        )
        .unwrap();
        assert_eq!(reopened.root_for(parent).unwrap(), Some(store.base_root()));
        assert_eq!(std::fs::metadata(wal).unwrap().len(), valid_len);
    }

    /// The cached tip is an accelerator, so every root it produces must equal
    /// what a cold store computes from the base snapshot for the same blocks.
    #[test]
    fn cached_tip_reproduces_cold_reconstruction() {
        let (warm, snapshot) = store();
        let (cold, _) = store();
        let mut parent = warm.base_block_hash();
        let mut all_ops = Vec::new();

        for block in 0u8..24 {
            let block_hash = B256::repeat_byte(0x30 + block);
            let operations = vec![operation(block, block.wrapping_mul(3))];
            all_ops.push(operations.clone());
            let expected = expected_root(&snapshot, &all_ops);

            // The warm store carries a cached tip from the previous iteration.
            assert_eq!(
                warm.compute_and_commit(parent, block_hash, expected, operations.clone())
                    .unwrap(),
                expected
            );
            // The cold one is rebuilt each time, so it never has a usable cache.
            let (fresh, _) = store();
            for (i, ops) in all_ops.iter().enumerate() {
                let hash = B256::repeat_byte(0x30 + i as u8);
                let prev = if i == 0 {
                    cold.base_block_hash()
                } else {
                    B256::repeat_byte(0x30 + i as u8 - 1)
                };
                fresh
                    .compute_and_commit(
                        prev,
                        hash,
                        expected_root(&snapshot, &all_ops[..=i]),
                        ops.clone(),
                    )
                    .unwrap();
            }
            assert_eq!(
                warm.snapshot_for(block_hash).unwrap(),
                fresh.snapshot_for(block_hash).unwrap(),
                "cached and cold reconstructions diverged at block {block}"
            );
            parent = block_hash;
        }
    }

    /// A fork off an older block cannot resume from a cached tip on the other
    /// branch, and must still produce that fork's own root.
    #[test]
    fn cached_tip_does_not_leak_across_branches() {
        let (store_, snapshot) = store();
        let base = store_.base_block_hash();

        let a_ops = vec![operation(1, 11)];
        let a_hash = B256::repeat_byte(0x41);
        let a_root = expected_root(&snapshot, std::slice::from_ref(&a_ops));
        store_
            .compute_and_commit(base, a_hash, a_root, a_ops.clone())
            .unwrap();

        // Extends A, so the cache now describes A's descendant.
        let a2_ops = vec![operation(2, 22)];
        let a2_hash = B256::repeat_byte(0x42);
        let a2_root = expected_root(&snapshot, &[a_ops, a2_ops.clone()]);
        store_
            .compute_and_commit(a_hash, a2_hash, a2_root, a2_ops)
            .unwrap();

        // Sibling of A off the base: the cached tip is not an ancestor.
        let b_ops = vec![operation(9, 99)];
        let b_hash = B256::repeat_byte(0x4B);
        let b_root = expected_root(&snapshot, std::slice::from_ref(&b_ops));
        assert_eq!(
            store_
                .compute_and_commit(base, b_hash, b_root, b_ops)
                .unwrap(),
            b_root
        );
        assert_ne!(b_root, a2_root);
        assert_eq!(store_.root_for(a2_hash).unwrap(), Some(a2_root));
    }

    /// Resuming from the cache shortens the ancestry walk, so the bound has to
    /// be measured from the base rather than from where the walk started.
    /// Pins that by rejecting at exactly the same block a cache-less store
    /// rejects at — the boundary itself, not a hardcoded guess about it.
    #[test]
    fn cached_tip_rejects_at_the_same_depth_as_a_cold_store() {
        const MAX_DEPTH: usize = 3;

        fn bounded_store(snapshot: &QmdbSnapshot, root: [u8; 32]) -> Gov5QmdbStateRootStore {
            Gov5QmdbStateRootStore::with_max_replay_depth(
                B256::repeat_byte(0x10),
                B256::from(root),
                snapshot.clone(),
                MAX_DEPTH,
            )
            .unwrap()
        }

        let mut base = QmdbCompatTree::new();
        base.set([1; 32], vec![1]);
        let snapshot = base.snapshot();
        let base_root = base.root();

        let warm = bounded_store(&snapshot, base_root);
        let mut parent = warm.base_block_hash();
        let mut all_ops: Vec<Vec<QmdbOperation>> = Vec::new();
        let mut first_rejected = None;

        for block in 0u8..8 {
            let operations = vec![operation(block, block)];
            all_ops.push(operations.clone());
            let hash = B256::repeat_byte(0x50 + block);
            let root = expected_root(&snapshot, &all_ops);

            // The warm store has a cached tip from the previous block; the cold
            // one is built from scratch and replays the whole ancestry.
            let cold = bounded_store(&snapshot, base_root);
            let mut cold_parent = cold.base_block_hash();
            let mut cold_result = None;
            for (i, ops) in all_ops.iter().enumerate() {
                let h = B256::repeat_byte(0x50 + i as u8);
                cold_result = Some(cold.compute_and_commit(
                    cold_parent,
                    h,
                    expected_root(&snapshot, &all_ops[..=i]),
                    ops.clone(),
                ));
                cold_parent = h;
            }

            let warm_result = warm.compute_and_commit(parent, hash, root, operations);
            let warm_depth_error = matches!(
                warm_result,
                Err(Gov5QmdbStateRootError::ReplayDepthExceeded(MAX_DEPTH))
            );
            let cold_depth_error = matches!(
                cold_result,
                Some(Err(Gov5QmdbStateRootError::ReplayDepthExceeded(MAX_DEPTH)))
            );
            assert_eq!(
                warm_depth_error, cold_depth_error,
                "cached and cold stores disagreed about the depth bound at block {block}"
            );
            if warm_depth_error {
                first_rejected = Some(block);
                break;
            }
            all_ops.truncate(usize::from(block) + 1);
            parent = hash;
        }

        assert!(
            first_rejected.is_some(),
            "the bound never triggered, so the test proved nothing"
        );
    }

    /// Measures how QMDB import and archive reads scale with chain depth.
    ///
    /// `compute_and_commit` takes the `compute_candidate_locked` fast path only
    /// for empty blocks. Any block with operations falls through to
    /// `reconstruct_tree_locked`, which replays the entire lineage from the
    /// base snapshot — so importing block N replays N-1 blocks' operations, and
    /// the whole chain costs O(n^2). `snapshot_for`/`proof_for` do the same
    /// replay while holding the mutex the import path needs.
    ///
    ///   cargo test -p n42-node --release qmdb_replay_scaling --     ///     --ignored --nocapture
    ///
    /// Read the per-block import number against the 8s slot. The growth rate
    /// between depths matters more than any single value: if it is linear in
    /// depth, the store needs a base-checkpoint fold before a long run, and
    /// the depth at which import crosses the slot budget is the deadline.
    #[test]
    #[ignore = "measurement, not a correctness gate"]
    fn qmdb_replay_scaling_by_depth() {
        // 32 keys per block, roughly a small real block's write footprint.
        const OPS_PER_BLOCK: usize = 32;

        fn block_ops(block: usize) -> Vec<QmdbOperation> {
            (0..OPS_PER_BLOCK)
                .map(|k| {
                    let mut key = [0u8; 32];
                    key[..8].copy_from_slice(&((block * OPS_PER_BLOCK + k) as u64).to_le_bytes());
                    QmdbOperation {
                        key,
                        value: Some(vec![(block % 251) as u8; 32]),
                    }
                })
                .collect()
        }

        for depth in [300usize, 600, 1_200] {
            let (store, _snapshot) = store();
            let mut parent = store.base_block_hash();
            let mut tip = parent;
            let mut last_import = std::time::Duration::ZERO;
            let total_start = std::time::Instant::now();

            for block in 0..depth {
                let block_hash = B256::from(blake3::hash(&block.to_le_bytes()).as_bytes());
                let operations = block_ops(block);
                let root = store.compute_candidate(parent, &operations).unwrap();
                let start = std::time::Instant::now();
                store
                    .compute_and_commit(parent, block_hash, root, operations)
                    .unwrap();
                last_import = start.elapsed();
                parent = block_hash;
                tip = block_hash;
            }
            let build_secs = total_start.elapsed().as_secs_f64();

            let start = std::time::Instant::now();
            assert!(store.snapshot_for(tip).unwrap().is_some());
            let archive_ms = start.elapsed().as_secs_f64() * 1000.0;

            println!(
                "depth {depth}: import of the tip block {:.0}ms ({:.2}% of an 8s slot),                  archive snapshot_for {archive_ms:.0}ms holding the import mutex,                  building the chain took {build_secs:.1}s",
                last_import.as_secs_f64() * 1000.0,
                last_import.as_secs_f64() / 8.0 * 100.0,
            );
        }
    }
    fn persistent_store(
        base: &Gov5QmdbStateRootStore,
        snapshot: &QmdbSnapshot,
        path: &Path,
    ) -> Gov5QmdbStateRootStore {
        Gov5QmdbStateRootStore::persistent(
            base.base_block_hash(),
            base.base_root(),
            snapshot.clone(),
            64,
            path.to_path_buf(),
        )
        .unwrap()
    }

    #[test]
    fn borrowed_wal_record_encodes_identically_to_the_owned_record() {
        let block = StoredQmdbBlock {
            parent_hash: B256::repeat_byte(0x31),
            root: B256::repeat_byte(0x32),
            operations: vec![operation(2, 2), operation(3, 0)],
        };
        let block_hash = B256::repeat_byte(0x33);
        let owned = bincode::serialize(&PersistedQmdbWalRecord {
            block_hash,
            block: block.clone(),
        })
        .unwrap();
        let borrowed = bincode::serialize(&PersistedQmdbWalRecordRef {
            block_hash,
            block: &block,
        })
        .unwrap();
        assert_eq!(owned, borrowed);
        let decoded: PersistedQmdbWalRecord = bincode::deserialize(&borrowed).unwrap();
        assert_eq!(decoded.block_hash, block_hash);
        assert_eq!(decoded.block, block);
    }

    /// A block whose WAL append fails must not be committed: it leaves the
    /// in-memory graph, the cached tip and the file exactly as they were, and
    /// a store reopened from disk does not know it. A retry then succeeds.
    #[test]
    fn wal_append_failure_rolls_back_the_insert() {
        let (base, snapshot) = store();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wal-failure.bin");
        let persistent = persistent_store(&base, &snapshot, &path);

        let first_ops = vec![operation(2, 2)];
        let first = B256::repeat_byte(0x21);
        let first_root = expected_root(&snapshot, std::slice::from_ref(&first_ops));
        persistent
            .compute_and_commit(base.base_block_hash(), first, first_root, first_ops.clone())
            .unwrap();
        let wal = wal_path(&path);
        let durable_len = std::fs::metadata(&wal).unwrap().len();

        let second_ops = vec![operation(3, 3)];
        let second = B256::repeat_byte(0x22);
        let second_root = expected_root(&snapshot, &[first_ops.clone(), second_ops.clone()]);
        persistent.inject_wal_fault(WalFault::FailWrite);
        let error = persistent
            .compute_and_commit(first, second, second_root, second_ops.clone())
            .unwrap_err();
        assert!(
            matches!(error, Gov5QmdbStateRootError::Persistence(_)),
            "unexpected error: {error:?}"
        );

        assert!(!persistent.contains(second).unwrap());
        assert_eq!(persistent.root_for(second).unwrap(), None);
        assert_eq!(persistent.parent_for(second).unwrap(), None);
        assert_eq!(persistent.retained_block_count().unwrap(), 1);
        assert!(!persistent.wal_in_flight());
        {
            let state = persistent.state.lock().unwrap();
            assert!(!state.blocks.contains_key(&second));
            assert_eq!(
                state.tree_at, first,
                "the tree must step back to the last durable block"
            );
        }
        assert_eq!(
            std::fs::metadata(&wal).unwrap().len(),
            durable_len,
            "the torn frame must be rolled back on disk"
        );
        let reopened = persistent_store(&base, &snapshot, &path);
        assert_eq!(reopened.root_for(first).unwrap(), Some(first_root));
        assert_eq!(reopened.root_for(second).unwrap(), None);
        drop(reopened);

        assert_eq!(
            persistent
                .compute_and_commit(first, second, second_root, second_ops)
                .unwrap(),
            second_root
        );
        assert!(persistent.contains(second).unwrap());
        drop(persistent);
        let reopened = persistent_store(&base, &snapshot, &path);
        assert_eq!(reopened.root_for(second).unwrap(), Some(second_root));
    }

    /// When the torn frame cannot be truncated, `len` no longer describes the
    /// file. The store must refuse further commits rather than append behind
    /// the garbage, and a reopen recovers the durable prefix from disk.
    #[test]
    fn wal_rollback_failure_poisons_the_handle_until_reopen() {
        let (base, snapshot) = store();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wal-poison.bin");
        let persistent = persistent_store(&base, &snapshot, &path);

        let first_ops = vec![operation(2, 2)];
        let first = B256::repeat_byte(0x21);
        let first_root = expected_root(&snapshot, std::slice::from_ref(&first_ops));
        persistent
            .compute_and_commit(base.base_block_hash(), first, first_root, first_ops.clone())
            .unwrap();
        let wal = wal_path(&path);
        let durable_len = std::fs::metadata(&wal).unwrap().len();

        let second_ops = vec![operation(3, 3)];
        let second = B256::repeat_byte(0x22);
        let second_root = expected_root(&snapshot, &[first_ops.clone(), second_ops.clone()]);
        persistent.inject_wal_fault(WalFault::FailWriteAndRollback);
        let error = persistent
            .compute_and_commit(first, second, second_root, second_ops.clone())
            .unwrap_err();
        assert!(
            matches!(&error, Gov5QmdbStateRootError::Persistence(reason) if reason.contains("rollback")),
            "unexpected error: {error:?}"
        );
        assert!(!persistent.contains(second).unwrap());
        assert!(!persistent.wal_in_flight());
        let torn_len = std::fs::metadata(&wal).unwrap().len();
        assert!(torn_len > durable_len, "the torn half-frame stays on disk");

        // Every later commit is refused without touching the file; reads and
        // candidates on the durable graph still work.
        let error = persistent
            .compute_and_commit(first, second, second_root, second_ops.clone())
            .unwrap_err();
        assert!(
            matches!(&error, Gov5QmdbStateRootError::Persistence(reason) if reason.contains("failed rollback")),
            "unexpected error: {error:?}"
        );
        assert!(!persistent.contains(second).unwrap());
        assert_eq!(std::fs::metadata(&wal).unwrap().len(), torn_len);
        assert_eq!(persistent.root_for(first).unwrap(), Some(first_root));
        assert_eq!(
            persistent.compute_candidate(first, &second_ops).unwrap(),
            second_root
        );
        drop(persistent);

        // Reopen: recovery truncates the torn tail, keeps the durable prefix,
        // and commits resume.
        let reopened = persistent_store(&base, &snapshot, &path);
        assert_eq!(std::fs::metadata(&wal).unwrap().len(), durable_len);
        assert_eq!(reopened.root_for(first).unwrap(), Some(first_root));
        assert_eq!(reopened.root_for(second).unwrap(), None);
        assert_eq!(
            reopened
                .compute_and_commit(first, second, second_root, second_ops)
                .unwrap(),
            second_root
        );
        drop(reopened);
        let reopened = persistent_store(&base, &snapshot, &path);
        assert_eq!(reopened.root_for(second).unwrap(), Some(second_root));
    }

    /// The WAL write and fsync run with the `state` lock released: while a
    /// commit is stalled inside its append, a candidate on top of the pending
    /// block and archive reads of durable blocks proceed immediately, and the
    /// pending block stays invisible to archive readers until it is durable.
    #[test]
    fn wal_append_does_not_hold_the_state_lock() {
        const DELAY: std::time::Duration = std::time::Duration::from_millis(400);

        let (base, snapshot) = store();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wal-delay.bin");
        let persistent = Arc::new(persistent_store(&base, &snapshot, &path));

        let first_ops = vec![operation(2, 2)];
        let first = B256::repeat_byte(0x21);
        let first_root = expected_root(&snapshot, std::slice::from_ref(&first_ops));
        persistent
            .compute_and_commit(base.base_block_hash(), first, first_root, first_ops.clone())
            .unwrap();

        let second_ops = vec![operation(3, 3)];
        let second = B256::repeat_byte(0x22);
        let second_root = expected_root(&snapshot, &[first_ops.clone(), second_ops.clone()]);
        persistent.inject_wal_fault(WalFault::Delay(DELAY));
        let committer = Arc::clone(&persistent);
        let commit_started = std::time::Instant::now();
        let commit = std::thread::spawn(move || {
            committer.compute_and_commit(first, second, second_root, second_ops)
        });
        let wait_started = std::time::Instant::now();
        while !persistent.wal_in_flight() {
            assert!(
                wait_started.elapsed() < std::time::Duration::from_secs(5),
                "commit never reached its WAL append"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }

        let third_ops = vec![operation(4, 4)];
        let reads_started = std::time::Instant::now();
        let candidate = persistent.compute_candidate(second, &third_ops).unwrap();
        assert!(!persistent.contains(second).unwrap());
        assert_eq!(persistent.root_for(second).unwrap(), None);
        assert_eq!(persistent.root_for(first).unwrap(), Some(first_root));
        assert!(persistent.snapshot_for(first).unwrap().is_some());
        let reads_elapsed = reads_started.elapsed();
        assert!(
            reads_elapsed < DELAY / 4,
            "reads were blocked behind the WAL append for {reads_elapsed:?}"
        );

        assert_eq!(commit.join().unwrap().unwrap(), second_root);
        assert!(
            commit_started.elapsed() >= DELAY,
            "the injected stall must have been inside the commit"
        );
        assert!(persistent.contains(second).unwrap());
        assert_eq!(persistent.root_for(second).unwrap(), Some(second_root));
        assert_eq!(
            candidate,
            expected_root(&snapshot, &[first_ops, vec![operation(3, 3)], third_ops]),
            "a candidate built on the in-flight block must equal the cold reconstruction"
        );
    }

    #[test]
    fn tip_cache_metric_counts_resumed_and_cold_reconstructions() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);

        let (store, snapshot) = store();
        let first_ops = vec![operation(2, 2)];
        let first = B256::repeat_byte(0x21);
        let first_root = expected_root(&snapshot, std::slice::from_ref(&first_ops));
        // Hit: the tree stands at the base, which is the parent.
        store
            .compute_and_commit(
                store.base_block_hash(),
                first,
                first_root,
                first_ops.clone(),
            )
            .unwrap();
        // Hit: the candidate builds on the committed tip the tree stands at.
        store.compute_candidate(first, &[operation(3, 3)]).unwrap();
        // Miss: a sibling branch off the base makes the tree step back.
        store
            .compute_candidate(store.base_block_hash(), &[operation(4, 4)])
            .unwrap();

        let outcomes: HashMap<String, u64> = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .filter(|(key, _, _, _)| key.key().name() == "n42_qmdb_tip_cache_total")
            .map(|(key, _, _, value)| {
                let outcome = key
                    .key()
                    .labels()
                    .find(|label| label.key() == "outcome")
                    .map(|label| label.value().to_string())
                    .unwrap();
                let DebugValue::Counter(count) = value else {
                    panic!("counter expected");
                };
                (outcome, count)
            })
            .collect();
        assert_eq!(outcomes.get("hit"), Some(&2));
        assert_eq!(outcomes.get("miss"), Some(&1));
    }

    /// The in-place tree walks to any retained block and back: siblings,
    /// deeper forks, and a return to the base, each time with the root the
    /// cold reconstruction gives, and it never forgets its way back.
    #[test]
    fn the_tree_walks_the_graph_in_place_and_returns_to_the_base() {
        let (store, snapshot) = store();
        let base = store.base_block_hash();
        let a_ops = vec![operation(2, 2), operation(3, 3)];
        let b_ops = vec![operation(2, 9), operation(4, 4)];
        let c_ops = vec![operation(3, 7)];
        let a = B256::repeat_byte(0xA1);
        let b = B256::repeat_byte(0xB1);
        let c = B256::repeat_byte(0xC1);
        let a_root = expected_root(&snapshot, std::slice::from_ref(&a_ops));
        let b_root = expected_root(&snapshot, &[a_ops.clone(), b_ops.clone()]);
        let c_root = expected_root(&snapshot, &[a_ops.clone(), c_ops.clone()]);
        store
            .compute_and_commit(base, a, a_root, a_ops.clone())
            .unwrap();
        store
            .compute_and_commit(a, b, b_root, b_ops.clone())
            .unwrap();
        assert_eq!(store.tree_position(), (b, 2, 2));
        // A sibling of b: the tree steps back to a, prices, and stays at a.
        assert_eq!(store.compute_candidate(a, &c_ops).unwrap(), c_root);
        assert_eq!(store.tree_position(), (a, 1, 1));
        store
            .compute_and_commit(a, c, c_root, c_ops.clone())
            .unwrap();
        assert_eq!(store.tree_position(), (c, 2, 2));
        // Back to b's branch (revert c, replay b) and to the base.
        assert_eq!(
            store.compute_candidate(b, &[operation(5, 5)]).unwrap(),
            expected_root(
                &snapshot,
                &[a_ops.clone(), b_ops.clone(), vec![operation(5, 5)]]
            )
        );
        assert_eq!(store.tree_position(), (b, 2, 2));
        assert_eq!(
            store.compute_candidate(base, &[operation(6, 6)]).unwrap(),
            expected_root(&snapshot, &[vec![operation(6, 6)]])
        );
        assert_eq!(store.tree_position(), (base, 0, 0));
        // And forward again to c, replaying two retained blocks.
        assert_eq!(store.compute_candidate(c, &[]).unwrap(), c_root);
        assert_eq!(store.tree_position(), (c, 2, 2));
        assert_eq!(store.root_for(b).unwrap(), Some(b_root));
    }

    /// A base file written from the tree restores to the same root and
    /// carries the block it was taken at.
    #[test]
    fn a_base_file_round_trips_the_tree_at_the_committed_tip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("branches.bin");
        let (_, snapshot) = store();
        let base = B256::repeat_byte(0x10);
        let base_root = B256::from(QmdbCompatTree::from_snapshot(&snapshot).unwrap().root());
        let store = Gov5QmdbStateRootStore::persistent(
            base,
            base_root,
            snapshot.clone(),
            DEFAULT_QMDB_REPLAY_DEPTH,
            path.clone(),
        )
        .unwrap()
        .with_identity(QmdbBaseIdentity {
            chain_id: 94,
            genesis_hash: B256::repeat_byte(0x42),
            block_number: 100,
        });
        let ops = vec![operation(2, 2)];
        let first = B256::repeat_byte(0x21);
        let first_root = expected_root(&snapshot, std::slice::from_ref(&ops));
        store
            .compute_and_commit(base, first, first_root, ops)
            .unwrap();
        assert_eq!(store.write_base_file().unwrap(), Some((first, 101)));
        let (mut tree, header) =
            Gov5QmdbStateRootStore::read_base_file(&base_file_path(&path)).unwrap();
        assert_eq!(B256::from(tree.root()), first_root);
        assert_eq!(header.block_number, 101);
        assert_eq!(B256::from(header.block_hash), first);
        assert_eq!(header.chain_id, 94);
        // A store opened on that base checks the root and starts there.
        let reopened = Gov5QmdbStateRootStore::from_leaf_tree(
            first,
            first_root,
            tree,
            DEFAULT_QMDB_REPLAY_DEPTH,
        )
        .unwrap();
        assert_eq!(reopened.base_root(), first_root);
        assert_eq!(
            reopened
                .compute_candidate(first, &[operation(3, 3)])
                .unwrap(),
            expected_root(&snapshot, &[vec![operation(2, 2)], vec![operation(3, 3)]])
        );
    }

    /// Measurement, not a gate: how long a commit holds the shared `state`
    /// mutex versus how long its WAL append and fsync take. Run with
    /// `--ignored --nocapture` on a persistent store so the WAL path is live.
    #[test]
    #[ignore = "measurement, not a correctness gate"]
    fn qmdb_commit_lock_hold_vs_wal_append() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};

        const BLOCKS: usize = 200;
        const OPS_PER_BLOCK: usize = 32;

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);

        let (base, snapshot) = store();
        let dir = tempfile::tempdir().unwrap();
        let persistent = Gov5QmdbStateRootStore::persistent(
            base.base_block_hash(),
            base.base_root(),
            snapshot,
            DEFAULT_QMDB_REPLAY_DEPTH,
            dir.path().join("qmdb-lock-hold.bin"),
        )
        .unwrap();
        let mut parent = persistent.base_block_hash();
        let started = std::time::Instant::now();
        for block in 0..BLOCKS {
            let operations: Vec<QmdbOperation> = (0..OPS_PER_BLOCK)
                .map(|k| {
                    let mut key = [0u8; 32];
                    key[..8].copy_from_slice(&((block * OPS_PER_BLOCK + k) as u64).to_le_bytes());
                    QmdbOperation {
                        key,
                        value: Some(vec![(block % 251) as u8; 32]),
                    }
                })
                .collect();
            let block_hash = B256::from(blake3::hash(&block.to_le_bytes()).as_bytes());
            let root = persistent.compute_candidate(parent, &operations).unwrap();
            persistent
                .compute_and_commit(parent, block_hash, root, operations)
                .unwrap();
            parent = block_hash;
        }
        let total_ms = started.elapsed().as_secs_f64() * 1000.0;

        let mut rows = Vec::new();
        for (key, _, _, value) in snapshotter.snapshot().into_vec() {
            let DebugValue::Histogram(samples) = value else {
                continue;
            };
            let name = key.key().name().to_string();
            let labels: Vec<String> = key
                .key()
                .labels()
                .map(|label| format!("{}={}", label.key(), label.value()))
                .collect();
            let mut sorted: Vec<f64> = samples.into_iter().map(|v| v.into_inner()).collect();
            sorted.sort_by(|a, b| a.total_cmp(b));
            let n = sorted.len();
            let mean = sorted.iter().sum::<f64>() / n.max(1) as f64;
            let p50 = sorted[n / 2];
            let p99 = sorted[(n * 99 / 100).min(n - 1)];
            let max = sorted[n - 1];
            rows.push(format!(
                "{name}{{{}}}: n={n} mean={mean:.3}ms p50={p50:.3}ms p99={p99:.3}ms max={max:.3}ms",
                labels.join(",")
            ));
        }
        rows.sort();
        println!(
            "qmdb commit lock-hold measurement: {BLOCKS} blocks x {OPS_PER_BLOCK} ops in {total_ms:.1}ms"
        );
        for row in rows {
            println!("  {row}");
        }
    }
}
