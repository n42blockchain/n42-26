//! Self-built Sparse Binary Merkle Tree (SBMT) — Phase 1 in-memory core.
//!
//! Decided as the production state-tree backend replacing the 16-ary `jmt 0.12`
//! JMT (devlog-59): a path-compressed binary merkle tree over 256-bit keys
//! (the blake3-hashed account/storage keys from [`crate::keys`]), hashed with
//! Blake3. Binary structure is zkVM-friendly (SP1), aligns with EIP-7864 /
//! AlDBaran SMT, and yields short, fixed-shape proofs.
//!
//! ## Compression invariant
//!
//! Any subtree containing 0 or 1 leaves is represented by `Empty` or that single
//! `Leaf` (never an internal chain). A leaf therefore hangs at the *shortest*
//! prefix that uniquely distinguishes it, so the root hash depends only on the
//! key set — not on insertion/deletion order. `insert` keeps a fresh key at the
//! current level and only splits on collision; `remove` re-collapses single-leaf
//! subtrees back up. This is the binary analogue of the JMT "0-or-1 leaf subtree"
//! rule.
//!
//! ## Hashing (domain-separated)
//!
//! - empty subtree → `EMPTY_HASH` (32 zero bytes)
//! - leaf          → `blake3(0x00 || key || value_hash)`
//! - internal      → `blake3(0x01 || left_hash || right_hash)`
//!
//! Phase 1 is the in-memory tree + correctness + benchmarking vs JMT. Sharding,
//! persistence, disk store, and mobile proof verification are follow-up phases.

use std::cell::Cell;

/// 32-byte hash or key.
pub type Hash = [u8; 32];

/// Hash of an empty subtree (all zeroes).
pub const EMPTY_HASH: Hash = [0u8; 32];

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
        /// Lazily-computed subtree hash; invalidated (None) on every rebuild of
        /// this node, so unchanged sibling subtrees keep their cached hash and
        /// `root_hash` only rehashes the touched path.
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

/// Insert/update `(key → value_hash)` into the subtree rooted at `link`.
fn insert(link: &mut Link, key: Hash, value_hash: Hash, depth: usize) -> bool {
    match link.take() {
        None => {
            *link = Some(Box::new(Node::Leaf { key, value_hash }));
            true // newly added
        }
        Some(node) => match *node {
            Node::Leaf {
                key: ek,
                value_hash: ev,
            } => {
                if ek == key {
                    *link = Some(Box::new(Node::Leaf { key, value_hash }));
                    false // updated in place
                } else {
                    // Collision at this prefix: split into an internal node and
                    // push both leaves down until their bits diverge.
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

/// Insert into an existing internal `Link` (used during a split).
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

/// Remove `key`. Returns true if it existed. Re-collapses single-leaf subtrees.
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

/// Re-establish the compression invariant after a removal: a subtree with a
/// single `Leaf` child collapses to that leaf; otherwise stay internal.
fn collapse(left: Link, right: Link) -> Link {
    match (left, right) {
        (None, None) => None,
        (Some(l), None) if matches!(l.as_ref(), Node::Leaf { .. }) => Some(l),
        (None, Some(r)) if matches!(r.as_ref(), Node::Leaf { .. }) => Some(r),
        (left, right) => new_internal(left, right),
    }
}

/// A Sparse Binary Merkle Tree.
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
                    // Empty slot on the path → key is absent.
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
                            // A different key occupies this prefix → exclusion.
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

/// An inclusion or exclusion proof for a single key.
///
/// - inclusion: `value_hash = Some(_)`, `other_leaf = None`
/// - exclusion (empty slot): both `None`
/// - exclusion (different leaf occupies the prefix): `value_hash = None`,
///   `other_leaf = Some((key, value_hash))`
#[derive(Debug, Clone, PartialEq, Eq)]
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

    /// Verify the proof against `root`. For inclusion, the caller passes the
    /// expected `value_hash`; for exclusion pass `None`.
    pub fn verify(&self, root: &Hash, expected_value_hash: Option<Hash>) -> bool {
        // The claimed value must match what the proof carries.
        if self.value_hash != expected_value_hash {
            return false;
        }

        // Hash of the node at the bottom of the proven path.
        let mut cur = match (self.value_hash, self.other_leaf) {
            (Some(vh), None) => hash_leaf(&self.key, &vh), // inclusion
            (None, None) => EMPTY_HASH,                    // exclusion: empty slot
            (None, Some((ok, ovh))) => {
                // exclusion: a different leaf sits here. Its key must actually
                // share the proven prefix, else the proof is malformed.
                if !shares_prefix(&self.key, &ok, self.siblings.len()) {
                    return false;
                }
                hash_leaf(&ok, &ovh)
            }
            (Some(_), Some(_)) => return false, // contradictory
        };

        // Fold sibling hashes bottom-up back to the root.
        for depth in (0..self.siblings.len()).rev() {
            let sib = self.siblings[depth];
            cur = if bit(&self.key, depth) {
                hash_internal(&sib, &cur) // key went right, sibling is left
            } else {
                hash_internal(&cur, &sib)
            };
        }
        cur == *root
    }
}

/// Whether `a` and `b` agree on their first `depth` bits.
#[inline]
fn shares_prefix(a: &Hash, b: &Hash, depth: usize) -> bool {
    (0..depth).all(|d| bit(a, d) == bit(b, d))
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
    fn empty_tree() {
        let t = Sbmt::new();
        assert!(t.is_empty());
        assert_eq!(t.root_hash(), EMPTY_HASH);
        assert_eq!(t.get(&k(1)), None);
    }

    #[test]
    fn single_insert_get() {
        let mut t = Sbmt::new();
        t.insert(k(0xAB), b"hello");
        assert_eq!(t.len(), 1);
        assert_eq!(t.get(&k(0xAB)), Some(hash_value(b"hello")));
        assert_ne!(t.root_hash(), EMPTY_HASH);
    }

    #[test]
    fn update_in_place_changes_root() {
        let mut t = Sbmt::new();
        t.insert(k(1), b"v0");
        let r0 = t.root_hash();
        t.insert(k(1), b"v1");
        assert_eq!(t.len(), 1, "update must not grow len");
        assert_ne!(t.root_hash(), r0);
        assert_eq!(t.get(&k(1)), Some(hash_value(b"v1")));
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

        assert_eq!(a.root_hash(), b.root_hash(), "root must not depend on order");
    }

    #[test]
    fn delete_restores_root() {
        // Inserting then deleting a key returns to the prior root (compression
        // invariant: single-leaf subtrees collapse back up).
        let mut t = Sbmt::new();
        for i in 0..32u64 {
            t.insert(key_from(i), &i.to_le_bytes());
        }
        let baseline = t.root_hash();

        let extra = key_from(9999);
        t.insert(extra, b"temp");
        assert_ne!(t.root_hash(), baseline);

        assert!(t.remove(&extra));
        assert_eq!(
            t.root_hash(),
            baseline,
            "root after insert+delete must match baseline"
        );
        assert_eq!(t.len(), 32);
    }

    #[test]
    fn delete_matches_fresh_build() {
        // A tree with key X removed must equal a tree built without X.
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
    fn inclusion_proof_verifies() {
        let mut t = Sbmt::new();
        for i in 0..100u64 {
            t.insert(key_from(i), &i.to_le_bytes());
        }
        let root = t.root_hash();

        let key = key_from(42);
        let vh = hash_value(&42u64.to_le_bytes());
        let proof = t.prove(key);
        assert!(proof.is_inclusion());
        assert_eq!(proof.value_hash, Some(vh));
        assert!(proof.verify(&root, Some(vh)));
        // Wrong value must fail.
        assert!(!proof.verify(&root, Some(hash_value(b"wrong"))));
    }

    #[test]
    fn exclusion_proof_verifies() {
        let mut t = Sbmt::new();
        for i in 0..100u64 {
            t.insert(key_from(i), &i.to_le_bytes());
        }
        let root = t.root_hash();

        let absent = key_from(123_456);
        let proof = t.prove(absent);
        assert!(!proof.is_inclusion());
        assert!(proof.verify(&root, None), "exclusion proof must verify");
    }

    #[test]
    fn tampered_proof_fails() {
        let mut t = Sbmt::new();
        for i in 0..100u64 {
            t.insert(key_from(i), &i.to_le_bytes());
        }
        let root = t.root_hash();
        let key = key_from(7);
        let vh = hash_value(&7u64.to_le_bytes());
        let mut proof = t.prove(key);
        if let Some(first) = proof.siblings.first_mut() {
            first[0] ^= 0xFF;
        }
        assert!(!proof.verify(&root, Some(vh)), "tampered sibling must fail");
    }
}
