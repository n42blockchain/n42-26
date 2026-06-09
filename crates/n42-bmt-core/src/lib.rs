//! Pure-blake3 Sparse Binary Merkle Tree engine + proof verification.
//!
//! Zero reth/mdbx dependencies, so `n42-mobile` / FFI can verify SBMT proofs
//! without pulling the storage stack. Contains the tree engine ([`Sbmt`]), the
//! in-shard proof ([`BmtProof`]), the depth-4 shard-root merkle, and the
//! end-to-end [`ShardedBmtProof`] (the type a mobile client verifies against a
//! block's combined state root).
//!
//! See `crates/n42-jmt/src/sharded_bmt.rs` for the sharded *builder* side
//! (`ShardedSbmt`), which depends on this crate.

use std::cell::Cell;

/// 32-byte hash or key.
pub type Hash = [u8; 32];

/// Hash of an empty subtree (all zeroes).
pub const EMPTY_HASH: Hash = [0u8; 32];

/// Number of shards (depth-4 shard-root merkle ⇒ 16 leaves).
pub const SHARD_COUNT: usize = 16;

const LEAF_PREFIX: u8 = 0x00;
const INTERNAL_PREFIX: u8 = 0x01;

#[inline]
fn hash_leaf(key: &Hash, value_hash: &Hash) -> Hash {
    let mut h = blake3::Hasher::new();
    h.update(&[LEAF_PREFIX]);
    h.update(key);
    h.update(value_hash);
    *h.finalize().as_bytes()
}

#[inline]
fn hash_internal(left: &Hash, right: &Hash) -> Hash {
    let mut h = blake3::Hasher::new();
    h.update(&[INTERNAL_PREFIX]);
    h.update(left);
    h.update(right);
    *h.finalize().as_bytes()
}

/// Hash a raw value blob into the 32-byte leaf value hash.
#[inline]
pub fn hash_value(value: &[u8]) -> Hash {
    *blake3::hash(value).as_bytes()
}

/// Bit at `depth` of `key`, MSB-first. `false` = left (0), `true` = right (1).
#[inline]
fn bit(key: &Hash, depth: usize) -> bool {
    (key[depth / 8] >> (7 - (depth % 8))) & 1 == 1
}

type Link = Option<Box<Node>>;

enum Node {
    Leaf {
        key: Hash,
        value_hash: Hash,
    },
    Internal {
        left: Link,
        right: Link,
        cache: Cell<Option<Hash>>,
    },
}

#[inline]
fn link_hash(link: &Link) -> Hash {
    match link {
        None => EMPTY_HASH,
        Some(node) => match node.as_ref() {
            Node::Leaf { key, value_hash } => hash_leaf(key, value_hash),
            Node::Internal {
                left,
                right,
                cache,
            } => {
                if let Some(h) = cache.get() {
                    return h;
                }
                let h = hash_internal(&link_hash(left), &link_hash(right));
                cache.set(Some(h));
                h
            }
        },
    }
}

#[inline]
fn new_internal(left: Link, right: Link) -> Link {
    Some(Box::new(Node::Internal {
        left,
        right,
        cache: Cell::new(None),
    }))
}

fn insert(link: &mut Link, key: Hash, value_hash: Hash, depth: usize) -> bool {
    match link.take() {
        None => {
            *link = Some(Box::new(Node::Leaf { key, value_hash }));
            true
        }
        Some(node) => match *node {
            Node::Leaf {
                key: ek,
                value_hash: ev,
            } => {
                if ek == key {
                    *link = Some(Box::new(Node::Leaf { key, value_hash }));
                    false
                } else {
                    let mut internal = new_internal(None, None);
                    descend_insert(&mut internal, ek, ev, depth);
                    descend_insert(&mut internal, key, value_hash, depth);
                    *link = internal;
                    true
                }
            }
            Node::Internal {
                mut left,
                mut right,
                ..
            } => {
                let added = if bit(&key, depth) {
                    insert(&mut right, key, value_hash, depth + 1)
                } else {
                    insert(&mut left, key, value_hash, depth + 1)
                };
                *link = new_internal(left, right);
                added
            }
        },
    }
}

fn descend_insert(link: &mut Link, key: Hash, value_hash: Hash, depth: usize) {
    if let Some(node) = link.as_mut()
        && let Node::Internal { left, right, cache } = node.as_mut()
    {
        cache.set(None);
        if bit(&key, depth) {
            insert(right, key, value_hash, depth + 1);
        } else {
            insert(left, key, value_hash, depth + 1);
        }
    }
}

fn remove(link: &mut Link, key: &Hash, depth: usize) -> bool {
    match link.take() {
        None => false,
        Some(node) => match *node {
            Node::Leaf {
                key: ek,
                value_hash: ev,
            } => {
                if ek == *key {
                    *link = None;
                    true
                } else {
                    *link = Some(Box::new(Node::Leaf {
                        key: ek,
                        value_hash: ev,
                    }));
                    false
                }
            }
            Node::Internal {
                mut left,
                mut right,
                ..
            } => {
                let removed = if bit(key, depth) {
                    remove(&mut right, key, depth + 1)
                } else {
                    remove(&mut left, key, depth + 1)
                };
                *link = collapse(left, right);
                removed
            }
        },
    }
}

fn collapse(left: Link, right: Link) -> Link {
    match (left, right) {
        (None, None) => None,
        (Some(l), None) if matches!(l.as_ref(), Node::Leaf { .. }) => Some(l),
        (None, Some(r)) if matches!(r.as_ref(), Node::Leaf { .. }) => Some(r),
        (left, right) => new_internal(left, right),
    }
}

/// A Sparse Binary Merkle Tree (path-compressed, blake3, 256-bit keys).
#[derive(Default)]
pub struct Sbmt {
    root: Link,
    len: usize,
}

impl Sbmt {
    /// Create an empty tree.
    pub fn new() -> Self {
        Self {
            root: None,
            len: 0,
        }
    }

    /// Number of live leaves.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the tree has no leaves.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Root hash (`EMPTY_HASH` for an empty tree).
    pub fn root_hash(&self) -> Hash {
        link_hash(&self.root)
    }

    /// Insert or update a key with a pre-hashed value.
    pub fn insert_hashed(&mut self, key: Hash, value_hash: Hash) {
        if insert(&mut self.root, key, value_hash, 0) {
            self.len += 1;
        }
    }

    /// Insert or update a key with a raw value blob (hashed internally).
    pub fn insert(&mut self, key: Hash, value: &[u8]) {
        self.insert_hashed(key, hash_value(value));
    }

    /// Remove a key. Returns true if it existed.
    pub fn remove(&mut self, key: &Hash) -> bool {
        if remove(&mut self.root, key, 0) {
            self.len -= 1;
            true
        } else {
            false
        }
    }

    /// Apply a batch of `(key, Option<value>)` updates (`None` = delete).
    pub fn apply_batch(&mut self, updates: &[(Hash, Option<Vec<u8>>)]) {
        for (key, value) in updates {
            match value {
                Some(v) => self.insert(*key, v),
                None => {
                    self.remove(key);
                }
            }
        }
    }

    /// Look up a key's value hash.
    pub fn get(&self, key: &Hash) -> Option<Hash> {
        let mut link = &self.root;
        let mut depth = 0;
        loop {
            match link {
                None => return None,
                Some(node) => match node.as_ref() {
                    Node::Leaf {
                        key: ek,
                        value_hash,
                    } => return if ek == key { Some(*value_hash) } else { None },
                    Node::Internal { left, right, .. } => {
                        link = if bit(key, depth) { right } else { left };
                        depth += 1;
                    }
                },
            }
        }
    }

    /// Generate an inclusion/exclusion proof for `key`.
    pub fn prove(&self, key: Hash) -> BmtProof {
        let mut siblings = Vec::new();
        let mut link = &self.root;
        let mut depth = 0;
        loop {
            match link {
                None => {
                    return BmtProof {
                        key,
                        value_hash: None,
                        siblings,
                        other_leaf: None,
                    };
                }
                Some(node) => match node.as_ref() {
                    Node::Leaf {
                        key: ek,
                        value_hash,
                    } => {
                        return if *ek == key {
                            BmtProof {
                                key,
                                value_hash: Some(*value_hash),
                                siblings,
                                other_leaf: None,
                            }
                        } else {
                            BmtProof {
                                key,
                                value_hash: None,
                                siblings,
                                other_leaf: Some((*ek, *value_hash)),
                            }
                        };
                    }
                    Node::Internal { left, right, .. } => {
                        if bit(&key, depth) {
                            siblings.push(link_hash(left));
                            link = right;
                        } else {
                            siblings.push(link_hash(right));
                            link = left;
                        }
                        depth += 1;
                    }
                },
            }
        }
    }
}

/// An inclusion or exclusion proof for a single key within one shard's tree.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BmtProof {
    pub key: Hash,
    pub value_hash: Option<Hash>,
    /// Sibling hashes, top-down (root → leaf).
    pub siblings: Vec<Hash>,
    pub other_leaf: Option<(Hash, Hash)>,
}

impl BmtProof {
    /// Whether this is an inclusion proof.
    pub fn is_inclusion(&self) -> bool {
        self.value_hash.is_some()
    }

    /// Number of sibling hashes on the authentication path (= proof depth).
    pub fn path_len(&self) -> usize {
        self.siblings.len()
    }

    /// Estimated serialized size in bytes.
    pub fn encoded_len(&self) -> usize {
        32 + 1
            + if self.value_hash.is_some() { 32 } else { 0 }
            + 4
            + self.siblings.len() * 32
            + 1
            + if self.other_leaf.is_some() { 64 } else { 0 }
    }

    /// Verify the proof against a shard `root`, given the expected value hash.
    pub fn verify(&self, root: &Hash, expected_value_hash: Option<Hash>) -> bool {
        if self.value_hash != expected_value_hash {
            return false;
        }
        let mut cur = match (self.value_hash, self.other_leaf) {
            (Some(vh), None) => hash_leaf(&self.key, &vh),
            (None, None) => EMPTY_HASH,
            (None, Some((ok, ovh))) => {
                if !shares_prefix(&self.key, &ok, self.siblings.len()) {
                    return false;
                }
                hash_leaf(&ok, &ovh)
            }
            (Some(_), Some(_)) => return false,
        };
        for depth in (0..self.siblings.len()).rev() {
            let sib = self.siblings[depth];
            cur = if bit(&self.key, depth) {
                hash_internal(&sib, &cur)
            } else {
                hash_internal(&cur, &sib)
            };
        }
        cur == *root
    }
}

#[inline]
fn shares_prefix(a: &Hash, b: &Hash, depth: usize) -> bool {
    (0..depth).all(|d| bit(a, d) == bit(b, d))
}

// ---------------------------------------------------------------------------
// Shard-root commitment: depth-4 binary merkle over the 16 shard roots.
// ---------------------------------------------------------------------------

#[inline]
fn hash_pair(a: &Hash, b: &Hash) -> Hash {
    let mut h = blake3::Hasher::new();
    h.update(a);
    h.update(b);
    *h.finalize().as_bytes()
}

/// Merkle root over the shard roots (16 → 8 → 4 → 2 → 1, depth 4).
pub fn shard_tree_root(leaves: &[Hash]) -> Hash {
    let mut level: Vec<Hash> = leaves.to_vec();
    while level.len() > 1 {
        level = level.chunks(2).map(|c| hash_pair(&c[0], &c[1])).collect();
    }
    level[0]
}

/// Authentication path (siblings, bottom-up) from `index` to the shard-tree root.
pub fn shard_tree_path(leaves: &[Hash], index: usize) -> Vec<Hash> {
    let mut path = Vec::new();
    let mut level: Vec<Hash> = leaves.to_vec();
    let mut idx = index;
    while level.len() > 1 {
        path.push(level[idx ^ 1]);
        level = level.chunks(2).map(|c| hash_pair(&c[0], &c[1])).collect();
        idx >>= 1;
    }
    path
}

/// Recompute the shard-tree root from a leaf shard root + its authentication path.
pub fn shard_tree_root_from_path(leaf: Hash, index: usize, path: &[Hash]) -> Hash {
    let mut cur = leaf;
    let mut idx = index;
    for sib in path {
        cur = if idx & 1 == 0 {
            hash_pair(&cur, sib)
        } else {
            hash_pair(sib, &cur)
        };
        idx >>= 1;
    }
    cur
}

/// Verification error for [`ShardedBmtProof`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BmtVerifyError {
    #[error("shard index {0} out of range (max 15)")]
    ShardIndexOutOfRange(u8),
    #[error("combined root mismatch")]
    CombinedRootMismatch,
    #[error("value does not match committed value hash")]
    ValueHashMismatch,
    #[error("in-shard proof verification failed")]
    InShardProofFailed,
}

/// A self-contained proof for a key in a sharded SBMT.
///
/// Carries the target shard root + its depth-4 shard-tree path, the in-shard
/// [`BmtProof`], and the raw value. Verifiable with only the block header's
/// combined state root — no tree access (this is what a mobile client runs).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ShardedBmtProof {
    pub shard_index: u8,
    pub shard_root: Hash,
    pub shard_path: Vec<Hash>,
    pub inner: BmtProof,
    pub value: Option<Vec<u8>>,
}

impl ShardedBmtProof {
    /// Verify against a known combined root hash (`[u8; 32]`).
    pub fn verify(&self, combined_root: &Hash) -> Result<(), BmtVerifyError> {
        if self.shard_index as usize >= SHARD_COUNT {
            return Err(BmtVerifyError::ShardIndexOutOfRange(self.shard_index));
        }
        let computed = shard_tree_root_from_path(
            self.shard_root,
            self.shard_index as usize,
            &self.shard_path,
        );
        if computed != *combined_root {
            return Err(BmtVerifyError::CombinedRootMismatch);
        }
        let expected_vh = self.value.as_ref().map(|v| hash_value(v));
        if self.inner.value_hash != expected_vh {
            return Err(BmtVerifyError::ValueHashMismatch);
        }
        if !self.inner.verify(&self.shard_root, expected_vh) {
            return Err(BmtVerifyError::InShardProofFailed);
        }
        Ok(())
    }

    /// Estimated serialized size in bytes.
    pub fn estimated_size(&self) -> usize {
        1 + 32
            + 4
            + self.shard_path.len() * 32
            + self.inner.encoded_len()
            + self.value.as_ref().map_or(0, |v| v.len())
    }

    /// Bytes of the in-shard merkle authentication path.
    pub fn path_bytes(&self) -> usize {
        self.inner.path_len() * 32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(byte: u8) -> Hash {
        [byte; 32]
    }

    fn key_from(seed: u64) -> Hash {
        *blake3::hash(&seed.to_le_bytes()).as_bytes()
    }

    #[test]
    fn empty_and_single() {
        let mut t = Sbmt::new();
        assert!(t.is_empty());
        assert_eq!(t.root_hash(), EMPTY_HASH);
        t.insert(k(0xAB), b"hello");
        assert_eq!(t.get(&k(0xAB)), Some(hash_value(b"hello")));
    }

    #[test]
    fn order_independent_root() {
        let keys: Vec<Hash> = (0..64).map(key_from).collect();
        let mut a = Sbmt::new();
        for (i, key) in keys.iter().enumerate() {
            a.insert(*key, &i.to_le_bytes());
        }
        let mut b = Sbmt::new();
        for (i, key) in keys.iter().enumerate().rev() {
            b.insert(*key, &i.to_le_bytes());
        }
        assert_eq!(a.root_hash(), b.root_hash());
    }

    #[test]
    fn delete_matches_fresh_build() {
        let keys: Vec<Hash> = (0..50).map(key_from).collect();
        let victim = keys[17];
        let mut full = Sbmt::new();
        for (i, key) in keys.iter().enumerate() {
            full.insert(*key, &i.to_le_bytes());
        }
        full.remove(&victim);
        let mut without = Sbmt::new();
        for (i, key) in keys.iter().enumerate() {
            if *key != victim {
                without.insert(*key, &i.to_le_bytes());
            }
        }
        assert_eq!(full.root_hash(), without.root_hash());
    }

    #[test]
    fn bmt_proof_inclusion_exclusion() {
        let mut t = Sbmt::new();
        for i in 0..100u64 {
            t.insert(key_from(i), &i.to_le_bytes());
        }
        let root = t.root_hash();
        let key = key_from(42);
        let vh = hash_value(&42u64.to_le_bytes());
        assert!(t.prove(key).verify(&root, Some(vh)));
        assert!(t.prove(key_from(99999)).verify(&root, None));
    }

    #[test]
    fn shard_tree_path_roundtrip() {
        let leaves: Vec<Hash> = (0..SHARD_COUNT as u64).map(key_from).collect();
        let root = shard_tree_root(&leaves);
        for i in 0..SHARD_COUNT {
            let path = shard_tree_path(&leaves, i);
            assert_eq!(path.len(), 4, "depth-4 tree");
            assert_eq!(shard_tree_root_from_path(leaves[i], i, &path), root);
        }
    }
}
