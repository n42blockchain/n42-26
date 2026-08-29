//! The split QMDB commitment, held without the dead slots.
//!
//! A QMDB root is `upper(twig roots)`, and a twig root is
//! `hash(leafRoot, hash(activeBits))`. Once a twig is full its leaves never
//! change again — slots are never reused — so the only part of a sealed twig
//! that a later block can touch is its active bits. This tree therefore keeps,
//! per sealed twig, exactly its frozen leaf root and its 256 bytes of bits;
//! only the twig being appended to (the open twig) carries a leaf heap. The
//! live entries are indexed by key. Chain 94's forest — 63 million slots of
//! which 6 million are live — is 30,933 sealed twigs (≈ 9 MB) plus the live
//! index, instead of the 63 million entries and 4 GB of twig heaps that
//! [`super::qmdb_compat::QmdbCompatTree`] materialises.
//!
//! What it gives up: membership proofs for keys in sealed twigs (the leaf
//! siblings are gone), and the v1 positional snapshot. Everything the state
//! root needs is here, byte for byte the same root as the full tree — the
//! tests below check that against the full implementation.
//!
//! Every mutation can be recorded into a [`BlockUndo`] and reverted exactly,
//! which is how a candidate block is priced without copying anything: apply,
//! read the root, revert.

use crate::qmdb_compat::{
    BITS_BYTES, BlockUndo, QmdbLeafForm, QmdbOperation, QmdbOperationError, QmdbProof,
    QmdbSlotEntry, QmdbTwigSnapshot, QmdbUndoError, UndoEntry, hash_bits,
};
use crate::{Hash, NULL_HASH, TWIG_HEIGHT, TWIG_SIZE, hash_leaf, hash_node, null_level};
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::io::{Read, Write};

const PORTABLE_SNAPSHOT_MAGIC_V2: &[u8; 8] = b"N42QMDB\x02";
const PORTABLE_TWIG_HOLLOW: u8 = 0;
const PORTABLE_TWIG_LEAVES: u8 = 1;
const MAX_PORTABLE_VALUE_SIZE: usize = 16 << 20;
/// Heaps of twigs sealed by recent blocks are kept so an undo that crosses a
/// twig boundary can reopen the twig. A block appends a few thousand slots at
/// most, so one boundary per block is the worst case; the bound only has to
/// cover the deepest revert the store performs.
const RETAINED_SEALED_HEAPS: usize = 256;
/// Leaf hashes of the first slots of twig 0 are reported by the reader so a
/// caller can match the chain's genesis prefix without the positional log.
pub const GENESIS_PREFIX_LEAVES: usize = 64;

/// A twig whose leaves are frozen: its leaf root and its active bits.
#[derive(Clone)]
struct SealedTwig {
    leaf_root: Hash,
    bits: [u8; BITS_BYTES],
}

/// The twig being appended to.
#[derive(Clone)]
struct OpenTwig {
    id: usize,
    nodes: Box<[Hash; 2 * TWIG_SIZE]>,
    bits: [u8; BITS_BYTES],
}

impl OpenTwig {
    fn new(id: usize, nulls: &[Hash; TWIG_HEIGHT + 1]) -> Self {
        let mut nodes = Box::new([NULL_HASH; 2 * TWIG_SIZE]);
        for (index, node) in nodes.iter_mut().enumerate().take(TWIG_SIZE).skip(1) {
            let depth = (u32::BITS - 1 - (index as u32).leading_zeros()) as usize;
            *node = nulls[TWIG_HEIGHT - depth];
        }
        Self {
            id,
            nodes,
            bits: [0u8; BITS_BYTES],
        }
    }

    fn set_leaf(&mut self, local: usize, leaf: Hash) {
        let mut node = TWIG_SIZE + local;
        self.nodes[node] = leaf;
        while node > 1 {
            node >>= 1;
            self.nodes[node] = hash_node(&self.nodes[node * 2], &self.nodes[node * 2 + 1]);
        }
    }

    fn recompute(&mut self) {
        for start in (1..TWIG_SIZE).rev() {
            self.nodes[start] = hash_node(&self.nodes[start * 2], &self.nodes[start * 2 + 1]);
        }
    }

    fn root(&self) -> Hash {
        hash_node(&self.nodes[1], &hash_bits(&self.bits))
    }
}

/// A live entry: where it sits and what it holds.
#[derive(Clone, Debug, PartialEq, Eq)]
struct LiveEntry {
    slot: u64,
    value: Box<[u8]>,
}

/// Why a leaf form could not be turned into a tree.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum QmdbLeafTreeError {
    #[error("QMDB leaf form is inconsistent: {0}")]
    Inconsistent(String),
    #[error("QMDB leaf form has duplicate live key {0:?}")]
    DuplicateLiveKey(Hash),
}

/// Why a leaf-form file could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum QmdbLeafFormIoError {
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("not a QMDB leaf-form (v2) snapshot")]
    WrongMagic,
    #[error("QMDB leaf-form snapshot content digest mismatch")]
    DigestMismatch,
    #[error("QMDB leaf-form snapshot is inconsistent: {0}")]
    Inconsistent(String),
    #[error(transparent)]
    Tree(#[from] QmdbLeafTreeError),
}

/// What the header of a leaf-form file says about the state it carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QmdbLeafFormHeader {
    pub chain_id: u64,
    pub genesis_hash: Hash,
    pub block_number: u64,
    pub block_hash: Hash,
    pub root: Hash,
    pub next_slot: u64,
    pub twigs: u64,
    pub live: u64,
    /// Leaf hashes of the first [`GENESIS_PREFIX_LEAVES`] slots of twig 0,
    /// when the file carried twig 0's leaves; frozen, so they identify the
    /// genesis allocation the chain was seeded from.
    pub genesis_prefix_leaves: Vec<Hash>,
}

/// The split QMDB commitment: sealed twigs as leaf root plus bits, the open
/// twig in full, the live entries by key.
#[derive(Clone)]
pub struct QmdbLeafTree {
    sealed: Vec<SealedTwig>,
    open: Option<OpenTwig>,
    /// Heaps of recently sealed twigs, oldest first, so an undo can reopen
    /// the twig a block sealed.
    retired_heaps: VecDeque<(usize, Box<[Hash; 2 * TWIG_SIZE]>)>,
    live: HashMap<Hash, LiveEntry>,
    next_slot: u64,
    /// Cached twig roots, one per twig (sealed and open).
    twig_roots: Vec<Hash>,
    /// Cached upper tree over `twig_roots`: `upper[cap + id]` is twig `id`'s
    /// root, `upper[1]` the state root. Rebuilt when the twig count outgrows
    /// `cap`, otherwise repaired along the paths of dirty twigs.
    upper: Vec<Hash>,
    upper_cap: usize,
    dirty: BTreeSet<usize>,
    recording: Option<BlockUndo>,
    nulls: [Hash; TWIG_HEIGHT + 1],
}

impl std::fmt::Debug for QmdbLeafTree {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QmdbLeafTree")
            .field("live", &self.live.len())
            .field("next_slot", &self.next_slot)
            .field("twigs", &self.twig_count())
            .finish_non_exhaustive()
    }
}

impl Default for QmdbLeafTree {
    fn default() -> Self {
        Self::new()
    }
}

impl QmdbLeafTree {
    pub fn new() -> Self {
        Self {
            sealed: Vec::new(),
            open: None,
            retired_heaps: VecDeque::new(),
            live: HashMap::new(),
            next_slot: 0,
            twig_roots: Vec::new(),
            upper: Vec::new(),
            upper_cap: 0,
            dirty: BTreeSet::new(),
            recording: None,
            nulls: null_level(),
        }
    }

    pub fn next_slot(&self) -> u64 {
        self.next_slot
    }

    /// Live entries.
    pub fn len(&self) -> usize {
        self.live.len()
    }

    pub fn is_empty(&self) -> bool {
        self.live.is_empty()
    }

    pub fn twig_count(&self) -> usize {
        self.sealed.len() + usize::from(self.open.is_some())
    }

    pub fn get(&self, key: &Hash) -> Option<&[u8]> {
        self.live.get(key).map(|entry| &*entry.value)
    }

    /// The slot a live key occupies.
    pub fn slot_of(&self, key: &Hash) -> Option<u64> {
        self.live.get(key).map(|entry| entry.slot)
    }

    fn twig_bits(&self, twig_id: usize) -> Option<&[u8; BITS_BYTES]> {
        if twig_id < self.sealed.len() {
            Some(&self.sealed[twig_id].bits)
        } else {
            self.open
                .as_ref()
                .filter(|open| open.id == twig_id)
                .map(|open| &open.bits)
        }
    }

    fn twig_bits_mut(&mut self, twig_id: usize) -> &mut [u8; BITS_BYTES] {
        if twig_id < self.sealed.len() {
            &mut self.sealed[twig_id].bits
        } else {
            &mut self
                .open
                .as_mut()
                .filter(|open| open.id == twig_id)
                .expect("a slot below the cursor belongs to a sealed or the open twig")
                .bits
        }
    }

    fn set_bit(&mut self, slot: u64, active: bool) {
        let twig_id = (slot as usize) / TWIG_SIZE;
        let local = (slot as usize) % TWIG_SIZE;
        let bits = self.twig_bits_mut(twig_id);
        if active {
            bits[local / 8] |= 1 << (local % 8);
        } else {
            bits[local / 8] &= !(1 << (local % 8));
        }
        self.dirty.insert(twig_id);
    }

    /// Append a new frozen leaf, deactivating an earlier live slot for `key`.
    pub fn set(&mut self, key: Hash, value: Vec<u8>) {
        if let Some(old) = self.live.get(&key) {
            let (old_slot, old_value) = (old.slot, old.value.clone());
            self.record_deactivation(old_slot, key, old_value);
            self.set_bit(old_slot, false);
        }
        if let Some(record) = self.recording.as_mut() {
            record.appended_keys.push(key);
        }
        let slot = self.next_slot;
        self.next_slot += 1;
        let twig_id = (slot as usize) / TWIG_SIZE;
        let local = (slot as usize) % TWIG_SIZE;
        if local == 0 {
            debug_assert!(self.open.is_none());
            self.open = Some(OpenTwig::new(twig_id, &self.nulls));
            self.twig_roots.push(NULL_HASH);
        }
        let leaf = hash_leaf(&key, &value);
        let open = self
            .open
            .as_mut()
            .expect("appending always has an open twig");
        open.set_leaf(local, leaf);
        open.bits[local / 8] |= 1 << (local % 8);
        self.dirty.insert(twig_id);
        self.live.insert(
            key,
            LiveEntry {
                slot,
                value: value.into_boxed_slice(),
            },
        );
        if local + 1 == TWIG_SIZE {
            self.seal_open();
        }
    }

    fn seal_open(&mut self) {
        let open = self.open.take().expect("sealing needs an open twig");
        debug_assert_eq!(open.id, self.sealed.len());
        self.sealed.push(SealedTwig {
            leaf_root: open.nodes[1],
            bits: open.bits,
        });
        self.retired_heaps.push_back((open.id, open.nodes));
        while self.retired_heaps.len() > RETAINED_SEALED_HEAPS {
            self.retired_heaps.pop_front();
        }
    }

    /// Deactivate the live slot for `key`, if any.
    pub fn delete(&mut self, key: &Hash) -> bool {
        let Some(entry) = self.live.remove(key) else {
            return false;
        };
        self.record_deactivation(entry.slot, *key, entry.value);
        self.set_bit(entry.slot, false);
        true
    }

    fn record_deactivation(&mut self, slot: u64, key: Hash, value: Box<[u8]>) {
        if let Some(record) = self.recording.as_mut() {
            record.entries.push(UndoEntry {
                slot,
                key,
                value: value.into_vec(),
            });
        }
    }

    /// Begins capturing an undo record for the operations that follow.
    pub fn start_undo_recording(&mut self) {
        self.recording = Some(BlockUndo {
            prev_next_slot: self.next_slot,
            entries: Vec::new(),
            appended_keys: Vec::new(),
        });
    }

    /// Ends the capture and returns the record, or `None` if none was started.
    pub fn stop_undo_recording(&mut self) -> Option<BlockUndo> {
        self.recording.take()
    }

    /// Apply one block's mutations in gov5's deterministic order (sorted by
    /// key). Duplicates are rejected before anything is changed.
    pub fn apply_sorted_ops(
        &mut self,
        operations: impl IntoIterator<Item = QmdbOperation>,
    ) -> Result<Hash, QmdbOperationError> {
        let mut operations = operations.into_iter().collect::<Vec<_>>();
        operations.sort_unstable_by_key(|operation| operation.key);
        for pair in operations.windows(2) {
            if pair[0].key == pair[1].key {
                return Err(QmdbOperationError::DuplicateKey(pair[0].key));
            }
        }
        for operation in operations {
            if let Some(value) = operation.value {
                self.set(operation.key, value);
            } else {
                self.delete(&operation.key);
            }
        }
        Ok(self.root())
    }

    /// Like [`Self::apply_sorted_ops`], returning the record that undoes it.
    pub fn apply_sorted_ops_recorded(
        &mut self,
        operations: impl IntoIterator<Item = QmdbOperation>,
    ) -> Result<(Hash, BlockUndo), QmdbOperationError> {
        self.start_undo_recording();
        let root = match self.apply_sorted_ops(operations) {
            Ok(root) => root,
            Err(error) => {
                self.recording = None;
                return Err(error);
            }
        };
        let undo = self.recording.take().unwrap_or_default();
        Ok((root, undo))
    }

    /// Rolls the tree back across one block. Afterwards the root is, byte for
    /// byte, the root before the block, and re-applying the block lands on
    /// the same slots. Nothing is mutated until every check has passed.
    pub fn apply_undo(&mut self, undo: &BlockUndo) -> Result<(), QmdbUndoError> {
        if self.recording.is_some() {
            return Err(QmdbUndoError::RecordingActive);
        }
        let prev = undo.prev_next_slot;
        if prev > self.next_slot {
            return Err(QmdbUndoError::Ahead {
                prev,
                next: self.next_slot,
            });
        }
        // A revived slot must be dead now and must not collide with a live
        // key that sits elsewhere below the cursor: such a record belongs to
        // another history.
        for entry in undo.entries.iter().filter(|entry| entry.slot < prev) {
            let bits = self
                .twig_bits((entry.slot as usize) / TWIG_SIZE)
                .ok_or(QmdbUndoError::EntryMismatch { slot: entry.slot })?;
            let local = (entry.slot as usize) % TWIG_SIZE;
            if bits[local / 8] & (1 << (local % 8)) != 0 {
                return Err(QmdbUndoError::EntryMismatch { slot: entry.slot });
            }
            if self
                .live
                .get(&entry.key)
                .is_some_and(|live| live.slot < prev)
            {
                return Err(QmdbUndoError::EntryMismatch { slot: entry.slot });
            }
        }
        // Reopening a twig the block sealed needs its heap.
        let first_sealed_by_block = (prev as usize).div_ceil(TWIG_SIZE);
        for twig_id in first_sealed_by_block..self.sealed.len() {
            if !self.retired_heaps.iter().any(|(id, _)| *id == twig_id) {
                return Err(QmdbUndoError::Ahead {
                    prev,
                    next: self.next_slot,
                });
            }
        }

        // 1. Truncate the block's appends, newest first, reopening twigs on
        //    the way down.
        let mut slot = self.next_slot;
        while slot > prev {
            slot -= 1;
            let twig_id = (slot as usize) / TWIG_SIZE;
            let local = (slot as usize) % TWIG_SIZE;
            if twig_id < self.sealed.len() {
                // Everything above this twig has been truncated already: the
                // open twig, if any, is empty and goes before the twig below
                // it is reopened.
                self.drop_open_twig();
                self.reopen_last_sealed();
            }
            let open = self
                .open
                .as_mut()
                .expect("the twig holding an appended slot is open");
            debug_assert_eq!(open.id, twig_id);
            open.nodes[TWIG_SIZE + local] = NULL_HASH;
            open.bits[local / 8] &= !(1 << (local % 8));
            self.dirty.insert(twig_id);
        }
        for key in &undo.appended_keys {
            if self.live.get(key).is_some_and(|live| live.slot >= prev) {
                self.live.remove(key);
            }
        }
        self.next_slot = prev;
        if let Some(open) = self.open.as_mut() {
            let appended = prev as usize - open.id * TWIG_SIZE;
            if appended == 0 {
                self.drop_open_twig();
            } else {
                open.recompute();
            }
        }
        let keep_below = self.twig_count().max(self.upper_cap);
        self.dirty.retain(|id| *id < keep_below);

        // 2. Revive the slots the block deactivated, below the cursor only: a
        //    slot the block appended and killed went with the truncation.
        for entry in undo.entries.iter().filter(|entry| entry.slot < prev) {
            self.live.insert(
                entry.key,
                LiveEntry {
                    slot: entry.slot,
                    value: entry.value.clone().into_boxed_slice(),
                },
            );
            self.set_bit(entry.slot, true);
        }
        Ok(())
    }

    /// Drops the open twig (which must hold no appended slot any more) and
    /// clears its leaf of the cached upper tree.
    fn drop_open_twig(&mut self) {
        let Some(open) = self.open.take() else {
            return;
        };
        let id = open.id;
        self.twig_roots.truncate(id);
        let index = self.upper_cap + id;
        if self.upper_cap > 0 && index < self.upper.len() {
            self.upper[index] = NULL_HASH;
            self.dirty.insert(id);
        }
    }

    fn reopen_last_sealed(&mut self) {
        let id = self.sealed.len() - 1;
        let position = self
            .retired_heaps
            .iter()
            .position(|(heap_id, _)| *heap_id == id)
            .expect("checked before mutating");
        let (_, nodes) = self
            .retired_heaps
            .remove(position)
            .expect("position is valid");
        let sealed = self.sealed.pop().expect("a sealed twig exists");
        debug_assert!(self.open.is_none());
        self.open = Some(OpenTwig {
            id,
            nodes,
            bits: sealed.bits,
        });
    }

    fn twig_root(&self, twig_id: usize) -> Hash {
        if twig_id < self.sealed.len() {
            let twig = &self.sealed[twig_id];
            hash_node(&twig.leaf_root, &hash_bits(&twig.bits))
        } else {
            self.open
                .as_ref()
                .filter(|open| open.id == twig_id)
                .map(OpenTwig::root)
                .unwrap_or(NULL_HASH)
        }
    }

    fn rebuild_upper(&mut self) {
        let count = self.twig_count();
        let cap = count.next_power_of_two().max(1);
        self.upper_cap = cap;
        self.upper = vec![NULL_HASH; cap * 2];
        self.twig_roots.resize(count, NULL_HASH);
        for id in 0..count {
            let root = self.twig_root(id);
            self.twig_roots[id] = root;
            self.upper[cap + id] = root;
        }
        for index in (1..cap).rev() {
            self.upper[index] = hash_node(&self.upper[index * 2], &self.upper[index * 2 + 1]);
        }
        self.dirty.clear();
    }

    /// The state root. Repairs the cached upper tree along the paths of the
    /// twigs touched since the last read.
    pub fn root(&mut self) -> Hash {
        let count = self.twig_count();
        if count == 0 {
            self.dirty.clear();
            return NULL_HASH;
        }
        // The upper tree is as deep as the twig count needs, exactly as the
        // full tree computes it, so a count that crossed a power of two in
        // either direction rebuilds it.
        if self.upper_cap != count.next_power_of_two() {
            self.rebuild_upper();
            return self.upper[1];
        }
        let dirty = std::mem::take(&mut self.dirty);
        for twig_id in dirty {
            let root = if twig_id < count {
                self.twig_root(twig_id)
            } else {
                NULL_HASH
            };
            if twig_id < self.twig_roots.len() {
                self.twig_roots[twig_id] = root;
            }
            let mut index = self.upper_cap + twig_id;
            self.upper[index] = root;
            while index > 1 {
                index >>= 1;
                self.upper[index] = hash_node(&self.upper[index * 2], &self.upper[index * 2 + 1]);
            }
        }
        self.upper[1]
    }

    /// Whether `key` is live in the open twig, the only twig whose leaf
    /// siblings this tree holds.
    pub fn open_twig_holds(&self, key: &Hash) -> bool {
        match (self.live.get(key), &self.open) {
            (Some(entry), Some(open)) => (entry.slot as usize) / TWIG_SIZE == open.id,
            _ => false,
        }
    }

    /// A gov5-compatible membership proof for a key in the open twig. The
    /// leaves of sealed twigs are not held, so a key there yields `None`,
    /// as does an absent key.
    pub fn prove(&mut self, key: &Hash) -> Option<QmdbProof> {
        if !self.open_twig_holds(key) {
            return None;
        }
        self.root();
        let entry = self.live.get(key)?;
        let open = self.open.as_ref()?;
        let slot = entry.slot;
        let local = slot as usize % TWIG_SIZE;
        let mut twig_path = [NULL_HASH; TWIG_HEIGHT];
        let mut node = TWIG_SIZE + local;
        for sibling in &mut twig_path {
            *sibling = open.nodes[node ^ 1];
            node >>= 1;
        }
        let mut upper_path = Vec::with_capacity(self.upper_cap.trailing_zeros() as usize);
        let mut upper_node = self.upper_cap + open.id;
        while upper_node > 1 {
            upper_path.push(self.upper[upper_node ^ 1]);
            upper_node >>= 1;
        }
        Some(QmdbProof {
            key: *key,
            value: entry.value.to_vec(),
            slot,
            twig_path,
            active_bits: open.bits,
            upper_path,
        })
    }

    /// The tree in leaf form. Sealed twigs are written hollow (leaf root and
    /// bits only); the open twig carries its leaves. Such a form restores
    /// into this tree, but not into the full [`super::qmdb_compat::QmdbCompatTree`],
    /// which insists on the leaves of any twig with live slots.
    pub fn leaf_form(&self) -> QmdbLeafForm {
        let mut twigs = Vec::with_capacity(self.twig_count());
        for twig in &self.sealed {
            twigs.push(QmdbTwigSnapshot {
                leaf_root: twig.leaf_root,
                bits: twig.bits.to_vec(),
                leaves: None,
            });
        }
        if let Some(open) = &self.open {
            let appended = self.next_slot as usize - open.id * TWIG_SIZE;
            twigs.push(QmdbTwigSnapshot {
                leaf_root: open.nodes[1],
                bits: open.bits.to_vec(),
                leaves: Some(open.nodes[TWIG_SIZE..TWIG_SIZE + appended].to_vec()),
            });
        }
        let mut live: Vec<QmdbSlotEntry> = self
            .live
            .iter()
            .map(|(key, entry)| QmdbSlotEntry {
                slot: entry.slot,
                key: *key,
                value: entry.value.to_vec(),
                active: true,
            })
            .collect();
        live.sort_unstable_by_key(|entry| entry.slot);
        QmdbLeafForm {
            next_slot: self.next_slot,
            twigs,
            live,
        }
    }

    /// A tree from a leaf form. A twig with leaves must fold to its leaf root
    /// and every live entry in it must hash to its leaf; a hollow twig is
    /// taken on its leaf root (this tree needs no more). Live bits past the
    /// cursor and out-of-order or duplicate live entries are refused.
    pub fn from_leaf_form(form: &QmdbLeafForm) -> Result<Self, QmdbLeafTreeError> {
        let mut builder = LeafTreeBuilder::new(form.next_slot, form.twigs.len() as u64)?;
        for (id, twig) in form.twigs.iter().enumerate() {
            let bits: [u8; BITS_BYTES] = twig.bits.as_slice().try_into().map_err(|_| {
                QmdbLeafTreeError::Inconsistent(format!(
                    "twig {id} has {} bytes of bits, not {BITS_BYTES}",
                    twig.bits.len()
                ))
            })?;
            builder.push_twig(id, twig.leaf_root, bits, twig.leaves.as_deref())?;
        }
        for entry in &form.live {
            builder.push_live(entry.slot, entry.key, entry.value.clone())?;
        }
        builder.finish(form.live.len() as u64)
    }

    /// Writes the tree as a portable v2 leaf-form snapshot (see
    /// [`Self::leaf_form`] for what sealed twigs carry), digest included.
    pub fn write_leaf_form_v2<W: Write>(
        &self,
        writer: W,
        chain_id: u64,
        genesis_hash: &Hash,
        block_number: u64,
        block_hash: &Hash,
        root: &Hash,
    ) -> Result<(), QmdbLeafFormIoError> {
        let mut out = DigestWriter::new(writer);
        out.write_all(PORTABLE_SNAPSHOT_MAGIC_V2)?;
        out.write_all(&chain_id.to_le_bytes())?;
        out.write_all(genesis_hash)?;
        out.write_all(&block_number.to_le_bytes())?;
        out.write_all(block_hash)?;
        out.write_all(root)?;
        out.write_all(&self.next_slot.to_le_bytes())?;
        out.write_all(&(self.twig_count() as u64).to_le_bytes())?;
        out.write_all(&(self.live.len() as u64).to_le_bytes())?;
        for twig in &self.sealed {
            out.write_all(&twig.leaf_root)?;
            out.write_all(&twig.bits)?;
            out.write_all(&[PORTABLE_TWIG_HOLLOW])?;
        }
        if let Some(open) = &self.open {
            let appended = self.next_slot as usize - open.id * TWIG_SIZE;
            out.write_all(&open.nodes[1])?;
            out.write_all(&open.bits)?;
            out.write_all(&[PORTABLE_TWIG_LEAVES])?;
            for leaf in &open.nodes[TWIG_SIZE..TWIG_SIZE + appended] {
                out.write_all(leaf)?;
            }
        }
        let mut live: Vec<(u64, &Hash, &[u8])> = self
            .live
            .iter()
            .map(|(key, entry)| (entry.slot, key, &*entry.value))
            .collect();
        live.sort_unstable_by_key(|(slot, _, _)| *slot);
        for (slot, key, value) in live {
            out.write_all(&slot.to_le_bytes())?;
            out.write_all(key)?;
            out.write_all(&(value.len() as u32).to_le_bytes())?;
            out.write_all(value)?;
        }
        let digest = out.finish()?;
        Ok(digest)
    }

    /// Reads a portable v2 leaf-form snapshot from a stream, verifying the
    /// content digest, folding every twig's leaves (in parallel) against its
    /// leaf root and every live entry against its leaf. Memory is the tree
    /// plus the live slots' leaf hashes while the entries are checked; the
    /// leaves of sealed twigs are never held whole.
    pub fn read_leaf_form_v2<R: Read>(
        reader: R,
    ) -> Result<(Self, QmdbLeafFormHeader), QmdbLeafFormIoError> {
        let mut reader = DigestReader::new(reader);
        let mut magic = [0u8; 8];
        reader.read_exact(&mut magic)?;
        if &magic != PORTABLE_SNAPSHOT_MAGIC_V2 {
            return Err(QmdbLeafFormIoError::WrongMagic);
        }
        let chain_id = reader.u64()?;
        let genesis_hash = reader.hash()?;
        let block_number = reader.u64()?;
        let block_hash = reader.hash()?;
        let root = reader.hash()?;
        let next_slot = reader.u64()?;
        let twig_count = reader.u64()?;
        let live_count = reader.u64()?;
        let mut builder = LeafTreeBuilder::new(next_slot, twig_count)?;
        let mut genesis_prefix_leaves = Vec::new();

        // Twigs with leaves are folded in batches on all cores; each batch is
        // read whole, verified, then reduced to leaf root and bits.
        const BATCH: usize = 512;
        let mut batch: Vec<(usize, Hash, [u8; BITS_BYTES], Vec<Hash>)> = Vec::with_capacity(BATCH);
        let flush = |batch: &mut Vec<(usize, Hash, [u8; BITS_BYTES], Vec<Hash>)>,
                     builder: &mut LeafTreeBuilder|
         -> Result<(), QmdbLeafFormIoError> {
            fold_batch(batch)?;
            for (id, leaf_root, bits, leaves) in batch.drain(..) {
                builder.push_folded_twig(id, leaf_root, bits, &leaves)?;
            }
            Ok(())
        };
        for id in 0..twig_count as usize {
            let leaf_root = reader.hash()?;
            let mut bits = [0u8; BITS_BYTES];
            reader.read_exact(&mut bits)?;
            let mode = reader.byte()?;
            match mode {
                PORTABLE_TWIG_HOLLOW => {
                    flush(&mut batch, &mut builder)?;
                    builder.push_twig(id, leaf_root, bits, None)?;
                }
                PORTABLE_TWIG_LEAVES => {
                    let appended = appended_in_twig(next_slot, id);
                    let mut leaves = vec![NULL_HASH; appended];
                    for leaf in &mut leaves {
                        *leaf = reader.hash()?;
                    }
                    if id == 0 {
                        genesis_prefix_leaves =
                            leaves.iter().take(GENESIS_PREFIX_LEAVES).copied().collect();
                    }
                    batch.push((id, leaf_root, bits, leaves));
                    if batch.len() == BATCH {
                        flush(&mut batch, &mut builder)?;
                    }
                }
                other => {
                    return Err(QmdbLeafFormIoError::Inconsistent(format!(
                        "twig {id} has mode {other}"
                    )));
                }
            }
        }
        flush(&mut batch, &mut builder)?;

        for _ in 0..live_count {
            let slot = reader.u64()?;
            let key = reader.hash()?;
            let value_len = reader.u32()? as usize;
            if value_len > MAX_PORTABLE_VALUE_SIZE {
                return Err(QmdbLeafFormIoError::Inconsistent(format!(
                    "live slot {slot} has a {value_len}-byte value"
                )));
            }
            let mut value = vec![0u8; value_len];
            reader.read_exact(&mut value)?;
            builder.push_live(slot, key, value)?;
        }
        let computed = reader.finish_digest();
        let mut stored = [0u8; 32];
        reader.inner.read_exact(&mut stored)?;
        if computed != stored {
            return Err(QmdbLeafFormIoError::DigestMismatch);
        }
        let mut trailing = [0u8; 1];
        if reader.inner.read(&mut trailing)? != 0 {
            return Err(QmdbLeafFormIoError::Inconsistent(
                "trailing bytes after the digest".into(),
            ));
        }
        let tree = builder.finish(live_count)?;
        Ok((
            tree,
            QmdbLeafFormHeader {
                chain_id,
                genesis_hash,
                block_number,
                block_hash,
                root,
                next_slot,
                twigs: twig_count,
                live: live_count,
                genesis_prefix_leaves,
            },
        ))
    }
}

/// The slots appended in twig `id` of a tree whose next slot is `next_slot`.
fn appended_in_twig(next_slot: u64, id: usize) -> usize {
    let base = (id * TWIG_SIZE) as u64;
    next_slot.saturating_sub(base).min(TWIG_SIZE as u64) as usize
}

/// Folds each batch entry's leaves and checks them against its leaf root.
fn fold_batch(
    batch: &mut [(usize, Hash, [u8; BITS_BYTES], Vec<Hash>)],
) -> Result<(), QmdbLeafFormIoError> {
    if batch.is_empty() {
        return Ok(());
    }
    let nulls = null_level();
    let threads = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(batch.len())
        .max(1);
    let chunk = batch.len().div_ceil(threads);
    let failed = std::sync::Mutex::new(None);
    std::thread::scope(|scope| {
        for part in batch.chunks(chunk) {
            let failed = &failed;
            let nulls = &nulls;
            scope.spawn(move || {
                let mut heap = OpenTwig::new(0, nulls);
                for (id, leaf_root, _, leaves) in part {
                    for node in heap.nodes[TWIG_SIZE..].iter_mut() {
                        *node = NULL_HASH;
                    }
                    for (local, leaf) in leaves.iter().enumerate() {
                        heap.nodes[TWIG_SIZE + local] = *leaf;
                    }
                    heap.recompute();
                    if heap.nodes[1] != *leaf_root {
                        *failed.lock().expect("lock") = Some(*id);
                        return;
                    }
                }
            });
        }
    });
    if let Some(id) = failed.into_inner().expect("lock") {
        return Err(QmdbLeafFormIoError::Inconsistent(format!(
            "twig {id}'s leaves do not fold to its leaf root"
        )));
    }
    Ok(())
}

/// Assembles a tree twig by twig, then live entry by live entry.
struct LeafTreeBuilder {
    tree: QmdbLeafTree,
    next_slot: u64,
    expected_twigs: usize,
    /// Leaf hashes of live slots, kept until their entries are checked.
    live_leaves: HashMap<u64, Hash>,
    live_bits: usize,
    previous_live_slot: Option<u64>,
}

impl LeafTreeBuilder {
    fn new(next_slot: u64, twig_count: u64) -> Result<Self, QmdbLeafTreeError> {
        let expected_twigs = (next_slot as usize).div_ceil(TWIG_SIZE);
        if twig_count != expected_twigs as u64 {
            return Err(QmdbLeafTreeError::Inconsistent(format!(
                "{twig_count} twigs for next slot {next_slot}, expected {expected_twigs}"
            )));
        }
        let mut tree = QmdbLeafTree::new();
        tree.next_slot = next_slot;
        tree.sealed.reserve(expected_twigs);
        Ok(Self {
            tree,
            next_slot,
            expected_twigs,
            live_leaves: HashMap::new(),
            live_bits: 0,
            previous_live_slot: None,
        })
    }

    fn check_bits(&self, id: usize, bits: &[u8; BITS_BYTES]) -> Result<usize, QmdbLeafTreeError> {
        let appended = appended_in_twig(self.next_slot, id);
        for local in appended..TWIG_SIZE {
            if bits[local / 8] & (1 << (local % 8)) != 0 {
                return Err(QmdbLeafTreeError::Inconsistent(format!(
                    "twig {id} marks slot {} live past the cursor",
                    id * TWIG_SIZE + local
                )));
            }
        }
        Ok(appended)
    }

    /// A twig whose leaves (if any) have already been folded and checked.
    fn push_folded_twig(
        &mut self,
        id: usize,
        leaf_root: Hash,
        bits: [u8; BITS_BYTES],
        leaves: &[Hash],
    ) -> Result<(), QmdbLeafTreeError> {
        if id != self.tree.twig_count() {
            return Err(QmdbLeafTreeError::Inconsistent(format!(
                "twig {id} arrived out of order"
            )));
        }
        let appended = self.check_bits(id, &bits)?;
        if leaves.len() != appended {
            return Err(QmdbLeafTreeError::Inconsistent(format!(
                "twig {id} carries {} leaves for {appended} appended slots",
                leaves.len()
            )));
        }
        for (local, leaf) in leaves.iter().enumerate() {
            if bits[local / 8] & (1 << (local % 8)) != 0 {
                self.live_leaves
                    .insert((id * TWIG_SIZE + local) as u64, *leaf);
                self.live_bits += 1;
            }
        }
        if appended == TWIG_SIZE {
            self.tree.sealed.push(SealedTwig { leaf_root, bits });
        } else {
            let mut open = OpenTwig::new(id, &self.tree.nulls);
            for (local, leaf) in leaves.iter().enumerate() {
                open.nodes[TWIG_SIZE + local] = *leaf;
            }
            open.recompute();
            open.bits = bits;
            debug_assert_eq!(open.nodes[1], leaf_root);
            self.tree.open = Some(open);
        }
        Ok(())
    }

    fn push_twig(
        &mut self,
        id: usize,
        leaf_root: Hash,
        bits: [u8; BITS_BYTES],
        leaves: Option<&[Hash]>,
    ) -> Result<(), QmdbLeafTreeError> {
        match leaves {
            Some(leaves) => {
                let appended = appended_in_twig(self.next_slot, id);
                if leaves.len() != appended {
                    return Err(QmdbLeafTreeError::Inconsistent(format!(
                        "twig {id} carries {} leaves for {appended} appended slots",
                        leaves.len()
                    )));
                }
                let mut heap = OpenTwig::new(id, &self.tree.nulls);
                for (local, leaf) in leaves.iter().enumerate() {
                    heap.nodes[TWIG_SIZE + local] = *leaf;
                }
                heap.recompute();
                if heap.nodes[1] != leaf_root {
                    return Err(QmdbLeafTreeError::Inconsistent(format!(
                        "twig {id}'s leaves do not fold to its leaf root"
                    )));
                }
                self.push_folded_twig(id, leaf_root, bits, leaves)
            }
            None => {
                if id != self.tree.twig_count() {
                    return Err(QmdbLeafTreeError::Inconsistent(format!(
                        "twig {id} arrived out of order"
                    )));
                }
                let appended = self.check_bits(id, &bits)?;
                if appended != TWIG_SIZE {
                    return Err(QmdbLeafTreeError::Inconsistent(format!(
                        "open twig {id} has no leaves"
                    )));
                }
                self.live_bits += bits
                    .iter()
                    .map(|byte| byte.count_ones() as usize)
                    .sum::<usize>();
                self.tree.sealed.push(SealedTwig { leaf_root, bits });
                Ok(())
            }
        }
    }

    fn push_live(&mut self, slot: u64, key: Hash, value: Vec<u8>) -> Result<(), QmdbLeafTreeError> {
        if self.tree.twig_count() != self.expected_twigs {
            return Err(QmdbLeafTreeError::Inconsistent(format!(
                "live slot {slot} arrived before every twig"
            )));
        }
        if slot >= self.next_slot {
            return Err(QmdbLeafTreeError::Inconsistent(format!(
                "live slot {slot} is past the cursor"
            )));
        }
        if self
            .previous_live_slot
            .is_some_and(|previous| previous >= slot)
        {
            return Err(QmdbLeafTreeError::Inconsistent(format!(
                "live slot {slot} is out of order"
            )));
        }
        self.previous_live_slot = Some(slot);
        let twig_id = slot as usize / TWIG_SIZE;
        let local = slot as usize % TWIG_SIZE;
        let bits = self
            .tree
            .twig_bits(twig_id)
            .expect("every twig has been pushed");
        if bits[local / 8] & (1 << (local % 8)) == 0 {
            return Err(QmdbLeafTreeError::Inconsistent(format!(
                "live slot {slot} has its bit clear"
            )));
        }
        if let Some(leaf) = self.live_leaves.remove(&slot)
            && leaf != hash_leaf(&key, &value)
        {
            return Err(QmdbLeafTreeError::Inconsistent(format!(
                "live slot {slot}'s entry does not hash to its leaf"
            )));
        }
        if self
            .tree
            .live
            .insert(
                key,
                LiveEntry {
                    slot,
                    value: value.into_boxed_slice(),
                },
            )
            .is_some()
        {
            return Err(QmdbLeafTreeError::DuplicateLiveKey(key));
        }
        Ok(())
    }

    fn finish(mut self, live_count: u64) -> Result<QmdbLeafTree, QmdbLeafTreeError> {
        if self.tree.twig_count() != self.expected_twigs {
            return Err(QmdbLeafTreeError::Inconsistent(format!(
                "{} twigs arrived, expected {}",
                self.tree.twig_count(),
                self.expected_twigs
            )));
        }
        if self.tree.live.len() as u64 != live_count || self.tree.live.len() != self.live_bits {
            return Err(QmdbLeafTreeError::Inconsistent(format!(
                "{} live entries for {} live slots (declared {live_count})",
                self.tree.live.len(),
                self.live_bits
            )));
        }
        self.tree.rebuild_upper();
        Ok(self.tree)
    }
}

struct DigestWriter<W: Write> {
    inner: W,
    hasher: blake3::Hasher,
}

impl<W: Write> DigestWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: blake3::Hasher::new(),
        }
    }

    fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.hasher.update(bytes);
        self.inner.write_all(bytes)
    }

    fn finish(mut self) -> std::io::Result<()> {
        let digest = self.hasher.finalize();
        self.inner.write_all(digest.as_bytes())?;
        self.inner.flush()
    }
}

struct DigestReader<R: Read> {
    inner: R,
    hasher: blake3::Hasher,
}

impl<R: Read> DigestReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            hasher: blake3::Hasher::new(),
        }
    }

    fn read_exact(&mut self, buf: &mut [u8]) -> std::io::Result<()> {
        self.inner.read_exact(buf)?;
        self.hasher.update(buf);
        Ok(())
    }

    fn u64(&mut self) -> std::io::Result<u64> {
        let mut buf = [0u8; 8];
        self.read_exact(&mut buf)?;
        Ok(u64::from_le_bytes(buf))
    }

    fn u32(&mut self) -> std::io::Result<u32> {
        let mut buf = [0u8; 4];
        self.read_exact(&mut buf)?;
        Ok(u32::from_le_bytes(buf))
    }

    fn byte(&mut self) -> std::io::Result<u8> {
        let mut buf = [0u8; 1];
        self.read_exact(&mut buf)?;
        Ok(buf[0])
    }

    fn hash(&mut self) -> std::io::Result<Hash> {
        let mut buf = [0u8; 32];
        self.read_exact(&mut buf)?;
        Ok(buf)
    }

    fn finish_digest(&self) -> Hash {
        *self.hasher.finalize().as_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qmdb_compat::QmdbCompatTree;

    fn key(n: u64) -> Hash {
        let mut key = [0u8; 32];
        key[..8].copy_from_slice(&n.to_be_bytes());
        *blake3::hash(&key).as_bytes()
    }

    fn sets(range: std::ops::Range<u64>, tag: u8) -> Vec<QmdbOperation> {
        range
            .map(|n| QmdbOperation {
                key: key(n),
                value: Some(vec![tag, n as u8, (n >> 8) as u8]),
            })
            .collect()
    }

    fn deletes(range: std::ops::Range<u64>) -> Vec<QmdbOperation> {
        range
            .map(|n| QmdbOperation {
                key: key(n),
                value: None,
            })
            .collect()
    }

    /// A churned history spanning several twigs, built on both trees.
    fn churned() -> (QmdbCompatTree, QmdbLeafTree) {
        let mut full = QmdbCompatTree::new();
        let mut leaf = QmdbLeafTree::new();
        let blocks: Vec<Vec<QmdbOperation>> = vec![
            sets(0..3000, 0xA0),
            deletes(0..1000),
            sets(500..2500, 0xB0),
            sets(3000..5000, 0xC0),
            deletes(2000..4000),
            sets(100..200, 0xD0),
        ];
        for ops in blocks {
            let a = full.apply_sorted_ops(ops.clone()).unwrap();
            let b = leaf.apply_sorted_ops(ops).unwrap();
            assert_eq!(a, b);
        }
        assert!(full.next_slot() > 3 * TWIG_SIZE as u64);
        (full, leaf)
    }

    #[test]
    fn roots_match_the_full_tree_through_a_churned_history() {
        let (full, mut leaf) = churned();
        assert_eq!(full.root(), leaf.root());
        assert_eq!(full.len(), leaf.len());
        assert_eq!(full.next_slot(), leaf.next_slot());
        for n in 0..5000u64 {
            assert_eq!(full.get(&key(n)), leaf.get(&key(n)), "key {n}");
        }
    }

    #[test]
    fn a_leaf_form_from_the_full_tree_restores_to_the_same_root() {
        let (full, mut leaf) = churned();
        let form = full.leaf_form();
        let mut restored = QmdbLeafTree::from_leaf_form(&form).unwrap();
        assert_eq!(restored.root(), leaf.root());
        assert_eq!(restored.len(), leaf.len());
        // And keeps evolving in step.
        let mut full = full;
        for ops in [sets(0..50, 0x11), deletes(10..30), sets(6000..6100, 0x12)] {
            assert_eq!(
                full.apply_sorted_ops(ops.clone()).unwrap(),
                restored.apply_sorted_ops(ops.clone()).unwrap()
            );
            assert_eq!(leaf.apply_sorted_ops(ops).unwrap(), restored.root());
        }
    }

    #[test]
    fn the_hollow_leaf_form_round_trips_through_the_v2_stream() {
        let (_, mut leaf) = churned();
        let root = leaf.root();
        let form = leaf.leaf_form();
        assert!(
            form.twigs[..form.twigs.len() - 1]
                .iter()
                .all(|t| t.leaves.is_none())
        );
        assert!(form.twigs.last().unwrap().leaves.is_some());
        let mut again = QmdbLeafTree::from_leaf_form(&form).unwrap();
        assert_eq!(again.root(), root);

        let mut bytes = Vec::new();
        leaf.write_leaf_form_v2(&mut bytes, 94, &key(1), 7, &key(2), &root)
            .unwrap();
        let (mut read, header) = QmdbLeafTree::read_leaf_form_v2(&bytes[..]).unwrap();
        assert_eq!(read.root(), root);
        assert_eq!(header.chain_id, 94);
        assert_eq!(header.block_number, 7);
        assert_eq!(header.root, root);
        assert_eq!(header.live, leaf.len() as u64);
        assert!(
            header.genesis_prefix_leaves.is_empty(),
            "twig 0 is hollow here"
        );

        // The block number in the header is covered by the digest alone.
        let mut tampered = bytes.clone();
        tampered[8 + 8 + 32] ^= 1;
        assert!(matches!(
            QmdbLeafTree::read_leaf_form_v2(&tampered[..]),
            Err(QmdbLeafFormIoError::DigestMismatch)
        ));
    }

    #[test]
    fn the_full_v2_stream_from_the_compat_tree_is_verified_and_read() {
        let (full, mut leaf) = churned();
        // The gov5 exporter writes every twig with leaves (mode 1).
        let portable = crate::qmdb_compat::QmdbPortableSnapshot {
            chain_id: 94,
            genesis_hash: key(1),
            block_number: 9,
            block_hash: key(2),
            root: full.root(),
            slots: crate::qmdb_compat::QmdbSlotSnapshot {
                next_slot: full.next_slot(),
                entries: Vec::new(),
            },
            leaf_form: Some(full.leaf_form()),
        };
        let bytes = portable.encode().unwrap();
        let (mut read, header) = QmdbLeafTree::read_leaf_form_v2(&bytes[..]).unwrap();
        assert_eq!(read.root(), leaf.root());
        assert_eq!(header.genesis_prefix_leaves.len(), GENESIS_PREFIX_LEAVES);
        // Slot 0 holds the smallest key of the first block's sorted batch.
        let first = (0..3000u64).min_by_key(|n| key(*n)).unwrap();
        assert_eq!(
            header.genesis_prefix_leaves[0],
            hash_leaf(&key(first), &[0xA0, first as u8, (first >> 8) as u8])
        );

        // A leaf that does not fold is refused.
        let mut form = full.leaf_form();
        form.twigs[1].leaves.as_mut().unwrap()[5] = key(99);
        let bad = crate::qmdb_compat::QmdbPortableSnapshot {
            leaf_form: Some(form),
            ..portable.clone()
        }
        .encode()
        .unwrap();
        let result = QmdbLeafTree::read_leaf_form_v2(&bad[..]);
        assert!(
            matches!(result, Err(QmdbLeafFormIoError::Inconsistent(_))),
            "{:?}",
            result.map(|(tree, _)| tree)
        );

        // A live entry that does not hash to its leaf is refused.
        let mut form = full.leaf_form();
        form.live[3].value.push(1);
        let bad = crate::qmdb_compat::QmdbPortableSnapshot {
            leaf_form: Some(form),
            ..portable
        }
        .encode()
        .unwrap();
        assert!(matches!(
            QmdbLeafTree::read_leaf_form_v2(&bad[..]),
            Err(QmdbLeafFormIoError::Tree(QmdbLeafTreeError::Inconsistent(
                _
            )))
        ));
    }

    /// Reads a real leaf-form export (`N42_QMDB_LEAF_FORM=<path>`), checks
    /// the root it claims and reports time and memory. Ignored unless the
    /// file is named; run with `--release`.
    #[test]
    #[ignore]
    fn a_real_leaf_form_export_rebuilds_its_root() {
        let Ok(path) = std::env::var("N42_QMDB_LEAF_FORM") else {
            return;
        };
        let started = std::time::Instant::now();
        let file = std::fs::File::open(&path).unwrap();
        let (mut tree, header) =
            QmdbLeafTree::read_leaf_form_v2(std::io::BufReader::with_capacity(1 << 20, file))
                .unwrap();
        let loaded = started.elapsed();
        let root_started = std::time::Instant::now();
        let root = tree.root();
        let rss = std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with("VmRSS"))
                    .map(str::to_string)
            });
        eprintln!(
            "leaf form {path}: block {} hash {} twigs {} live {} next_slot {} loaded in {:.2?}, root in {:.2?}, {}",
            header.block_number,
            hex::encode(header.block_hash),
            header.twigs,
            header.live,
            header.next_slot,
            loaded,
            root_started.elapsed(),
            rss.unwrap_or_default()
        );
        assert_eq!(
            root, header.root,
            "rebuilt root differs from the export's claim"
        );
        // One candidate priced by apply / root / undo.
        let ops: Vec<QmdbOperation> = (0..300u64)
            .map(|n| QmdbOperation {
                key: key(n),
                value: Some(vec![1, 2, 3]),
            })
            .collect();
        let candidate_started = std::time::Instant::now();
        let (candidate_root, undo) = tree.apply_sorted_ops_recorded(ops).unwrap();
        tree.apply_undo(&undo).unwrap();
        let restored = tree.root();
        eprintln!(
            "candidate of 300 appends priced and reverted in {:.2?} (root {})",
            candidate_started.elapsed(),
            hex::encode(candidate_root)
        );
        assert_eq!(restored, root);
    }

    #[test]
    fn undo_restores_the_root_and_the_layout_across_twig_boundaries() {
        let (_, mut leaf) = churned();
        let before = leaf.root();
        let before_slot = leaf.next_slot();
        let before_twigs = leaf.twig_count();
        let mut ops = sets(0..10, 0xE0);
        ops.extend(deletes(4000..4100));
        ops.extend(sets(7000..7000 + 3 * TWIG_SIZE as u64, 0xE1));
        let (after, undo) = leaf.apply_sorted_ops_recorded(ops).unwrap();
        assert_ne!(after, before);
        assert!(leaf.twig_count() > before_twigs + 2);
        leaf.apply_undo(&undo).unwrap();
        assert_eq!(leaf.root(), before);
        assert_eq!(leaf.next_slot(), before_slot);
        assert_eq!(leaf.twig_count(), before_twigs);
        assert_eq!(
            leaf.get(&key(0)),
            None,
            "deleted before the block, set by it, gone again"
        );
        assert_eq!(leaf.get(&key(150)), Some(&[0xD0, 150, 0][..]));
        assert_eq!(leaf.get(&key(4050)), Some(&[0xC0, 0xD2, 0x0F][..]));
        assert_eq!(leaf.get(&key(7000)), None);

        // Re-applying lands on the same root as a tree that never reverted.
        let (_, mut fresh) = churned();
        let ops2 = sets(0..40, 0xF0);
        let honest = fresh.apply_sorted_ops(ops2.clone()).unwrap();
        assert_eq!(leaf.apply_sorted_ops(ops2).unwrap(), honest);
    }

    #[test]
    fn undo_records_stack_newest_first_and_match_the_full_tree() {
        let (mut full, mut leaf) = churned();
        let root0 = leaf.root();
        let (r1, u1) = leaf.apply_sorted_ops_recorded(sets(0..300, 0xB1)).unwrap();
        let (r2, u2) = leaf.apply_sorted_ops_recorded(deletes(100..600)).unwrap();
        let (r3, u3) = leaf
            .apply_sorted_ops_recorded(sets(8000..8000 + TWIG_SIZE as u64 + 7, 0xB3))
            .unwrap();
        assert_eq!(full.apply_sorted_ops(sets(0..300, 0xB1)).unwrap(), r1);
        assert_eq!(full.apply_sorted_ops(deletes(100..600)).unwrap(), r2);
        assert_eq!(
            full.apply_sorted_ops(sets(8000..8000 + TWIG_SIZE as u64 + 7, 0xB3))
                .unwrap(),
            r3
        );
        leaf.apply_undo(&u3).unwrap();
        assert_eq!(leaf.root(), r2);
        leaf.apply_undo(&u2).unwrap();
        assert_eq!(leaf.root(), r1);
        leaf.apply_undo(&u1).unwrap();
        assert_eq!(leaf.root(), root0);
        assert!(matches!(
            leaf.apply_undo(&u1),
            Err(QmdbUndoError::Ahead { .. }) | Err(QmdbUndoError::EntryMismatch { .. })
        ));
    }

    #[test]
    fn a_foreign_undo_record_is_refused_without_mutating() {
        let (_, mut leaf) = churned();
        let root = leaf.root();
        let (_, mut other) = churned();
        other.apply_sorted_ops(sets(0..1, 0xEE)).unwrap();
        let (_, foreign) = other.apply_sorted_ops_recorded(sets(0..1, 0xEF)).unwrap();
        assert!(matches!(
            leaf.apply_undo(&foreign),
            Err(QmdbUndoError::Ahead { .. })
        ));
        assert_eq!(leaf.root(), root);
    }

    #[test]
    fn proofs_from_the_open_twig_verify_and_match_the_full_tree() {
        let (full, mut leaf) = churned();
        let root = leaf.root();
        let open_id = leaf.open.as_ref().unwrap().id;
        let mut proved = 0;
        for n in 0..5000u64 {
            let k = key(n);
            match leaf.prove(&k) {
                Some(proof) => {
                    assert_eq!(Some(proof.clone()), full.prove(&k));
                    assert!(proof.verify_for_key(&root, &k));
                    assert_eq!(proof.slot as usize / TWIG_SIZE, open_id);
                    proved += 1;
                }
                None => assert!(
                    full.prove(&k)
                        .is_none_or(|p| p.slot as usize / TWIG_SIZE != open_id)
                ),
            }
        }
        assert!(proved > 0);
        assert_eq!(full.root(), root);
    }

    #[test]
    fn an_empty_tree_has_the_null_root_and_grows_like_the_full_one() {
        let mut leaf = QmdbLeafTree::new();
        let mut full = QmdbCompatTree::new();
        assert_eq!(leaf.root(), NULL_HASH);
        assert_eq!(leaf.root(), full.root());
        for i in 0..5u64 {
            let ops = sets(i * 1000..(i + 1) * 1000, i as u8);
            assert_eq!(
                full.apply_sorted_ops(ops.clone()).unwrap(),
                leaf.apply_sorted_ops(ops).unwrap()
            );
        }
    }
}
