use alloy_primitives::B256;
use n42_twig_core::qmdb_compat::{BlockUndo, QmdbCompatTree, QmdbOperation, QmdbOperationError, QmdbProof, QmdbSnapshot, QmdbSnapshotError, QmdbUndoError};
use n42_twig_core::qmdb_leaf_tree::{QmdbLeafFormHeader, QmdbLeafFormIoError, QmdbLeafTree, QmdbLeafTreeError};
use serde::{Deserialize, Serialize};
use std::{collections::{HashMap,HashSet,VecDeque},fs::OpenOptions,io::Write,path::{Path,PathBuf},sync::{Arc,Mutex,RwLock,atomic::{AtomicBool,AtomicU64,Ordering}}};
use crate::qmdb_read_view::QmdbReadView;
/// Maximum ancestry replay accepted by the bounded interoperability strategy.
///
/// The production Gov5 bridge retains an authenticated lineage from its
/// bootstrap checkpoint. The previous participant default (65,536, duplicated
/// in the CLI) made a healthy node deterministically fail at block 65,538.
/// Keep one shared default with enough runway for qualification and production
/// replacement windows. Operators can still set a smaller explicit bound for
/// fail-closed testing or a larger audited bound for longer archive horizons.
pub const DEFAULT_QMDB_REPLAY_DEPTH: usize = 1_048_576;

/// Upper bound on hot undo records. The byte budget and retained leaf heaps
/// can shorten this window; older branches rebuild from the authenticated base.
const RETAINED_UNDO_RECORDS: usize = 8_192;
const RETAINED_UNDO_BYTES: usize = 256 * 1024 * 1024;
const RESIDENT_OPERATION_BYTES: usize = 128 * 1024 * 1024;
const PREPARED_CANDIDATE_BYTES: usize = 64 * 1024 * 1024;

/// Committed blocks between two background rewrites of the base file.
pub const DEFAULT_QMDB_REBASE_BLOCKS: u64 = 20_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoredQmdbBlock {
    parent_hash: B256,
    root: B256,
    operations: Vec<QmdbOperation>,
}

/// A locator into the immutable, durable WAL prefix. Future WAL compaction
/// must replace these locators before removing or rewriting that prefix.
#[derive(Debug, Clone)]
struct QmdbWalLocation {
    path: Arc<PathBuf>,
    offset: u64,
    payload_len: u32,
    checksum: [u8; 32],
}

#[derive(Debug, Clone)]
struct IndexedQmdbBlock {
    parent_hash: B256,
    root: B256,
    operations: Option<Vec<QmdbOperation>>,
    wal: Option<QmdbWalLocation>,
}

impl IndexedQmdbBlock {
    fn resident(block: StoredQmdbBlock) -> Self {
        Self {
            parent_hash: block.parent_hash,
            root: block.root,
            operations: Some(block.operations),
            wal: None,
        }
    }

    fn resident_bytes(&self) -> usize {
        self.operations.as_ref().map_or(0, |ops| {
            ops.capacity() * std::mem::size_of::<QmdbOperation>()
                + ops
                    .iter()
                    .filter_map(|op| op.value.as_ref())
                    .map(Vec::capacity)
                    .sum::<usize>()
        })
    }

    fn operations(
        &self,
        hash: B256,
    ) -> Result<std::borrow::Cow<'_, [QmdbOperation]>, Gov5QmdbStateRootError> {
        if let Some(operations) = &self.operations {
            return Ok(std::borrow::Cow::Borrowed(operations));
        }
        use std::io::{Read, Seek, SeekFrom};
        let source = self.wal.as_ref().ok_or_else(|| {
            Gov5QmdbStateRootError::Persistence("QMDB operations have no backing record".into())
        })?;
        let read = || -> Result<StoredQmdbBlock, Gov5QmdbStateRootError> {
            let mut file = std::fs::File::open(source.path.as_ref())
                .map_err(|error| Gov5QmdbStateRootError::Persistence(error.to_string()))?;
            file.seek(SeekFrom::Start(source.offset))
                .map_err(|error| Gov5QmdbStateRootError::Persistence(error.to_string()))?;
            let mut length = [0; 4];
            file.read_exact(&mut length)
                .map_err(|error| Gov5QmdbStateRootError::Persistence(error.to_string()))?;
            if u32::from_le_bytes(length) != source.payload_len
                || source.payload_len as usize > QMDB_WAL_MAX_RECORD_BYTES
            {
                return Err(Gov5QmdbStateRootError::Persistence(
                    "QMDB cold record length changed".into(),
                ));
            }
            let mut payload = vec![0; source.payload_len as usize];
            let mut checksum = [0; 32];
            file.read_exact(&mut payload)
                .and_then(|()| file.read_exact(&mut checksum))
                .map_err(|error| Gov5QmdbStateRootError::Persistence(error.to_string()))?;
            if checksum != source.checksum || blake3::hash(&payload).as_bytes() != &source.checksum
            {
                return Err(Gov5QmdbStateRootError::Persistence(
                    "QMDB cold record checksum changed".into(),
                ));
            }
            let record: PersistedQmdbWalRecord = bincode::deserialize(&payload)
                .map_err(|error| Gov5QmdbStateRootError::Persistence(error.to_string()))?;
            if record.block_hash != hash
                || record.block.parent_hash != self.parent_hash
                || record.block.root != self.root
            {
                return Err(Gov5QmdbStateRootError::Persistence(
                    "QMDB cold record identity changed".into(),
                ));
            }
            Ok(record.block)
        };
        let started = std::time::Instant::now();
        let result = read();
        metrics::histogram!("n42_qmdb_cold_operations_read_ms")
            .record(started.elapsed().as_secs_f64() * 1000.0);
        metrics::counter!("n42_qmdb_cold_operations_reads_total", "outcome" => if result.is_ok() { "ok" } else { "error" }).increment(1);
        result.map(|block| std::borrow::Cow::Owned(block.operations))
    }

    fn materialize(&self, hash: B256) -> Result<StoredQmdbBlock, Gov5QmdbStateRootError> {
        Ok(StoredQmdbBlock {
            parent_hash: self.parent_hash,
            root: self.root,
            operations: self.operations(hash)?.into_owned(),
        })
    }
}

/// Execution bundles can enumerate the same mutations in different orders.
/// Compare their complete contents without copying account/storage values or
/// changing the on-disk order of records written by older versions.
fn same_operations(left: &[QmdbOperation], right: &[QmdbOperation]) -> bool {
    if left == right {
        return true;
    }
    if left.len() != right.len() {
        return false;
    }
    let mut left: Vec<_> = left.iter().collect();
    let mut right: Vec<_> = right.iter().collect();
    left.sort_unstable_by_key(|operation| operation.key);
    right.sort_unstable_by_key(|operation| operation.key);
    left == right
}

/// One undo record on the tree's path: the block it reverts and the block
/// the tree stands at after reverting it.
struct AppliedUndo {
    block_hash: B256,
    parent_hash: B256,
    undo: BlockUndo,
    heap_bytes: usize,
}

/// An unpublished tree calculation, never an execution result or durable block.
/// `tree_at` still names its parent until the exact operations are admitted.
struct PreparedCandidate {
    parent_hash: B256,
    root: B256,
    operations: Vec<QmdbOperation>,
    undo: BlockUndo,
}

fn undo_heap_bytes(undo: &BlockUndo) -> usize {
    undo.entries.capacity() * std::mem::size_of::<n42_twig_core::qmdb_compat::UndoEntry>()
        + undo.appended_keys.capacity() * std::mem::size_of::<[u8; 32]>()
        + undo
            .entries
            .iter()
            .map(|entry| entry.value.capacity())
            .sum::<usize>()
}

/// Immutable starting point for rebuilding beyond the hot undo window.
/// Persistent stores keep it on disk, independently of the rolling base file.
enum QmdbReplayBase {
    Memory(Box<QmdbLeafTree>),
    File(PathBuf),
}

impl std::fmt::Debug for QmdbReplayBase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Memory(tree) => f
                .debug_struct("MemoryBase")
                .field("live", &tree.len())
                .finish(),
            Self::File(path) => f.debug_tuple("FileBase").field(path).finish(),
        }
    }
}

// In-memory only: persistence goes through `PersistedQmdbBranchState` and the
// base file; the tree is a derived accelerator whose position is transient.
struct QmdbBranchState {
    blocks: HashMap<B256, IndexedQmdbBlock>,
    operations_bytes: usize,
    operations_budget: usize,
    operations_order: VecDeque<B256>,
    /// The one tree, moved in place along the graph.
    tree: QmdbLeafTree,
    /// The block the tree represents after undoing `prepared`, if present.
    /// It is always the base or a block in `blocks`.
    tree_at: B256,
    prepared: Option<PreparedCandidate>,
    /// Parent edges from the base to `tree_at`.
    tree_depth: usize,
    /// Undo records of the blocks applied to reach `tree_at`, oldest first.
    /// Popping the newest moves the tree to that block's parent.
    applied: VecDeque<AppliedUndo>,
    undo_heap_bytes: usize,
    undo_byte_budget: usize,
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
            .field("prepared", &self.prepared.is_some())
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

/// Test-only WAL fault injection, applied to the next encode or append.
#[cfg(test)]
#[derive(Clone, Debug)]
enum WalFault {
    /// Refuse a frame before it can enter the WAL or block graph.
    FailEncode,
    /// Stall inside the append, outside the `state` lock.
    Delay(std::time::Duration),
    /// Let a speculative candidate run before the pending append fails.
    WaitThenFail(Arc<std::sync::Barrier>),
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
    replay_base: QmdbReplayBase,
    /// Chain identity and base height, written into the base file's header.
    identity: QmdbBaseIdentity,
    max_replay_depth: usize,
    persistence_path: Option<PathBuf>,
    wal_read_path: Option<Arc<PathBuf>>,
    rebase_every: u64,
    commits_since_rebase: AtomicU64,
    rebase_in_flight: Arc<AtomicBool>,
    state: Mutex<QmdbBranchState>,
    /// Serializes commits end to end (tree work, insert, WAL append). WAL order
    /// then equals insertion order and a child can never be published while
    /// its parent's durability is still in flight.
    commit: Mutex<()>,
    wal: Mutex<Option<QmdbWalFile>>,
    /// Derived immutable execution views. Disabled until explicitly requested;
    /// pinned readers keep their version even after this cache evicts it.
    read_views: RwLock<Option<QmdbReadViewCache>>,
    #[cfg(test)]
    wal_fault: Mutex<Option<WalFault>>,
}

#[derive(Debug)]
struct QmdbReadViewCache {
    capacity: usize,
    byte_budget: usize,
    logical_bytes: usize,
    views: HashMap<B256, Arc<QmdbReadView>>,
    order: VecDeque<B256>,
}

impl QmdbReadViewCache {
    fn get(&mut self, hash: B256) -> Option<Arc<QmdbReadView>> {
        let view = self.views.get(&hash)?.clone();
        if self.order.back() != Some(&hash) {
            self.order.retain(|entry| *entry != hash);
            self.order.push_back(hash);
        }
        Some(view)
    }

    fn insert(&mut self, view: Arc<QmdbReadView>) {
        let hash = view.block_hash();
        self.logical_bytes += view.logical_bytes();
        if let Some(previous) = self.views.insert(hash, view) {
            self.logical_bytes -= previous.logical_bytes();
        } else {
            self.order.push_back(hash);
        }
        // Keep the newest view even when it alone exceeds the budget: dropping
        // it would force a full reconstruction for every execution provider.
        while self.order.len() > 1
            && (self.order.len() > self.capacity || self.logical_bytes > self.byte_budget)
        {
            if let Some(hash) = self.order.pop_front()
                && let Some(view) = self.views.remove(&hash)
            {
                self.logical_bytes -= view.logical_bytes();
            }
        }
        metrics::gauge!("n42_qmdb_read_cache_views").set(self.views.len() as f64);
        metrics::gauge!("n42_qmdb_read_cache_logical_bytes").set(self.logical_bytes as f64);
        metrics::gauge!("n42_qmdb_read_cache_over_budget_bytes")
            .set(self.logical_bytes.saturating_sub(self.byte_budget) as f64);
    }
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
    #[error("QMDB block {block_hash} conflicts with an already authenticated block identity")]
    ConflictingBlock { block_hash: B256 },
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
        tree: QmdbLeafTree,
        max_replay_depth: usize,
    ) -> Result<Self, Gov5QmdbStateRootError> {
        let replay_base = QmdbReplayBase::Memory(Box::new(tree.clone()));
        Self::from_leaf_tree_with_replay_base(
            base_block_hash,
            base_root,
            tree,
            max_replay_depth,
            replay_base,
        )
    }

    fn from_leaf_tree_with_replay_base(
        base_block_hash: B256,
        base_root: B256,
        mut tree: QmdbLeafTree,
        max_replay_depth: usize,
        replay_base: QmdbReplayBase,
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
            replay_base,
            identity: QmdbBaseIdentity {
                chain_id: 0,
                genesis_hash: B256::ZERO,
                block_number: 0,
            },
            max_replay_depth,
            persistence_path: None,
            wal_read_path: None,
            rebase_every: std::env::var("N42_QMDB_REBASE_BLOCKS")
                .ok()
                .and_then(|value| value.parse().ok())
                .filter(|value| *value > 0)
                .unwrap_or(DEFAULT_QMDB_REBASE_BLOCKS),
            commits_since_rebase: AtomicU64::new(0),
            rebase_in_flight: Arc::new(AtomicBool::new(false)),
            state: Mutex::new(QmdbBranchState {
                blocks: HashMap::new(),
                operations_bytes: 0,
                operations_budget: RESIDENT_OPERATION_BYTES,
                operations_order: VecDeque::new(),
                tree,
                tree_at: base_block_hash,
                prepared: None,
                tree_depth: 0,
                applied: VecDeque::new(),
                undo_heap_bytes: 0,
                undo_byte_budget: RETAINED_UNDO_BYTES,
                pending_durable: None,
            }),
            commit: Mutex::new(()),
            wal: Mutex::new(None),
            read_views: RwLock::new(None),
            #[cfg(test)]
            wal_fault: Mutex::new(None),
        })
    }

    /// Records the chain identity and base height written into base files.
    pub fn with_identity(mut self, identity: QmdbBaseIdentity) -> Self {
        self.identity = identity;
        self
    }

    pub const fn base_identity(&self) -> QmdbBaseIdentity {
        self.identity
    }

    pub fn wal_enabled(&self) -> bool {
        self.persistence_path.is_some()
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
        mut tree: QmdbLeafTree,
        max_replay_depth: usize,
        path: PathBuf,
    ) -> Result<Self, Gov5QmdbStateRootError> {
        let rebuilt = B256::from(tree.root());
        if rebuilt != base_root {
            return Err(Gov5QmdbStateRootError::BaseRootMismatch {
                got: rebuilt,
                expected: base_root,
            });
        }
        // This derived replay anchor belongs to this open store's immutable
        // base. Background rebasing updates a different file. Recreate it from
        // the supplied authenticated tree on open, without another RAM copy.
        let mut anchor = path.as_os_str().to_os_string();
        anchor.push(".replay");
        let anchor = PathBuf::from(anchor);
        write_base_file(
            &anchor,
            &tree,
            QmdbBaseIdentity {
                chain_id: 0,
                genesis_hash: B256::ZERO,
                block_number: 0,
            },
            0,
            base_block_hash,
            base_root,
        )?;
        let mut store = Self::from_leaf_tree_with_replay_base(
            base_block_hash,
            base_root,
            tree,
            max_replay_depth,
            QmdbReplayBase::File(base_file_path(&anchor)),
        )?;
        store.persistence_path = Some(path.clone());
        store.wal_read_path = Some(Arc::new(wal_path(&path)));
        match std::fs::File::open(&path) {
            Ok(file) => {
                let persisted: PersistedQmdbBranchState =
                    bincode::deserialize_from(std::io::BufReader::new(file))
                        .map_err(|error| Gov5QmdbStateRootError::Persistence(error.to_string()))?;
                if persisted.version != PERSISTED_BRANCH_STATE_VERSION
                    || persisted.base_block_hash != store.base_block_hash
                    || persisted.base_root != store.base_root
                {
                    return Err(Gov5QmdbStateRootError::PersistedBaseMismatch);
                }
                let mut blocks = persisted
                    .blocks
                    .into_iter()
                    .map(|(hash, block)| (hash, IndexedQmdbBlock::resident(block)))
                    .collect();
                load_wal(&wal_path(&path), &mut blocks)?;
                let mut state = store
                    .state
                    .lock()
                    .map_err(|_| Gov5QmdbStateRootError::LockPoisoned)?;
                state.blocks = blocks;
                validate_persisted_blocks(&store, &mut state)?;
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
        store.index_legacy_checkpoint_operations()?;
        Ok(store)
    }

    /// Reads a leaf-form base file and its header, checking the root it
    /// claims. The caller binds the header's block to its chain.
    pub fn read_base_file(
        path: &Path,
    ) -> Result<(QmdbLeafTree, QmdbLeafFormHeader), Gov5QmdbStateRootError> {
        Self::read_base_file_with_prefix(path, n42_twig_core::qmdb_leaf_tree::GENESIS_PREFIX_LEAVES)
    }

    pub fn read_base_file_with_prefix(
        path: &Path,
        prefix_count: usize,
    ) -> Result<(QmdbLeafTree, QmdbLeafFormHeader), Gov5QmdbStateRootError> {
        let file = std::fs::File::open(path).map_err(|error| {
            Gov5QmdbStateRootError::Persistence(format!(
                "failed to open QMDB base {}: {error}",
                path.display()
            ))
        })?;
        let (mut tree, header) = QmdbLeafTree::read_leaf_form_v2_with_prefix(
            std::io::BufReader::with_capacity(1 << 20, file),
            prefix_count,
        )?;
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

    /// Enable derived execution read views. Subsequent commits publish a view
    /// only after their binary root and WAL succeed. Existing durable blocks
    /// can be pinned with `prepare_read_view`, including after restart.
    pub fn enable_read_views(
        &self,
        capacity: std::num::NonZeroUsize,
    ) -> Result<(), Gov5QmdbStateRootError> {
        self.enable_read_views_with_budget(
            capacity,
            std::num::NonZeroUsize::new(512 * 1024 * 1024).unwrap(),
        )
    }

    /// Bound cache ownership by view count and full logical key/value bytes.
    /// Shared nodes are charged repeatedly; allocator overhead and views held
    /// by active providers are not an RSS bound. One oversized newest view is
    /// retained and reported by the over-budget gauge.
    pub fn enable_read_views_with_budget(
        &self,
        capacity: std::num::NonZeroUsize,
        byte_budget: std::num::NonZeroUsize,
    ) -> Result<(), Gov5QmdbStateRootError> {
        let _commit = self
            .commit
            .lock()
            .map_err(|_| Gov5QmdbStateRootError::LockPoisoned)?;
        let mut views = self
            .read_views
            .write()
            .map_err(|_| Gov5QmdbStateRootError::LockPoisoned)?;
        if views.is_none() {
            *views = Some(QmdbReadViewCache {
                capacity: capacity.get(),
                byte_budget: byte_budget.get(),
                logical_bytes: 0,
                views: HashMap::new(),
                order: VecDeque::new(),
            });
        }
        Ok(())
    }

    /// Provider-level cache lookup, with LRU promotion so a lagging database's
    /// actively used version survives newer commits. `None` means a cache
    /// miss, never an absent account/slot. No forest lock or per-key lookup.
    pub fn read_view_for(
        &self,
        block_hash: B256,
    ) -> Result<Option<Arc<QmdbReadView>>, Gov5QmdbStateRootError> {
        let mut views = self
            .read_views
            .write()
            .map_err(|_| Gov5QmdbStateRootError::LockPoisoned)?;
        Ok(views.as_mut().and_then(|cache| cache.get(block_hash)))
    }

    /// Materialize a retained durable version on demand, e.g. the persisted
    /// execution head at startup or after a deep unwind. Keep the returned Arc
    /// for the provider's lifetime; never call this once per account/slot.
    pub fn prepare_read_view(
        &self,
        block_hash: B256,
    ) -> Result<Option<Arc<QmdbReadView>>, Gov5QmdbStateRootError> {
        if let Some(view) = self.read_view_for(block_hash)? {
            return Ok(Some(view));
        }
        let _commit = self
            .commit
            .lock()
            .map_err(|_| Gov5QmdbStateRootError::LockPoisoned)?;
        // A concurrent provider may have prepared it while this one waited.
        if let Some(view) = self.read_view_for(block_hash)? {
            return Ok(Some(view));
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| Gov5QmdbStateRootError::LockPoisoned)?;
        let Some(root) = (if block_hash == self.base_block_hash {
            Some(self.base_root)
        } else {
            Self::durable_block(&state, block_hash).map(|block| block.root)
        }) else {
            return Ok(None);
        };
        let started = std::time::Instant::now();
        self.move_tree_to(&mut state, block_hash)?;
        let view = Arc::new(QmdbReadView::from_tree(block_hash, root, &state.tree));
        metrics::histogram!("n42_qmdb_read_view_build_ms", "operation" => "restore")
            .record(started.elapsed().as_secs_f64() * 1_000.0);
        if let Some(cache) = self
            .read_views
            .write()
            .map_err(|_| Gov5QmdbStateRootError::LockPoisoned)?
            .as_mut()
        {
            cache.insert(Arc::clone(&view));
        }
        Ok(Some(view))
    }

    fn candidate_read_view(
        &self,
        state: &QmdbBranchState,
        parent_hash: B256,
        block_hash: B256,
        root: B256,
        operations: &[QmdbOperation],
    ) -> Result<Option<Arc<QmdbReadView>>, Gov5QmdbStateRootError> {
        let views = self
            .read_views
            .read()
            .map_err(|_| Gov5QmdbStateRootError::LockPoisoned)?;
        let Some(cache) = views.as_ref() else {
            return Ok(None);
        };
        let parent = cache.views.get(&parent_hash).cloned();
        drop(views);
        let started = std::time::Instant::now();
        // The candidate is already applied, while branch metadata still
        // names its parent until admission succeeds under the same lock.
        debug_assert_eq!(state.tree_at, parent_hash);
        let view = Arc::new(match parent {
            Some(parent) => parent.with_operations(block_hash, root, operations),
            None => QmdbReadView::from_tree(block_hash, root, &state.tree),
        });
        metrics::histogram!("n42_qmdb_read_view_build_ms", "operation" => "commit")
            .record(started.elapsed().as_secs_f64() * 1_000.0);
        metrics::histogram!("n42_qmdb_read_view_updates").record(operations.len() as f64);
        Ok(Some(view))
    }

    /// Live entries and append cursor of the tree, for start-up logging.
    pub fn tree_stats(&self) -> Result<(usize, u64, usize), Gov5QmdbStateRootError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| Gov5QmdbStateRootError::LockPoisoned)?;
        Self::discard_prepared(&mut state)?;
        Ok((
            state.tree.len(),
            state.tree.next_slot(),
            state.tree.twig_count(),
        ))
    }

    /// A retained block as archive readers see it: durable, or absent.
    fn durable_block(state: &QmdbBranchState, block_hash: B256) -> Option<&IndexedQmdbBlock> {
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

    /// Compute a candidate from its exact parent branch, then undo its mutations.
    /// This does not publish a block, root, read view, or durable record.
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
            .apply_candidate_locked(&mut state, parent_hash, operations)
            .and_then(|(root, undo)| {
                if let Some(undo) = undo {
                    state.tree.apply_undo(&undo)?;
                }
                Ok(root)
            });
        metrics::histogram!("n42_qmdb_candidate_compute_ms", "operation" => "candidate")
            .record(compute_started.elapsed().as_secs_f64() * 1_000.0);
        metrics::histogram!("n42_qmdb_lock_hold_ms", "operation" => "candidate")
            .record(lock_acquired.elapsed().as_secs_f64() * 1_000.0);
        result
    }

    /// Price an owned builder delta and retain at most one bounded, unpublished
    /// tree calculation. Import still executes normally and must supply the
    /// same parent and complete operations before the applied tree can be reused.
    pub fn prepare_candidate(
        &self,
        parent_hash: B256,
        operations: Vec<QmdbOperation>,
    ) -> Result<B256, Gov5QmdbStateRootError> {
        self.prepare_candidate_with_limit(parent_hash, operations, PREPARED_CANDIDATE_BYTES)
    }

    fn prepare_candidate_with_limit(
        &self,
        parent_hash: B256,
        operations: Vec<QmdbOperation>,
        byte_limit: usize,
    ) -> Result<B256, Gov5QmdbStateRootError> {
        let started = std::time::Instant::now();
        let mut state = self
            .state
            .lock()
            .map_err(|_| Gov5QmdbStateRootError::LockPoisoned)?;
        let acquired = std::time::Instant::now();
        metrics::histogram!("n42_qmdb_lock_wait_ms", "operation" => "prepare")
            .record(acquired.duration_since(started).as_secs_f64() * 1000.0);
        let result = self.apply_candidate_locked(&mut state, parent_hash, &operations)
            .and_then(|(root, undo)| {
                if let Some(undo) = undo {
                    // Charge Vec/value capacity and every newly touched leaf
                    // heap conservatively. This is retained payload accounting,
                    // not an RSS or transient-allocation bound.
                    let heap_count = state.tree.next_slot().div_ceil(n42_twig_core::TWIG_SIZE as u64)
                        .saturating_sub(undo.prev_next_slot / n42_twig_core::TWIG_SIZE as u64);
                    let heap_bytes = (heap_count as usize).saturating_mul(
                        2 * n42_twig_core::TWIG_SIZE * std::mem::size_of::<[u8; 32]>());
                    let bytes = operations.capacity() * std::mem::size_of::<QmdbOperation>()
                        + operations.iter().filter_map(|op| op.value.as_ref()).map(Vec::capacity).sum::<usize>()
                        + undo_heap_bytes(&undo) + heap_bytes;
                    if bytes <= byte_limit {
                        state.prepared = Some(PreparedCandidate { parent_hash, root, operations, undo });
                        metrics::gauge!("n42_qmdb_prepared_candidate_bytes").set(bytes as f64);
                        metrics::counter!("n42_qmdb_prepared_candidates_total", "outcome" => "retained").increment(1);
                    } else {
                        state.tree.apply_undo(&undo)?;
                        metrics::counter!("n42_qmdb_prepared_candidates_total", "outcome" => "oversized").increment(1);
                    }
                }
                Ok(root)
            });
        metrics::histogram!("n42_qmdb_candidate_compute_ms", "operation" => "prepare")
            .record(acquired.elapsed().as_secs_f64() * 1000.0);
        metrics::histogram!("n42_qmdb_lock_hold_ms", "operation" => "prepare")
            .record(acquired.elapsed().as_secs_f64() * 1000.0);
        result
    }

    fn discard_prepared(state: &mut QmdbBranchState) -> Result<(), Gov5QmdbStateRootError> {
        if let Some(prepared) = &state.prepared {
            debug_assert_eq!(state.tree_at, prepared.parent_hash);
            // Keep the recovery record available if validation of its undo fails.
            state.tree.apply_undo(&prepared.undo)?;
            state.prepared = None;
            metrics::gauge!("n42_qmdb_prepared_candidate_bytes").set(0.0);
            metrics::counter!("n42_qmdb_prepared_candidates_total", "outcome" => "discarded")
                .increment(1);
        }
        Ok(())
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
        if block_hash == self.base_block_hash {
            metrics::counter!("n42_qmdb_commit_outcomes_total", "outcome" => "cache_conflict")
                .increment(1);
            metrics::histogram!("n42_qmdb_lock_hold_ms", "operation" => "commit")
                .record(lock_acquired.elapsed().as_secs_f64() * 1_000.0);
            return Err(Gov5QmdbStateRootError::ConflictingBlock { block_hash });
        }
        if let Some(stored) = state.blocks.get(&block_hash) {
            let result = if stored.parent_hash == parent_hash
                && stored.root == expected_root
                && same_operations(&stored.operations(block_hash)?, &operations)
            {
                metrics::counter!("n42_qmdb_commit_outcomes_total", "outcome" => "cache_hit")
                    .increment(1);
                Ok(stored.root)
            } else {
                metrics::counter!("n42_qmdb_commit_outcomes_total", "outcome" => "cache_conflict")
                    .increment(1);
                // A cache conflict is not a newly computed root mismatch.
                // Returning the old root could equal the claimed header root
                // and make the engine accept a different execution bundle.
                Err(Gov5QmdbStateRootError::ConflictingBlock { block_hash })
            };
            metrics::histogram!("n42_qmdb_lock_hold_ms", "operation" => "commit")
                .record(lock_acquired.elapsed().as_secs_f64() * 1_000.0);
            return result;
        }

        let compute_started = std::time::Instant::now();
        let candidate = if state.prepared.as_ref().is_some_and(|prepared| {
            prepared.parent_hash == parent_hash
                && same_operations(&prepared.operations, &operations)
        }) {
            let prepared = state.prepared.take().expect("matched candidate");
            debug_assert_eq!(state.tree_at, parent_hash);
            metrics::gauge!("n42_qmdb_prepared_candidate_bytes").set(0.0);
            metrics::counter!("n42_qmdb_prepared_candidates_total", "outcome" => "adopted")
                .increment(1);
            Ok((prepared.root, Some(prepared.undo)))
        } else {
            metrics::counter!("n42_qmdb_prepared_candidates_total", "outcome" => "miss")
                .increment(1);
            self.apply_candidate_locked(&mut state, parent_hash, &operations)
        };
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
            if let Some(undo) = &undo {
                state.tree.apply_undo(undo)?;
            }
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
        let read_view = match self.candidate_read_view(
            &state,
            parent_hash,
            block_hash,
            root,
            &stored.operations,
        ) {
            Ok(view) => view,
            Err(error) => {
                if let Some(undo) = &undo {
                    state.tree.apply_undo(undo)?;
                }
                metrics::counter!("n42_qmdb_commit_outcomes_total", "outcome" => "read_view_error")
                    .increment(1);
                return Err(error);
            }
        };
        // Frame the record while the operations are still ours to borrow, so
        // a record that cannot be encoded never enters `blocks`. The write and
        // fsync happen after the lock is released.
        let frame = match self.encode_wal_frame(block_hash, &stored) {
            Ok(frame) => frame,
            Err(error) => {
                if let Some(undo) = &undo {
                    state.tree.apply_undo(undo)?;
                }
                metrics::counter!("n42_qmdb_commit_outcomes_total", "outcome" => "wal_error")
                    .increment(1);
                metrics::histogram!("n42_qmdb_lock_hold_ms", "operation" => "commit")
                    .record(lock_acquired.elapsed().as_secs_f64() * 1_000.0);
                return Err(error);
            }
        };
        // Admit the already applied candidate, retaining its original undo.
        // Successful commits perform no candidate revert or second apply.
        if let Some(undo) = undo {
            Self::push_applied(&mut state, block_hash, parent_hash, undo);
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
        let stored = IndexedQmdbBlock::resident(stored);
        let resident_bytes = stored.resident_bytes();
        state.operations_bytes += resident_bytes;
        if resident_bytes != 0 {
            state.operations_order.push_back(block_hash);
        }
        state.blocks.insert(block_hash, stored);
        state.pending_durable = frame.is_some().then_some(block_hash);
        let mut lock_hold = lock_acquired.elapsed();
        drop(state);

        let wal_started = std::time::Instant::now();
        let wal_result = match &frame {
            Some(frame) => self
                .append_wal_frame(frame)
                .map(|offset| Some(self.wal_location(offset, frame))),
            None => Ok(None),
        };
        metrics::histogram!("n42_qmdb_wal_append_ms")
            .record(wal_started.elapsed().as_secs_f64() * 1_000.0);

        let publish_started = std::time::Instant::now();
        let mut state = self
            .state
            .lock()
            .map_err(|_| Gov5QmdbStateRootError::LockPoisoned)?;
        state.pending_durable = None;
        let location = match wal_result {
            Ok(location) => location,
            Err(error) => {
                metrics::counter!("n42_qmdb_commit_outcomes_total", "outcome" => "wal_error")
                    .increment(1);
                if let Some(removed) = state.blocks.remove(&block_hash) {
                    state.operations_bytes -= removed.resident_bytes();
                }
                state.operations_order.retain(|hash| *hash != block_hash);
                // Speculation during the append may have evicted leaf heaps
                // needed by this block's undo. Move through the normal checked
                // path, which can reconstruct the durable parent from the base.
                if state.tree_at == block_hash {
                    self.move_tree_to(&mut state, parent_hash)?;
                }
                lock_hold += publish_started.elapsed();
                metrics::histogram!("n42_qmdb_lock_hold_ms", "operation" => "commit")
                    .record(lock_hold.as_secs_f64() * 1_000.0);
                return Err(error);
            }
        };
        state
            .blocks
            .get_mut(&block_hash)
            .expect("serialized commit")
            .wal = location;
        Self::trim_operations(&mut state);
        if let Some(view) = read_view
            && let Some(cache) = self
                .read_views
                .write()
                .map_err(|_| Gov5QmdbStateRootError::LockPoisoned)?
                .as_mut()
        {
            cache.insert(view);
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

    fn wal_location(&self, offset: u64, frame: &[u8]) -> QmdbWalLocation {
        QmdbWalLocation {
            path: self.wal_read_path.as_ref().expect("persistent WAL").clone(),
            offset,
            payload_len: u32::from_le_bytes(frame[..4].try_into().expect("encoded length")),
            checksum: frame[frame.len() - 32..]
                .try_into()
                .expect("encoded checksum"),
        }
    }

    fn trim_operations(state: &mut QmdbBranchState) {
        while state.operations_bytes > state.operations_budget {
            let Some(hash) = state.operations_order.pop_front() else {
                break;
            };
            let Some(block) = state.blocks.get_mut(&hash) else {
                continue;
            };
            if block.wal.is_none() {
                // Memory-only stores have no cold source. Never discard their
                // sole copy, or a block whose append is not yet durable.
                state.operations_order.push_front(hash);
                break;
            }
            state.operations_bytes -= block.resident_bytes();
            block.operations = None;
        }
        metrics::gauge!("n42_qmdb_operations_resident_bytes").set(state.operations_bytes as f64);
        metrics::gauge!("n42_qmdb_operations_over_budget_bytes").set(
            state
                .operations_bytes
                .saturating_sub(state.operations_budget) as f64,
        );
    }

    /// Legacy checkpoint records may predate the WAL. Append only those
    /// missing from it, preserving the authoritative checkpoint on errors.
    /// Each newly indexed record is fsynced before its RAM copy is released.
    fn index_legacy_checkpoint_operations(&self) -> Result<(), Gov5QmdbStateRootError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| Gov5QmdbStateRootError::LockPoisoned)?;
        state.operations_bytes = state
            .blocks
            .values()
            .map(IndexedQmdbBlock::resident_bytes)
            .sum();
        let legacy: Vec<_> = state
            .blocks
            .iter()
            .filter(|(_, block)| block.wal.is_none())
            .map(|(hash, _)| *hash)
            .collect();
        for hash in legacy {
            let stored = state.blocks[&hash].materialize(hash)?;
            let frame = self
                .encode_wal_frame(hash, &stored)?
                .expect("persistent checkpoint");
            let offset = self.append_wal_frame(&frame)?;
            let location = self.wal_location(offset, &frame);
            let block = state.blocks.get_mut(&hash).expect("legacy record");
            let released = block.resident_bytes();
            block.wal = Some(location);
            block.operations = None;
            state.operations_bytes -= released;
        }
        state.operations_bytes = state
            .blocks
            .values()
            .map(IndexedQmdbBlock::resident_bytes)
            .sum();
        state.operations_order.clear();
        Self::trim_operations(&mut state);
        Ok(())
    }

    fn push_applied(
        state: &mut QmdbBranchState,
        block_hash: B256,
        parent_hash: B256,
        undo: BlockUndo,
    ) {
        debug_assert_eq!(state.tree_at, parent_hash);
        let heap_bytes = undo_heap_bytes(&undo);
        state.undo_heap_bytes += heap_bytes;
        state.applied.push_back(AppliedUndo {
            block_hash,
            parent_hash,
            undo,
            heap_bytes,
        });
        // Keep the newest record even if it alone exceeds the budget: a WAL
        // failure must still be able to roll back the just-applied block.
        while state.applied.len() > 1
            && (state.applied.len() > RETAINED_UNDO_RECORDS
                || state.undo_heap_bytes > state.undo_byte_budget)
        {
            let removed = state.applied.pop_front().expect("more than one record");
            state.undo_heap_bytes -= removed.heap_bytes;
        }
        Self::record_undo_usage(state);
        state.tree_at = block_hash;
        state.tree_depth = state.tree_depth.saturating_add(1);
    }

    /// Reverts the newest applied block; the tree then stands at its parent.
    fn pop_applied(state: &mut QmdbBranchState) -> Result<(), Gov5QmdbStateRootError> {
        let applied = state
            .applied
            .back()
            .ok_or(Gov5QmdbStateRootError::MissingParent(state.tree_at))?;
        debug_assert_eq!(applied.block_hash, state.tree_at);
        state.tree.apply_undo(&applied.undo)?;
        let applied = state.applied.pop_back().expect("validated undo");
        state.undo_heap_bytes -= applied.heap_bytes;
        Self::record_undo_usage(state);
        state.tree_at = applied.parent_hash;
        state.tree_depth = state.tree_depth.saturating_sub(1);
        Ok(())
    }

    fn record_undo_usage(state: &QmdbBranchState) {
        metrics::gauge!("n42_qmdb_undo_heap_bytes").set(state.undo_heap_bytes as f64);
        metrics::gauge!("n42_qmdb_undo_records").set(state.applied.len() as f64);
        metrics::gauge!("n42_qmdb_undo_over_budget_bytes")
            .set(state.undo_heap_bytes.saturating_sub(state.undo_byte_budget) as f64);
    }

    fn load_replay_base(&self) -> Result<QmdbLeafTree, Gov5QmdbStateRootError> {
        let started = std::time::Instant::now();
        let tree = match &self.replay_base {
            QmdbReplayBase::Memory(tree) => (**tree).clone(),
            QmdbReplayBase::File(path) => {
                let (tree, header) = Self::read_base_file(path)?;
                if B256::from(header.block_hash) != self.base_block_hash
                    || B256::from(header.root) != self.base_root
                {
                    return Err(Gov5QmdbStateRootError::PersistedBaseMismatch);
                }
                tree
            }
        };
        metrics::histogram!("n42_qmdb_replay_base_load_ms")
            .record(started.elapsed().as_secs_f64() * 1_000.0);
        metrics::counter!("n42_qmdb_replay_base_loads_total").increment(1);
        Ok(tree)
    }

    /// Apply a candidate exactly once and return its root and undo. Branch
    /// metadata still names `parent_hash`: callers must either admit the undo
    /// with `push_applied` or revert it before releasing the state lock.
    fn apply_candidate_locked(
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
        let (root, undo) = state.tree.apply_sorted_ops_recorded_borrowed(operations)?;
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
        Self::discard_prepared(state)?;
        if target == state.tree_at {
            metrics::counter!("n42_qmdb_tip_cache_total", "outcome" => "hit").increment(1);
            return Ok(state.tree_depth);
        }
        metrics::counter!("n42_qmdb_tip_cache_total", "outcome" => "miss").increment(1);
        // Blocks the tree can stand at by popping `n` applied records.
        let mut reachable: HashMap<B256, usize> = HashMap::with_capacity(state.applied.len() + 1);
        reachable.insert(state.tree_at, 0);
        for (pops, applied) in state.applied.iter().rev().enumerate() {
            // Undo values can outlive the leaf heaps needed to reopen old
            // twigs. Such an ancestor requires base replay, not a partial pop.
            if !state.tree.can_rewind_to(applied.undo.prev_next_slot) {
                break;
            }
            reachable.entry(applied.parent_hash).or_insert(pops + 1);
        }
        let mut lineage: Vec<B256> = Vec::new();
        let mut cursor = target;
        let pops = loop {
            if let Some(pops) = reachable.get(&cursor) {
                break Some(*pops);
            }
            if cursor == self.base_block_hash {
                break None;
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
        metrics::histogram!("n42_qmdb_tree_moves", "direction" => "revert")
            .record(pops.unwrap_or(0) as f64);
        metrics::histogram!("n42_qmdb_tree_moves", "direction" => "replay")
            .record(lineage.len() as f64);
        if let Some(pops) = pops {
            for _ in 0..pops {
                Self::pop_applied(state)?;
            }
        } else {
            // Load and authenticate before discarding the current tree. A
            // missing/corrupt anchor leaves the current branch usable.
            let tree = self.load_replay_base()?;
            state.tree = tree;
            state.tree_at = self.base_block_hash;
            state.tree_depth = 0;
            state.applied.clear();
            state.undo_heap_bytes = 0;
            Self::record_undo_usage(state);
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
            let operations = stored.operations(hash)?;
            let (root, undo) = tree.apply_sorted_ops_recorded_borrowed(&operations)?;
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
            drop(operations);
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
            let mut state = self
                .state
                .lock()
                .map_err(|_| Gov5QmdbStateRootError::LockPoisoned)?;
            if state.pending_durable.is_some() && state.pending_durable == Some(state.tree_at) {
                return Ok(None);
            }
            Self::discard_prepared(&mut state)?;
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
            Ok(mut state) => {
                if Self::discard_prepared(&mut state).is_err() {
                    self.rebase_in_flight.store(false, Ordering::Release);
                    return;
                }
                (state.tree.clone(), state.tree_at, state.tree_depth)
            }
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
        blocks: &HashMap<B256, IndexedQmdbBlock>,
    ) -> Result<(), Gov5QmdbStateRootError> {
        let Some(path) = &self.persistence_path else {
            return Ok(());
        };
        let persisted = PersistedQmdbBranchState {
            version: PERSISTED_BRANCH_STATE_VERSION,
            base_block_hash: self.base_block_hash,
            base_root: self.base_root,
            blocks: blocks
                .iter()
                .map(|(hash, block)| Ok((*hash, block.materialize(*hash)?)))
                .collect::<Result<_, Gov5QmdbStateRootError>>()?,
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
        #[cfg(test)]
        {
            let mut fault = self.wal_fault.lock().unwrap();
            if matches!(*fault, Some(WalFault::FailEncode)) {
                *fault = None;
                return Err(Gov5QmdbStateRootError::Persistence(
                    "injected QMDB WAL encoding failure".to_owned(),
                ));
            }
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
    fn append_wal_frame(&self, frame: &[u8]) -> Result<u64, Gov5QmdbStateRootError> {
        let mut guard = self
            .wal
            .lock()
            .map_err(|_| Gov5QmdbStateRootError::LockPoisoned)?;
        let Some(wal) = guard.as_mut() else {
            return Err(Gov5QmdbStateRootError::Persistence(
                "QMDB WAL handle is missing".into(),
            ));
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
        let offset = wal.len;
        wal.len = wal.len.saturating_add(frame.len() as u64);
        Ok(offset)
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
            WalFault::FailEncode => (
                Some(std::io::Error::other("unconsumed WAL encoding fault")),
                false,
            ),
            WalFault::Delay(duration) => {
                std::thread::sleep(duration);
                (None, false)
            }
            WalFault::WaitThenFail(release) => {
                release.wait();
                (
                    Some(std::io::Error::other("injected QMDB WAL barrier failure")),
                    false,
                )
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
        .and_then(|()| n42_jmt::snapshot::sync_parent_directory(&path))
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
    n42_jmt::snapshot::sync_parent_directory(path)
        .map_err(|error| Gov5QmdbStateRootError::Persistence(error.to_string()))?;
    Ok(QmdbWalFile {
        file,
        len,
        poisoned: None,
    })
}

fn load_wal(
    path: &Path,
    blocks: &mut HashMap<B256, IndexedQmdbBlock>,
) -> Result<(), Gov5QmdbStateRootError> {
    use std::io::{BufReader, ErrorKind, Read};
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(Gov5QmdbStateRootError::Persistence(error.to_string())),
    };
    let mut reader = BufReader::with_capacity(128 * 1024, file);
    let source_path = Arc::new(path.to_path_buf());
    let mut offset = 0u64;
    let mut payload = Vec::new();
    loop {
        let mut length = [0; 4];
        // Read one byte first to distinguish clean EOF from a torn length.
        match reader.read_exact(&mut length[..1]) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::UnexpectedEof => break,
            Err(error) => return Err(Gov5QmdbStateRootError::Persistence(error.to_string())),
        }
        if let Err(error) = reader.read_exact(&mut length[1..]) {
            if error.kind() == ErrorKind::UnexpectedEof {
                truncate_incomplete_wal(path, offset)?;
                break;
            }
            return Err(Gov5QmdbStateRootError::Persistence(error.to_string()));
        }
        let payload_len = u32::from_le_bytes(length) as usize;
        if payload_len > QMDB_WAL_MAX_RECORD_BYTES {
            return Err(Gov5QmdbStateRootError::Persistence(format!(
                "QMDB WAL record at offset {offset} declares invalid length {payload_len}"
            )));
        }
        // Retain at most one bounded frame buffer, not the complete WAL.
        payload
            .try_reserve_exact(payload_len.saturating_sub(payload.len()))
            .map_err(|error| {
                Gov5QmdbStateRootError::Persistence(format!(
                    "QMDB WAL frame allocation failed: {error}"
                ))
            })?;
        payload.resize(payload_len, 0);
        let mut checksum = [0; QMDB_WAL_CHECKSUM_BYTES];
        if let Err(error) = reader
            .read_exact(&mut payload)
            .and_then(|()| reader.read_exact(&mut checksum))
        {
            if error.kind() == ErrorKind::UnexpectedEof {
                truncate_incomplete_wal(path, offset)?;
                break;
            }
            return Err(Gov5QmdbStateRootError::Persistence(error.to_string()));
        }
        if blake3::hash(&payload).as_bytes() != &checksum {
            return Err(Gov5QmdbStateRootError::Persistence(format!(
                "QMDB WAL checksum mismatch at offset {offset}"
            )));
        }
        let record: PersistedQmdbWalRecord = bincode::deserialize(&payload)
            .map_err(|error| Gov5QmdbStateRootError::Persistence(error.to_string()))?;
        let indexed = IndexedQmdbBlock {
            parent_hash: record.block.parent_hash,
            root: record.block.root,
            operations: record.block.operations.is_empty().then(Vec::new),
            wal: Some(QmdbWalLocation {
                path: source_path.clone(),
                offset,
                payload_len: payload_len as u32,
                checksum,
            }),
        };
        match blocks.entry(record.block_hash) {
            std::collections::hash_map::Entry::Occupied(mut existing) => {
                if existing.get().parent_hash != record.block.parent_hash
                    || existing.get().root != record.block.root
                    || existing.get().operations(record.block_hash)?.as_ref()
                        != record.block.operations
                {
                    return Err(Gov5QmdbStateRootError::Persistence(format!(
                        "QMDB WAL redefines block {}",
                        record.block_hash
                    )));
                }
                existing.insert(indexed);
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(indexed);
            }
        }
        offset = offset
            .checked_add(4 + payload_len as u64 + QMDB_WAL_CHECKSUM_BYTES as u64)
            .ok_or_else(|| {
                Gov5QmdbStateRootError::Persistence("QMDB WAL frame length overflow".into())
            })?;
    }
    Ok(())
}

fn truncate_incomplete_wal(path: &Path, valid_len: u64) -> Result<(), Gov5QmdbStateRootError> {
    OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|file| {
            file.set_len(valid_len)?;
            file.sync_all()
        })
        .map_err(|error| Gov5QmdbStateRootError::Persistence(error.to_string()))
}

fn qualification_abort_at(point: &str) {
    if std::env::var("N42_QUALIFICATION_ABORT_AT").ok().as_deref() == Some(point) {
        eprintln!("N42_QUALIFICATION_ABORT_AT={point}: aborting after durable boundary");
        std::process::abort();
    }
}

/// Replays every retained block and checks each stored root. Parent-before-child
/// traversal uses the same bounded undo/base replay path as live branch moves,
/// rather than retaining one undo per ancestor until traversal finishes. Blocks that do
/// not descend from the base (retained below a base that has since moved
/// forward) are dropped from `blocks` with a warning; a block whose ancestry
/// is missing or cyclic is an error. The tree is left at the base.
fn validate_persisted_blocks(
    store: &Gov5QmdbStateRootStore,
    state: &mut QmdbBranchState,
) -> Result<(), Gov5QmdbStateRootError> {
    let base_block_hash = store.base_block_hash;
    let max_replay_depth = store.max_replay_depth;
    if state.blocks.contains_key(&base_block_hash) {
        return Err(Gov5QmdbStateRootError::ConflictingBlock {
            block_hash: base_block_hash,
        });
    }
    let mut children: HashMap<B256, Vec<B256>> = HashMap::new();
    for (hash, block) in &state.blocks {
        children.entry(block.parent_hash).or_default().push(*hash);
    }
    let mut validated: HashSet<B256> = HashSet::with_capacity(state.blocks.len());
    let mut pending: Vec<_> = children
        .remove(&base_block_hash)
        .unwrap_or_default()
        .into_iter()
        .map(|hash| (hash, 1usize))
        .collect();
    while let Some((child, depth)) = pending.pop() {
        // Candidate computation replays the parent and then applies the
        // child's operations, so a stored child may be one level beyond the
        // parent replay bound.
        if depth > max_replay_depth.saturating_add(1) {
            return Err(Gov5QmdbStateRootError::Persistence(format!(
                "QMDB block {child} exceeds replay depth {max_replay_depth}"
            )));
        }
        let parent = state.blocks[&child].parent_hash;
        store.move_tree_to(state, parent)?;
        let block = &state.blocks[&child];
        let operations = block.operations(child)?;
        let (root, undo) = state.tree.apply_sorted_ops_recorded_borrowed(&operations)?;
        let root = B256::from(root);
        if root != block.root {
            return Err(Gov5QmdbStateRootError::Persistence(format!(
                "QMDB stored root diverged for block {child}: got {root}, stored {}",
                block.root
            )));
        }
        drop(operations);
        Gov5QmdbStateRootStore::push_applied(state, child, parent, undo);
        debug_assert_eq!(state.tree_depth, depth);
        validated.insert(child);
        let next_depth = depth.checked_add(1).ok_or_else(|| {
            Gov5QmdbStateRootError::Persistence("QMDB ancestry depth overflow".into())
        })?;
        pending.extend(
            children
                .remove(&child)
                .unwrap_or_default()
                .into_iter()
                .map(|hash| (hash, next_depth)),
        );
    }
    store.move_tree_to(state, base_block_hash)?;
    let blocks = &mut state.blocks;
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
        .and_then(|()| n42_jmt::snapshot::sync_parent_directory(path))
        .map_err(|error| Gov5QmdbStateRootError::Persistence(error.to_string()))
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
    fn prepared_candidate_adopts_only_the_complete_delta_and_checks_header_root() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let (store, snapshot) = store();
        let parent = store.base_block_hash();
        store
            .enable_read_views(std::num::NonZeroUsize::new(2).unwrap())
            .unwrap();
        let pinned_parent = store.prepare_read_view(parent).unwrap().unwrap();
        let ops = vec![operation(1, 9), operation(2, 8)];
        let root = expected_root(&snapshot, std::slice::from_ref(&ops));
        assert_eq!(store.prepare_candidate(parent, ops.clone()).unwrap(), root);
        assert!(store.state.lock().unwrap().prepared.is_some());
        assert_eq!(pinned_parent.value(&[1; 32]), Some(&[1][..]));
        assert_eq!(
            store
                .prepare_read_view(parent)
                .unwrap()
                .unwrap()
                .value(&[1; 32]),
            Some(&[1][..])
        );
        assert!(store.state.lock().unwrap().prepared.is_some());
        assert_eq!(store.root_for(parent).unwrap(), Some(store.base_root()));
        assert_eq!(store.retained_block_count().unwrap(), 0);
        let hash = B256::repeat_byte(0x41);
        let mut reordered = ops.clone();
        reordered.reverse();
        assert!(
            matches!(store.compute_and_commit(parent, hash, B256::ZERO, reordered),
            Err(Gov5QmdbStateRootError::RootMismatch { got, .. }) if got == root)
        );
        assert!(store.state.lock().unwrap().prepared.is_none());
        assert!(!store.contains(hash).unwrap());
        assert_eq!(
            QmdbCompatTree::from_snapshot(&store.snapshot_for(parent).unwrap().unwrap())
                .unwrap()
                .root(),
            QmdbCompatTree::from_snapshot(&snapshot).unwrap().root()
        );
        store.prepare_candidate(parent, ops.clone()).unwrap();
        assert_eq!(
            store.compute_and_commit(parent, hash, root, ops).unwrap(),
            root
        );
        assert_eq!(store.retained_block_count().unwrap(), 1);
        assert_eq!(
            store.read_view_for(hash).unwrap().unwrap().value(&[1; 32]),
            Some(&[9][..])
        );
        let adopted =
            snapshotter
                .snapshot()
                .into_vec()
                .into_iter()
                .find_map(|(key, _, _, value)| {
                    (key.key().name() == "n42_qmdb_prepared_candidates_total"
                        && key
                            .key()
                            .labels()
                            .any(|label| label.key() == "outcome" && label.value() == "adopted"))
                    .then_some(value)
                });
        assert!(matches!(adopted, Some(DebugValue::Counter(2))));
    }
#[test]
    fn prepared_sibling_parent_and_changed_operations_never_reuse_a_root() {
        let (store, snapshot) = store();
        let base = store.base_block_hash();
        let a = B256::repeat_byte(0x41);
        let b = B256::repeat_byte(0x42);
        let a_ops = vec![operation(1, 2)];
        let b_ops = vec![operation(1, 3)];
        for (hash, ops) in [(a, a_ops.clone()), (b, b_ops.clone())] {
            store
                .compute_and_commit(
                    base,
                    hash,
                    expected_root(&snapshot, std::slice::from_ref(&ops)),
                    ops,
                )
                .unwrap();
        }
        let ops = vec![operation(2, 4)];
        let a_root = store.prepare_candidate(a, ops.clone()).unwrap();
        let b_root = expected_root(&snapshot, &[b_ops.clone(), ops.clone()]);
        assert_ne!(a_root, b_root);
        store
            .compute_and_commit(b, B256::repeat_byte(0x43), b_root, ops)
            .unwrap();
        for (index, incoming) in [
            vec![operation(2, 8)],
            vec![QmdbOperation {
                key: [2; 32],
                value: None,
            }],
            vec![QmdbOperation {
                key: [2; 32],
                value: Some(vec![]),
            }],
        ]
        .into_iter()
        .enumerate()
        {
            store.prepare_candidate(a, vec![operation(2, 7)]).unwrap();
            let expected = expected_root(&snapshot, &[a_ops.clone(), incoming.clone()]);
            store
                .compute_and_commit(a, B256::repeat_byte(0x50 + index as u8), expected, incoming)
                .unwrap();
        }
        store.prepare_candidate(a, vec![operation(2, 7)]).unwrap();
        assert!(matches!(
            store.compute_and_commit(
                a,
                B256::repeat_byte(0x60),
                a_root,
                vec![operation(2, 7), operation(2, 7)]
            ),
            Err(Gov5QmdbStateRootError::InvalidOperations(_))
        ));
        assert_eq!(
            store.compute_candidate(a, &[]).unwrap(),
            expected_root(&snapshot, &[a_ops])
        );
    }
#[test]
    fn prepared_candidate_is_invisible_to_historical_reads_and_capacity_fallback() {
        for reader in 0..4 {
            let (store, snapshot) = store();
            let base = store.base_block_hash();
            store
                .enable_read_views(std::num::NonZeroUsize::new(2).unwrap())
                .unwrap();
            store
                .prepare_candidate(base, vec![operation(1, 9)])
                .unwrap();
            match reader {
                0 => {
                    assert_eq!(
                        QmdbCompatTree::from_snapshot(&store.snapshot_for(base).unwrap().unwrap())
                            .unwrap()
                            .root(),
                        QmdbCompatTree::from_snapshot(&snapshot).unwrap().root()
                    );
                }
                1 => {
                    assert!(
                        store
                            .proof_for(base, [1; 32])
                            .unwrap()
                            .unwrap()
                            .verify_for_key(&store.base_root().0, &[1; 32])
                    );
                }
                2 => {
                    assert_eq!(
                        store
                            .prepare_read_view(base)
                            .unwrap()
                            .unwrap()
                            .value(&[1; 32]),
                        Some(&[1][..])
                    );
                }
                _ => {
                    assert_eq!(store.tree_stats().unwrap().0, 1);
                }
            }
            assert!(store.state.lock().unwrap().prepared.is_none());
        }
        let (store, _) = store();
        let mut value = Vec::with_capacity(1024 * 1024);
        value.push(7);
        let ops = vec![QmdbOperation {
            key: [2; 32],
            value: Some(value),
        }];
        let expected = store
            .compute_candidate(store.base_block_hash(), &ops)
            .unwrap();
        assert_eq!(
            store
                .prepare_candidate_with_limit(store.base_block_hash(), ops, 512 * 1024)
                .unwrap(),
            expected
        );
        assert!(store.state.lock().unwrap().prepared.is_none());
        assert_eq!(
            store
                .compute_candidate(store.base_block_hash(), &[])
                .unwrap(),
            store.base_root()
        );
    }
#[test]
    fn prepared_candidate_is_never_written_into_a_durable_base() {
        for background in [false, true] {
            let (base, snapshot) = store();
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("prepared-base.bin");
            let mut store = persistent_store(&base, &snapshot, &path);
            store.rebase_every = 1;
            store
                .prepare_candidate(base.base_block_hash(), vec![operation(1, 9)])
                .unwrap();
            if background {
                store.maybe_rebase_in_background();
                let started = std::time::Instant::now();
                while store.rebase_in_flight.load(Ordering::Acquire) {
                    assert!(started.elapsed() < std::time::Duration::from_secs(5));
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            } else {
                store.write_base_file().unwrap();
            }
            assert!(store.state.lock().unwrap().prepared.is_none());
            let (mut tree, header) =
                Gov5QmdbStateRootStore::read_base_file(&base_file_path(&path)).unwrap();
            assert_eq!(B256::from(header.block_hash), base.base_block_hash());
            assert_eq!(B256::from(tree.root()), base.base_root());
            assert_eq!(tree.get(&[1; 32]), Some(&[1][..]));
        }
    }
#[test]
    fn prepared_admission_wal_faults_rollback_and_retry_without_publication() {
        for fault in [WalFault::FailEncode, WalFault::FailWrite] {
            let (base, snapshot) = store();
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("prepared-wal.bin");
            let store = persistent_store(&base, &snapshot, &path);
            store
                .enable_read_views(std::num::NonZeroUsize::new(2).unwrap())
                .unwrap();
            let ops = vec![operation(1, 9), operation(2, 7)];
            let root = store
                .prepare_candidate(base.base_block_hash(), ops.clone())
                .unwrap();
            let hash = B256::repeat_byte(0x41);
            store.inject_wal_fault(fault);
            assert!(matches!(
                store.compute_and_commit(base.base_block_hash(), hash, root, ops.clone()),
                Err(Gov5QmdbStateRootError::Persistence(_))
            ));
            assert!(!store.contains(hash).unwrap());
            assert!(store.read_view_for(hash).unwrap().is_none());
            assert_eq!(
                QmdbCompatTree::from_snapshot(
                    &store.snapshot_for(base.base_block_hash()).unwrap().unwrap()
                )
                .unwrap()
                .root(),
                QmdbCompatTree::from_snapshot(&snapshot).unwrap().root()
            );
            store
                .prepare_candidate(base.base_block_hash(), ops.clone())
                .unwrap();
            store
                .compute_and_commit(base.base_block_hash(), hash, root, ops)
                .unwrap();
            drop(store);
            let reopened = persistent_store(&base, &snapshot, &path);
            assert_eq!(reopened.root_for(hash).unwrap(), Some(root));
            assert_eq!(
                reopened
                    .prepare_read_view(hash)
                    .unwrap()
                    .unwrap()
                    .value(&[1; 32]),
                Some(&[9][..])
            );
        }
    }
#[test]
    fn prepared_child_is_discarded_if_pending_parent_wal_fails() {
        let (base, snapshot) = store();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prepared-child.bin");
        let store = Arc::new(persistent_store(&base, &snapshot, &path));
        let hash = B256::repeat_byte(0x41);
        let ops = vec![operation(1, 9)];
        let root = store
            .prepare_candidate(base.base_block_hash(), ops.clone())
            .unwrap();
        let release = Arc::new(std::sync::Barrier::new(2));
        store.inject_wal_fault(WalFault::WaitThenFail(release.clone()));
        let writer = store.clone();
        let task = std::thread::spawn(move || {
            writer.compute_and_commit(writer.base_block_hash(), hash, root, ops)
        });
        let started = std::time::Instant::now();
        while !store.wal_in_flight() {
            assert!(started.elapsed() < std::time::Duration::from_secs(5));
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let candidate = store.prepare_candidate(hash, vec![operation(2, 7)]);
        let retained = store.state.lock().unwrap().prepared.is_some();
        release.wait();
        assert!(task.join().unwrap().is_err());
        assert!(candidate.is_ok() && retained);
        assert!(store.state.lock().unwrap().prepared.is_none());
        assert!(!store.contains(hash).unwrap());
        assert_eq!(
            QmdbCompatTree::from_snapshot(
                &store.snapshot_for(base.base_block_hash()).unwrap().unwrap()
            )
            .unwrap()
            .root(),
            QmdbCompatTree::from_snapshot(&snapshot).unwrap().root()
        );
    }
#[test]
#[ignore="measurement"]
    fn bench_persistent_commit_path() {
        use alloy_primitives::U256;
        use n42_twig_core::qmdb_compat::{encode_gov5_account_value, gov5_account_key};
        let keys = std::env::var("N42_COMMIT_BENCH_KEYS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(200_000);
        let blocks = std::env::var("N42_COMMIT_BENCH_BLOCKS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(3);
        let reads = match std::env::var("N42_COMMIT_BENCH_READS").as_deref() {
            Ok("0") => false,
            Err(_) | Ok("1") => true,
            Ok(other) => panic!("invalid read benchmark mode {other}"),
        };
        let preparation =
            std::env::var("N42_COMMIT_BENCH_PREPARATION").unwrap_or_else(|_| "none".into());
        assert!(matches!(
            preparation.as_str(),
            "none" | "discard" | "retain"
        ));
        assert!(keys > 0 && blocks > 0);
        let address = |n: u64| {
            let mut address = [0u8; 20];
            address[12..].copy_from_slice(&n.to_be_bytes());
            address
        };
        let operations = |count: u64, generation: u64| -> Vec<QmdbOperation> {
            (0..count)
                .map(|n| QmdbOperation {
                    key: gov5_account_key(&address(n)),
                    value: Some(encode_gov5_account_value(
                        generation,
                        &U256::from(generation * 1_000_000_000).to_be_bytes(),
                        &B256::ZERO.0,
                    )),
                })
                .collect()
        };
        let mut reference = QmdbLeafTree::new();
        let base_root = B256::from(reference.apply_sorted_ops(operations(keys, 1)).unwrap());
        let base_tree = reference.clone();
        let base_hash = B256::repeat_byte(0x77);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bench-store.bin");
        let store = Gov5QmdbStateRootStore::persistent_from_leaf_tree(
            base_hash,
            base_root,
            reference.clone(),
            DEFAULT_QMDB_REPLAY_DEPTH,
            path.clone(),
        )
        .unwrap();
        if reads {
            store
                .enable_read_views(std::num::NonZeroUsize::new(64).unwrap())
                .unwrap();
            store.prepare_read_view(base_hash).unwrap().unwrap();
        }
        let mut parent = base_hash;
        for generation in 2..blocks + 2 {
            let mut ops = operations(keys.min(147_000), generation);
            let root = B256::from(reference.apply_sorted_ops(ops.clone()).unwrap());
            let hash = B256::from(blake3::hash(&generation.to_le_bytes()).as_bytes());
            // Builder and import independently produce their deltas. Their
            // construction/copy is outside both timings, like EVM execution.
            let mut builder_ops = if preparation == "none" {
                Vec::new()
            } else {
                ops.clone()
            };
            let start = std::time::Instant::now();
            let mut retained = false;
            if preparation != "none" {
                builder_ops.sort_unstable_by_key(|op| op.key);
                ops.sort_unstable_by_key(|op| op.key);
                let candidate = if preparation == "retain" {
                    store.prepare_candidate(parent, builder_ops).unwrap()
                } else {
                    let root = store.compute_candidate(parent, &builder_ops).unwrap();
                    drop(builder_ops);
                    root
                };
                assert_eq!(candidate, root);
                retained = store.state.lock().unwrap().prepared.is_some();
                assert_eq!(retained, preparation == "retain");
            }
            let candidate_ms = start.elapsed().as_secs_f64() * 1000.0;
            let commit_start = std::time::Instant::now();
            let committed = store.compute_and_commit(parent, hash, root, ops).unwrap();
            let commit_ms = commit_start.elapsed().as_secs_f64() * 1000.0;
            let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
            assert_eq!(committed, root);
            assert!(store.contains(hash).unwrap());
            assert_eq!(B256::from(store.state.lock().unwrap().tree.root()), root);
            if reads {
                assert_eq!(
                    store
                        .read_view_for(hash)
                        .unwrap()
                        .unwrap()
                        .account(&alloy_primitives::Address::from(address(0)))
                        .unwrap()
                        .unwrap()
                        .nonce,
                    generation
                );
            }
            println!(
                "commit_bench reads={} keys={keys} updates={} generation={generation} elapsed_ms={elapsed_ms:.3} root={root} wal_bytes={}",
                u8::from(reads),
                keys.min(147_000),
                std::fs::metadata(wal_path(&path)).unwrap().len()
            );
            println!(
                "prepare_bench mode={preparation} retained={retained} generation={generation} candidate_ms={candidate_ms:.3} commit_ms={commit_ms:.3}"
            );
            parent = hash;
        }
        drop(store);
        let reopened = Gov5QmdbStateRootStore::persistent_from_leaf_tree(
            base_hash,
            base_root,
            base_tree,
            DEFAULT_QMDB_REPLAY_DEPTH,
            path,
        )
        .unwrap();
        assert_eq!(
            reopened.root_for(parent).unwrap(),
            Some(B256::from(reference.root()))
        );
        assert_eq!(
            reopened
                .prepare_read_view(parent)
                .unwrap()
                .unwrap()
                .account(&alloy_primitives::Address::from(address(0)))
                .unwrap()
                .unwrap()
                .nonce,
            blocks + 1
        );
        println!("commit_bench restart_verified=true");
    }
}
