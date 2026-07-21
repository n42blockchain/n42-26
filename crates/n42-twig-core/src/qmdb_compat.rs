//! QMDB split-commitment compatibility core.
//!
//! gov5 QMDB freezes an appended leaf forever. Updates and deletes only alter
//! the twig's active bitmap, and a twig commits `hash(leaf_root, bits_root)`.
//! The existing [`crate::TwigTree`] intentionally predates that representation
//! and nulls inactive leaves, so it must not be used to import replay-v2 QMDB
//! state. This small, isolated core is the compatibility baseline.

use std::collections::HashMap;

use crate::{Hash, NULL_HASH, TWIG_HEIGHT, TWIG_SIZE, hash_leaf, hash_node, null_level};

const BITS_PREFIX: u8 = 0x03;
const BITS_BYTES: usize = TWIG_SIZE / 8;
const PROOF_CODEC_VERSION: u8 = 0x02;
const MAX_UPPER_PATH: usize = 64;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct QmdbEntrySnapshot {
    pub key: Hash,
    pub value: Vec<u8>,
    pub active: bool,
}

/// Portable, positional QMDB state. Every slot below `next_slot` must be
/// present, including dead slots: their frozen leaves are consensus-relevant.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct QmdbSnapshot {
    pub next_slot: u64,
    pub entries: Vec<QmdbEntrySnapshot>,
}

/// The position-tagged form of gov5's `qmdb.SlotEntry`. This is the direct
/// importer boundary for a replay-v2 exporter: slot position is consensus
/// data, not an implementation detail.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct QmdbSlotEntry {
    pub slot: u64,
    pub key: Hash,
    pub value: Vec<u8>,
    pub active: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct QmdbSlotSnapshot {
    pub next_slot: u64,
    pub entries: Vec<QmdbSlotEntry>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum QmdbSnapshotError {
    #[error("QMDB snapshot has {entries} entries but next_slot is {next_slot}")]
    NonPositional { entries: usize, next_slot: u64 },
    #[error("QMDB snapshot has duplicate active key {0:?}")]
    DuplicateActiveKey(Hash),
    #[error("QMDB slot log is not contiguous: expected slot {expected}, got {got}")]
    NonContiguousSlotLog { expected: u64, got: u64 },
}

/// A gov5 QMDB v2 membership proof.  Its byte encoding is deliberately kept
/// separate from the legacy [`crate::TwigProof`]: QMDB commits a frozen leaf
/// tree plus an active-bits root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QmdbProof {
    pub key: Hash,
    pub value: Vec<u8>,
    pub slot: u64,
    pub twig_path: [Hash; TWIG_HEIGHT],
    pub active_bits: [u8; BITS_BYTES],
    pub upper_path: Vec<Hash>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum QmdbProofCodecError {
    #[error("QMDB proof is truncated at byte {offset}")]
    Truncated { offset: usize },
    #[error("QMDB proof version {0:#x} is unsupported")]
    UnsupportedVersion(u8),
    #[error("QMDB proof twig height {got} does not equal {expected}")]
    WrongTwigHeight { got: u8, expected: usize },
    #[error("QMDB proof upper path length {0} exceeds {MAX_UPPER_PATH}")]
    UpperPathTooLong(usize),
    #[error("QMDB proof value length overflows usize")]
    ValueLengthOverflow,
    #[error("QMDB proof has {0} trailing bytes")]
    TrailingBytes(usize),
}

impl QmdbProof {
    /// Decode the exact v2 proof layout emitted by gov5 `qmdb.Proof.Marshal`.
    /// The transport envelope (SSZ/snappy/RPC) is intentionally outside this
    /// low-level verifier.
    pub fn decode(bytes: &[u8]) -> Result<Self, QmdbProofCodecError> {
        let mut pos = 0;
        let version = read_u8(bytes, &mut pos)?;
        if version != PROOF_CODEC_VERSION {
            return Err(QmdbProofCodecError::UnsupportedVersion(version));
        }
        let key = read_hash(bytes, &mut pos)?;
        let slot = u64::from_le_bytes(read_array(bytes, &mut pos)?);
        let twig_height = read_u8(bytes, &mut pos)?;
        if twig_height as usize != TWIG_HEIGHT {
            return Err(QmdbProofCodecError::WrongTwigHeight {
                got: twig_height,
                expected: TWIG_HEIGHT,
            });
        }
        let mut twig_path = [NULL_HASH; TWIG_HEIGHT];
        for hash in &mut twig_path {
            *hash = read_hash(bytes, &mut pos)?;
        }
        let active_bits = read_array(bytes, &mut pos)?;
        let upper_len = read_u8(bytes, &mut pos)? as usize;
        if upper_len > MAX_UPPER_PATH {
            return Err(QmdbProofCodecError::UpperPathTooLong(upper_len));
        }
        let mut upper_path = Vec::with_capacity(upper_len);
        for _ in 0..upper_len {
            upper_path.push(read_hash(bytes, &mut pos)?);
        }
        let value_len = u32::from_le_bytes(read_array(bytes, &mut pos)?);
        let value_len =
            usize::try_from(value_len).map_err(|_| QmdbProofCodecError::ValueLengthOverflow)?;
        let value = read_bytes(bytes, &mut pos, value_len)?.to_vec();
        if pos != bytes.len() {
            return Err(QmdbProofCodecError::TrailingBytes(bytes.len() - pos));
        }
        Ok(Self {
            key,
            value,
            slot,
            twig_path,
            active_bits,
            upper_path,
        })
    }

    /// Verify membership against a QMDB world root, including the active-bit
    /// assertion which distinguishes a frozen dead leaf from a live value.
    pub fn verify(&self, root: &Hash) -> bool {
        let mut local = (self.slot as usize) % TWIG_SIZE;
        if self.active_bits[local / 8] & (1 << (local % 8)) == 0 {
            return false;
        }
        let mut node = hash_leaf(&self.key, &self.value);
        for sibling in &self.twig_path {
            node = if local & 1 == 0 {
                hash_node(&node, sibling)
            } else {
                hash_node(sibling, &node)
            };
            local >>= 1;
        }
        node = hash_node(&node, &hash_bits(&self.active_bits));
        let mut twig_id = (self.slot as usize) / TWIG_SIZE;
        for sibling in &self.upper_path {
            node = if twig_id & 1 == 0 {
                hash_node(&node, sibling)
            } else {
                hash_node(sibling, &node)
            };
            twig_id >>= 1;
        }
        node == *root
    }
}

fn read_u8(bytes: &[u8], pos: &mut usize) -> Result<u8, QmdbProofCodecError> {
    Ok(*read_bytes(bytes, pos, 1)?
        .first()
        .expect("one requested byte is present"))
}

fn read_hash(bytes: &[u8], pos: &mut usize) -> Result<Hash, QmdbProofCodecError> {
    read_array(bytes, pos)
}

fn read_array<const N: usize>(
    bytes: &[u8],
    pos: &mut usize,
) -> Result<[u8; N], QmdbProofCodecError> {
    let mut out = [0; N];
    out.copy_from_slice(read_bytes(bytes, pos, N)?);
    Ok(out)
}

fn read_bytes<'a>(
    bytes: &'a [u8],
    pos: &mut usize,
    len: usize,
) -> Result<&'a [u8], QmdbProofCodecError> {
    let end = pos
        .checked_add(len)
        .filter(|end| *end <= bytes.len())
        .ok_or(QmdbProofCodecError::Truncated { offset: *pos })?;
    let out = &bytes[*pos..end];
    *pos = end;
    Ok(out)
}

#[derive(Clone)]
struct Entry {
    key: Hash,
    value: Vec<u8>,
    active: bool,
}

struct Twig {
    nodes: Box<[Hash; 2 * TWIG_SIZE]>,
    bits: [u8; BITS_BYTES],
    bits_root: Hash,
    root: Hash,
}

impl Twig {
    fn new(nulls: &[Hash; TWIG_HEIGHT + 1]) -> Self {
        let mut nodes = Box::new([NULL_HASH; 2 * TWIG_SIZE]);
        for (index, node) in nodes.iter_mut().enumerate().take(TWIG_SIZE).skip(1) {
            let depth = (u32::BITS - 1 - (index as u32).leading_zeros()) as usize;
            *node = nulls[TWIG_HEIGHT - depth];
        }
        let bits = [0u8; BITS_BYTES];
        let bits_root = hash_bits(&bits);
        let root = hash_node(&nodes[1], &bits_root);
        Self {
            nodes,
            bits,
            bits_root,
            root,
        }
    }

    fn set_leaf(&mut self, local: usize, leaf: Hash) {
        let mut node = TWIG_SIZE + local;
        self.nodes[node] = leaf;
        while node > 1 {
            node >>= 1;
            self.nodes[node] = hash_node(&self.nodes[node * 2], &self.nodes[node * 2 + 1]);
        }
        self.refresh_root();
    }

    fn set_active(&mut self, local: usize, active: bool) {
        let byte = local / 8;
        let mask = 1 << (local % 8);
        if active {
            self.bits[byte] |= mask;
        } else {
            self.bits[byte] &= !mask;
        }
        self.bits_root = hash_bits(&self.bits);
        self.refresh_root();
    }

    fn set_leaf_unchecked(&mut self, local: usize, leaf: Hash) {
        self.nodes[TWIG_SIZE + local] = leaf;
    }

    fn recompute(&mut self) {
        for start in (1..TWIG_SIZE).rev() {
            self.nodes[start] = hash_node(&self.nodes[start * 2], &self.nodes[start * 2 + 1]);
        }
        self.bits_root = hash_bits(&self.bits);
        self.refresh_root();
    }

    fn refresh_root(&mut self) {
        self.root = hash_node(&self.nodes[1], &self.bits_root);
    }
}

fn hash_bits(bits: &[u8; BITS_BYTES]) -> Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&[BITS_PREFIX]);
    hasher.update(bits);
    *hasher.finalize().as_bytes()
}

/// A correctness-first QMDB tree for cross-client bootstrap and vectors.
///
/// It deliberately rebuilds the small upper tree on root reads. gov5's
/// incremental upper-tree and eviction optimizations can be added after this
/// representation has complete replay-v2 vectors.
pub struct QmdbCompatTree {
    entries: Vec<Entry>,
    index: HashMap<Hash, u64>,
    twigs: Vec<Twig>,
    next_slot: u64,
}

impl Default for QmdbCompatTree {
    fn default() -> Self {
        Self::new()
    }
}

impl QmdbCompatTree {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            index: HashMap::new(),
            twigs: Vec::new(),
            next_slot: 0,
        }
    }

    pub fn next_slot(&self) -> u64 {
        self.next_slot
    }

    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn get(&self, key: &Hash) -> Option<&[u8]> {
        self.index
            .get(key)
            .map(|slot| self.entries[*slot as usize].value.as_slice())
    }

    /// Append a new frozen leaf, deactivating an earlier live slot for `key`.
    pub fn set(&mut self, key: Hash, value: Vec<u8>) {
        if let Some(old_slot) = self.index.get(&key).copied() {
            self.deactivate(old_slot);
        }
        let slot = self.next_slot;
        self.next_slot += 1;
        let twig_id = (slot as usize) / TWIG_SIZE;
        let local = (slot as usize) % TWIG_SIZE;
        self.ensure_twig(twig_id);
        self.twigs[twig_id].set_leaf(local, hash_leaf(&key, &value));
        self.twigs[twig_id].set_active(local, true);
        self.entries.push(Entry {
            key,
            value,
            active: true,
        });
        self.index.insert(key, slot);
    }

    /// Deactivate a key without erasing its frozen leaf.
    pub fn delete(&mut self, key: &Hash) -> bool {
        let Some(slot) = self.index.remove(key) else {
            return false;
        };
        self.deactivate(slot);
        true
    }

    pub fn root(&self) -> Hash {
        if self.twigs.is_empty() {
            return NULL_HASH;
        }
        let cap = self.twigs.len().next_power_of_two();
        let mut upper = vec![NULL_HASH; cap * 2];
        for (index, twig) in self.twigs.iter().enumerate() {
            upper[cap + index] = twig.root;
        }
        for index in (1..cap).rev() {
            upper[index] = hash_node(&upper[index * 2], &upper[index * 2 + 1]);
        }
        upper[1]
    }

    pub fn snapshot(&self) -> QmdbSnapshot {
        QmdbSnapshot {
            next_slot: self.next_slot,
            entries: self
                .entries
                .iter()
                .map(|entry| QmdbEntrySnapshot {
                    key: entry.key,
                    value: entry.value.clone(),
                    active: entry.active,
                })
                .collect(),
        }
    }

    pub fn from_snapshot(snapshot: &QmdbSnapshot) -> Result<Self, QmdbSnapshotError> {
        if snapshot.next_slot != snapshot.entries.len() as u64 {
            return Err(QmdbSnapshotError::NonPositional {
                entries: snapshot.entries.len(),
                next_slot: snapshot.next_slot,
            });
        }
        let mut tree = Self::new();
        tree.next_slot = snapshot.next_slot;
        for (slot, entry) in snapshot.entries.iter().enumerate() {
            let twig_id = slot / TWIG_SIZE;
            let local = slot % TWIG_SIZE;
            tree.ensure_twig(twig_id);
            tree.twigs[twig_id].set_leaf_unchecked(local, hash_leaf(&entry.key, &entry.value));
            if entry.active {
                if tree.index.insert(entry.key, slot as u64).is_some() {
                    return Err(QmdbSnapshotError::DuplicateActiveKey(entry.key));
                }
                tree.twigs[twig_id].set_active(local, true);
            }
            tree.entries.push(Entry {
                key: entry.key,
                value: entry.value.clone(),
                active: entry.active,
            });
        }
        for twig in &mut tree.twigs {
            twig.recompute();
        }
        Ok(tree)
    }

    /// Import a gov5-style `SnapshotLog` only when every append slot is
    /// present. A sparse live-key export is insufficient: a dead entry's
    /// frozen leaf still contributes to the QMDB root. Rejecting it is safer
    /// than silently creating a root that cannot match replay-v2 checkpoints.
    pub fn from_slot_snapshot(snapshot: &QmdbSlotSnapshot) -> Result<Self, QmdbSnapshotError> {
        let mut entries = Vec::with_capacity(snapshot.entries.len());
        for (expected, entry) in snapshot.entries.iter().enumerate() {
            let expected = expected as u64;
            if entry.slot != expected {
                return Err(QmdbSnapshotError::NonContiguousSlotLog {
                    expected,
                    got: entry.slot,
                });
            }
            entries.push(QmdbEntrySnapshot {
                key: entry.key,
                value: entry.value.clone(),
                active: entry.active,
            });
        }
        Self::from_snapshot(&QmdbSnapshot {
            next_slot: snapshot.next_slot,
            entries,
        })
    }

    fn ensure_twig(&mut self, twig_id: usize) {
        let nulls = null_level();
        while self.twigs.len() <= twig_id {
            self.twigs.push(Twig::new(&nulls));
        }
    }

    fn deactivate(&mut self, slot: u64) {
        let entry = &mut self.entries[slot as usize];
        if !entry.active {
            return;
        }
        entry.active = false;
        let twig_id = (slot as usize) / TWIG_SIZE;
        let local = (slot as usize) % TWIG_SIZE;
        self.twigs[twig_id].set_active(local, false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(byte: u8) -> Hash {
        [byte; 32]
    }

    #[test]
    fn split_commitment_freezes_deleted_leaf() {
        let mut tree = QmdbCompatTree::new();
        tree.set(key(1), b"first".to_vec());
        let inserted_root = tree.root();

        let nulls = null_level();
        let mut leaf_root = hash_leaf(&key(1), b"first");
        for sibling in nulls.iter().take(TWIG_HEIGHT) {
            leaf_root = hash_node(&leaf_root, sibling);
        }
        let mut active_bits = [0u8; BITS_BYTES];
        active_bits[0] = 1;
        assert_eq!(
            inserted_root,
            hash_node(&leaf_root, &hash_bits(&active_bits))
        );

        assert!(tree.delete(&key(1)));
        let deleted_root = tree.root();
        assert_ne!(inserted_root, deleted_root);
        // Crucially, the leaf root is still the original leaf hash; only the
        // bitmap changes. This is where legacy TwigTree intentionally differs.
        assert_eq!(
            deleted_root,
            hash_node(&leaf_root, &hash_bits(&[0; BITS_BYTES]))
        );
        assert_eq!(tree.next_slot(), 1);
    }

    #[test]
    fn snapshot_preserves_dead_slots_and_root() {
        let mut tree = QmdbCompatTree::new();
        tree.set(key(1), b"old".to_vec());
        tree.set(key(1), b"new".to_vec());
        tree.delete(&key(1));
        let root = tree.root();
        let snapshot = tree.snapshot();
        assert_eq!(snapshot.entries.len(), 2);
        assert!(snapshot.entries.iter().all(|entry| !entry.active));
        assert_eq!(
            QmdbCompatTree::from_snapshot(&snapshot).unwrap().root(),
            root
        );
    }

    #[test]
    fn slot_snapshot_preserves_positions_and_rejects_sparse_history() {
        let mut tree = QmdbCompatTree::new();
        tree.set(key(1), b"old".to_vec());
        tree.set(key(1), b"new".to_vec());
        let snapshot = tree.snapshot();
        let slot_snapshot = QmdbSlotSnapshot {
            next_slot: snapshot.next_slot,
            entries: snapshot
                .entries
                .iter()
                .enumerate()
                .map(|(slot, entry)| QmdbSlotEntry {
                    slot: slot as u64,
                    key: entry.key,
                    value: entry.value.clone(),
                    active: entry.active,
                })
                .collect(),
        };
        assert_eq!(
            QmdbCompatTree::from_slot_snapshot(&slot_snapshot)
                .unwrap()
                .root(),
            tree.root()
        );

        let mut sparse = slot_snapshot;
        sparse.entries.remove(0);
        assert!(matches!(
            QmdbCompatTree::from_slot_snapshot(&sparse),
            Err(QmdbSnapshotError::NonContiguousSlotLog {
                expected: 0,
                got: 1
            })
        ));
    }

    #[test]
    fn decodes_and_verifies_gov5_v2_proof_layout() {
        let mut tree = QmdbCompatTree::new();
        tree.set(key(7), b"proof-value".to_vec());
        let root = tree.root();

        let nulls = null_level();
        let mut encoded = Vec::new();
        encoded.push(PROOF_CODEC_VERSION);
        encoded.extend_from_slice(&key(7));
        encoded.extend_from_slice(&0_u64.to_le_bytes());
        encoded.push(TWIG_HEIGHT as u8);
        // Slot zero is a left child at every level, so every sibling is the
        // all-null subtree of the corresponding height.
        for sibling in nulls.iter().take(TWIG_HEIGHT) {
            encoded.extend_from_slice(sibling);
        }
        let mut bits = [0_u8; BITS_BYTES];
        bits[0] = 1;
        encoded.extend_from_slice(&bits);
        encoded.push(0); // one twig: no upper path
        encoded.extend_from_slice(&(b"proof-value".len() as u32).to_le_bytes());
        encoded.extend_from_slice(b"proof-value");

        let proof = QmdbProof::decode(&encoded).unwrap();
        assert!(proof.verify(&root));

        let mut dead = proof.clone();
        dead.active_bits[0] = 0;
        assert!(!dead.verify(&root));
        encoded.push(0);
        assert!(matches!(
            QmdbProof::decode(&encoded),
            Err(QmdbProofCodecError::TrailingBytes(1))
        ));
    }
}
