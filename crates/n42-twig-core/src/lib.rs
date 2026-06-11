//! All-DRAM QMDB-style twig engine (AlDBaran variant), P1 scalar core.
//!
//! Zero reth/mdbx deps (blake3 + serde + thiserror only) so mobile/FFI can verify
//! twig proofs without the storage stack. This is the single-shard engine: a twig
//! forest (2048-leaf contiguous binary-heap subtrees) + an upper merkle over twig
//! roots, with an append-only slot model (root depends on append history; see
//! `docs/devlog-64` for the consensus-determinism invariants the sharded layer
//! must enforce).
//!
//! Ported from gov5 `lib/qmdb` (Go); P1 is scalar + all-in-DRAM (no SSD entry-log,
//! no eviction tiers, no compaction yet — those are P2+).

use std::collections::HashMap;

/// 32-byte hash / key.
pub type Hash = [u8; 32];

/// Leaves per twig (a complete binary subtree of height [`TWIG_HEIGHT`]).
pub const TWIG_SIZE: usize = 2048;
/// Twig subtree height (`log2(TWIG_SIZE)`).
pub const TWIG_HEIGHT: usize = 11;
/// All-zero hash (empty subtree / null leaf).
pub const NULL_HASH: Hash = [0u8; 32];

const LEAF_PREFIX: u8 = 0x01;

/// Leaf commitment: `blake3(0x01 || key || value)` — the value is folded directly
/// into the leaf hash (unified KV + Merkle: no separate value map).
#[inline]
pub fn hash_leaf(key: &Hash, value: &[u8]) -> Hash {
    let mut h = blake3::Hasher::new();
    h.update(&[LEAF_PREFIX]);
    h.update(key);
    h.update(value);
    *h.finalize().as_bytes()
}

/// Internal node: `blake3(left || right)` (no domain prefix, 64-byte input).
#[inline]
pub fn hash_node(left: &Hash, right: &Hash) -> Hash {
    let mut h = blake3::Hasher::new();
    h.update(left);
    h.update(right);
    *h.finalize().as_bytes()
}

/// One twig: a complete binary heap of `2*TWIG_SIZE` hashes. `nodes[1]` is the
/// twig root; internal nodes occupy `[1, TWIG_SIZE)`; leaves `[TWIG_SIZE, 2*TWIG_SIZE)`.
/// `children(j) = (2j, 2j+1)`. Boxed (128 KiB) to keep it off the stack.
struct Twig {
    nodes: Box<[Hash; 2 * TWIG_SIZE]>,
    live: usize,
    dirty: bool,
}

impl Twig {
    fn new() -> Self {
        Self {
            nodes: Box::new([NULL_HASH; 2 * TWIG_SIZE]),
            live: 0,
            dirty: false,
        }
    }

    #[inline]
    fn set_leaf(&mut self, local: usize, h: Hash) {
        self.nodes[TWIG_SIZE + local] = h;
        self.dirty = true;
    }

    #[inline]
    fn root(&self) -> Hash {
        self.nodes[1]
    }

    /// Bottom-up full recompute of all 2047 internal nodes from the leaves.
    /// Processing `j` from high to low guarantees children `(2j, 2j+1)` are done
    /// first (they are larger indices). An all-null twig folds to `null_twig_root`.
    fn recompute(&mut self) {
        for j in (1..TWIG_SIZE).rev() {
            self.nodes[j] = hash_node(&self.nodes[2 * j], &self.nodes[2 * j + 1]);
        }
        self.dirty = false;
    }

    /// Fast path for an all-null (live == 0) twig: set the root directly to the
    /// precomputed `null_twig_root` instead of folding 2047 null hashes. Same
    /// result as `recompute()` on all-null leaves.
    fn mark_empty(&mut self, null_twig_root: Hash) {
        self.nodes[1] = null_twig_root;
        self.dirty = false;
    }
}

struct Entry {
    /// Kept for P2 compaction (re-append live entries by keyHash). Unused in P1.
    #[allow(dead_code)]
    key: Hash,
    value: Vec<u8>,
    active: bool,
}

/// All-null twig commitment: `null_level[TWIG_HEIGHT]` = fold of NULL_HASH up 11
/// levels. An emptied twig commits to this (matches gov5's `nullTwigRoot`).
fn null_twig_root() -> Hash {
    let mut h = NULL_HASH;
    for _ in 0..TWIG_HEIGHT {
        h = hash_node(&h, &h);
    }
    h
}

/// Single-shard append-only twig tree. `set` assigns a monotonically increasing
/// slot; updating a key deactivates its old slot (nulls that leaf) and appends a
/// new entry. The world root is therefore a function of the append history.
pub struct TwigTree {
    entries: Vec<Entry>, // indexed by slot
    twigs: Vec<Twig>,
    index: HashMap<Hash, u64>, // key -> active slot
    next_slot: u64,
    null_twig_root: Hash,
    // Upper merkle over twig roots, rebuilt by `root()`; cached for `prove()`.
    upper: Vec<Hash>,
    up_cap: usize,
}

impl Default for TwigTree {
    fn default() -> Self {
        Self::new()
    }
}

impl TwigTree {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            twigs: Vec::new(),
            index: HashMap::new(),
            next_slot: 0,
            null_twig_root: null_twig_root(),
            upper: Vec::new(),
            up_cap: 0,
        }
    }

    /// Number of live (active) keys.
    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// Total slots ever appended (the append-history length).
    pub fn next_slot(&self) -> u64 {
        self.next_slot
    }

    fn deactivate(&mut self, slot: u64) {
        self.entries[slot as usize].active = false;
        let twig_id = (slot / TWIG_SIZE as u64) as usize;
        let local = (slot % TWIG_SIZE as u64) as usize;
        let t = &mut self.twigs[twig_id];
        t.set_leaf(local, NULL_HASH);
        t.live -= 1;
    }

    /// Insert or update `key -> value`. Deactivates the old slot if the key
    /// existed, then appends a fresh entry at `next_slot`.
    pub fn set(&mut self, key: Hash, value: &[u8]) {
        if let Some(&old) = self.index.get(&key) {
            self.deactivate(old);
        }
        let slot = self.next_slot;
        self.next_slot += 1;
        let twig_id = (slot / TWIG_SIZE as u64) as usize;
        let local = (slot % TWIG_SIZE as u64) as usize;
        while self.twigs.len() <= twig_id {
            self.twigs.push(Twig::new());
        }
        let leaf = hash_leaf(&key, value);
        let t = &mut self.twigs[twig_id];
        t.set_leaf(local, leaf);
        t.live += 1;
        debug_assert_eq!(self.entries.len() as u64, slot);
        self.entries.push(Entry {
            key,
            value: value.to_vec(),
            active: true,
        });
        self.index.insert(key, slot);
    }

    /// Delete `key`. Returns whether it was present.
    pub fn delete(&mut self, key: &Hash) -> bool {
        if let Some(slot) = self.index.remove(key) {
            self.deactivate(slot);
            true
        } else {
            false
        }
    }

    /// Read the current value for `key`.
    pub fn get(&self, key: &Hash) -> Option<&[u8]> {
        self.index
            .get(key)
            .map(|&slot| self.entries[slot as usize].value.as_slice())
    }

    /// Apply a block's `(key, Option<value>)` ops in **canonical (keyHash-sorted)
    /// order** — the consensus-determinism invariant: the append-slot root depends
    /// on order, so every node MUST apply the same block in the same order. Sorting
    /// by key makes the root independent of the input order. Keys within a block
    /// are expected unique (the `StateDiff`/sharded layer guarantees this).
    pub fn apply_batch(&mut self, ops: &[(Hash, Option<Vec<u8>>)]) {
        let mut order: Vec<usize> = (0..ops.len()).collect();
        order.sort_by_key(|&i| ops[i].0);
        for i in order {
            match &ops[i].1 {
                Some(v) => self.set(ops[i].0, v),
                None => {
                    self.delete(&ops[i].0);
                }
            }
        }
    }

    /// Deterministic compaction: re-append the live entries of every sparse twig
    /// (live ratio < `threshold`, excluding the active twig) at fresh slots in
    /// keyHash-sorted order, emptying those twigs. Returns the number of entries
    /// moved. **Changes the world root** (entries move to new slots) — must run at
    /// the same block boundary with the same threshold on every node (determinism
    /// invariant #2). Reclaims the value heap of the emptied slots.
    pub fn compact(&mut self, threshold: f64) -> usize {
        let active = (self.next_slot / TWIG_SIZE as u64) as usize;
        let mut sparse: Vec<usize> = Vec::new();
        for (id, t) in self.twigs.iter().enumerate() {
            if id == active || t.live == 0 {
                continue;
            }
            if (t.live as f64) / (TWIG_SIZE as f64) < threshold {
                sparse.push(id);
            }
        }
        // Collect live entries (deterministic keyHash order).
        let mut live: Vec<(Hash, Vec<u8>)> = Vec::new();
        for &id in &sparse {
            let base = id as u64 * TWIG_SIZE as u64;
            for local in 0..TWIG_SIZE as u64 {
                let e = &self.entries[(base + local) as usize];
                if e.active {
                    live.push((e.key, e.value.clone()));
                }
            }
        }
        live.sort_by_key(|p| p.0);
        let moved = live.len();
        // Re-append (deactivates old slots, appends to the active twig forward).
        for (k, v) in live {
            self.set(k, &v);
        }
        // Reclaim the value heap of the now fully-dead sparse twig slots.
        for &id in &sparse {
            let base = id as u64 * TWIG_SIZE as u64;
            for local in 0..TWIG_SIZE as u64 {
                let slot = (base + local) as usize;
                self.entries[slot].value = Vec::new();
            }
        }
        moved
    }

    /// Recompute dirty twig roots + rebuild the upper tree, returning the world
    /// root. Must be called before `prove()` (it refreshes the cached upper tree).
    pub fn root(&mut self) -> Hash {
        if self.twigs.is_empty() {
            self.upper.clear();
            self.up_cap = 0;
            return NULL_HASH;
        }
        for t in &mut self.twigs {
            if t.dirty {
                if t.live == 0 {
                    t.mark_empty(self.null_twig_root);
                } else {
                    t.recompute();
                }
            }
        }
        let n = self.twigs.len();
        let up_cap = n.next_power_of_two();
        let mut upper = vec![NULL_HASH; 2 * up_cap];
        for (i, t) in self.twigs.iter().enumerate() {
            upper[up_cap + i] = t.root();
        }
        for j in (1..up_cap).rev() {
            upper[j] = hash_node(&upper[2 * j], &upper[2 * j + 1]);
        }
        let world = upper[1];
        self.upper = upper;
        self.up_cap = up_cap;
        world
    }

    /// Build an inclusion proof for a live `key` (twig path + upper path). Returns
    /// `None` if the key is absent. Requires a prior `root()` call (uses the cached
    /// upper tree); debug-asserts that twigs are clean.
    pub fn prove(&self, key: &Hash) -> Option<TwigProof> {
        let &slot = self.index.get(key)?;
        debug_assert!(
            self.up_cap >= self.twigs.len() && !self.twigs.iter().any(|t| t.dirty),
            "call root() before prove()"
        );
        let value = self.entries[slot as usize].value.clone();
        let twig_id = (slot / TWIG_SIZE as u64) as usize;
        let local = (slot % TWIG_SIZE as u64) as usize;

        let nodes = &self.twigs[twig_id].nodes;
        let mut twig_path = [NULL_HASH; TWIG_HEIGHT];
        let mut j = TWIG_SIZE + local;
        for sib in twig_path.iter_mut() {
            *sib = nodes[j ^ 1];
            j >>= 1;
        }

        let mut upper_path = Vec::new();
        let mut j = self.up_cap + twig_id;
        while j > 1 {
            upper_path.push(self.upper[j ^ 1]);
            j >>= 1;
        }

        Some(TwigProof {
            key: *key,
            value,
            slot,
            twig_path,
            upper_path,
        })
    }
}

/// Inclusion proof for one key in a [`TwigTree`]: the in-twig sibling path
/// (11 levels), the upper-tree sibling path, the global slot, and the raw value.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TwigProof {
    pub key: Hash,
    pub value: Vec<u8>,
    pub slot: u64,
    pub twig_path: [Hash; TWIG_HEIGHT],
    pub upper_path: Vec<Hash>,
}

impl TwigProof {
    /// Recompute the world root from the proof and compare to `root`. The
    /// leaf/twig/upper folds use the slot's bits to pick sibling sides.
    pub fn verify(&self, root: &Hash) -> bool {
        let mut node = hash_leaf(&self.key, &self.value);
        let mut idx = (self.slot % TWIG_SIZE as u64) as usize;
        for sib in &self.twig_path {
            node = if idx & 1 == 0 {
                hash_node(&node, sib)
            } else {
                hash_node(sib, &node)
            };
            idx >>= 1;
        }
        let mut tid = (self.slot / TWIG_SIZE as u64) as usize;
        for sib in &self.upper_path {
            node = if tid & 1 == 0 {
                hash_node(&node, sib)
            } else {
                hash_node(sib, &node)
            };
            tid >>= 1;
        }
        node == *root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(i: u64) -> Hash {
        *blake3::hash(&i.to_le_bytes()).as_bytes()
    }
    fn val(i: u64) -> Vec<u8> {
        i.to_be_bytes().to_vec()
    }

    #[test]
    fn empty_root_is_null() {
        let mut t = TwigTree::new();
        assert_eq!(t.root(), NULL_HASH);
    }

    #[test]
    fn single_insert_proof_roundtrip() {
        let mut t = TwigTree::new();
        t.set(key(1), &val(1));
        let root = t.root();
        assert_ne!(root, NULL_HASH);
        let p = t.prove(&key(1)).unwrap();
        assert!(p.verify(&root));
        // wrong root rejected
        assert!(!p.verify(&[0xFF; 32]));
        // absent key has no proof
        assert!(t.prove(&key(999)).is_none());
    }

    #[test]
    fn get_readback() {
        let mut t = TwigTree::new();
        t.set(key(7), b"hello");
        assert_eq!(t.get(&key(7)), Some(&b"hello"[..]));
        t.set(key(7), b"world"); // update
        assert_eq!(t.get(&key(7)), Some(&b"world"[..]));
        assert!(t.delete(&key(7)));
        assert_eq!(t.get(&key(7)), None);
    }

    #[test]
    fn multi_twig_proofs() {
        // > 2048 entries forces multiple twigs + a non-trivial upper tree.
        let mut t = TwigTree::new();
        let n = 5000u64;
        for i in 0..n {
            t.set(key(i), &val(i));
        }
        let root = t.root();
        assert!(t.twigs.len() >= 3, "expected multiple twigs");
        for i in [0u64, 1, 2047, 2048, 2049, 4095, 4096, 4999] {
            let p = t.prove(&key(i)).unwrap();
            assert!(p.verify(&root), "proof for key {i} must verify");
            assert_eq!(p.value, val(i));
        }
    }

    #[test]
    fn update_and_delete_change_root_and_verify() {
        let mut t = TwigTree::new();
        for i in 0..100 {
            t.set(key(i), &val(i));
        }
        let r0 = t.root();
        // update one key -> root changes, new value proves
        t.set(key(42), b"updated");
        let r1 = t.root();
        assert_ne!(r0, r1);
        let p = t.prove(&key(42)).unwrap();
        assert_eq!(p.value, b"updated");
        assert!(p.verify(&r1));
        // delete -> root changes, key gone
        t.delete(&key(42));
        let r2 = t.root();
        assert_ne!(r1, r2);
        assert!(t.prove(&key(42)).is_none());
    }

    /// Cross-language consistency: 5000 inserts (key=blake3(i_le), value=i_be8)
    /// must produce the EXACT root gov5's Go QMDB (`lib/qmdb`) produces for the
    /// same input — proves the twig/upper/leaf hashing + append layout are
    /// byte-faithful to the reference. Go root captured from `_twig_xcheck`.
    #[test]
    fn cross_check_root_vs_gov5_go() {
        let mut t = TwigTree::new();
        for i in 0..5000u64 {
            t.set(key(i), &val(i));
        }
        let root = t.root();
        let expected =
            hex_to_hash("b32e9a4b7039a6d6c87716c6a0ea4818e7c5c8a93e5ecb7c996c694e6a689912");
        assert_eq!(
            root, expected,
            "Rust twig root must match gov5 Go QMDB root for identical input"
        );
    }

    fn hex_to_hash(s: &str) -> Hash {
        let mut h = [0u8; 32];
        for (i, b) in h.iter_mut().enumerate() {
            *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).unwrap();
        }
        h
    }

    /// Cross-language: `apply_batch` (keyHash-sorted) of 0..5000 must match the
    /// root gov5's Go QMDB produces when Set in the same keyHash-sorted order —
    /// validates the canonical-ordering determinism invariant is byte-faithful.
    #[test]
    fn cross_check_sorted_batch_vs_gov5_go() {
        let ops: Vec<(Hash, Option<Vec<u8>>)> =
            (0..5000u64).map(|i| (key(i), Some(val(i)))).collect();
        let mut t = TwigTree::new();
        t.apply_batch(&ops);
        let expected =
            hex_to_hash("58671c9adadd83040c3966abdbb8fd06d7d7ea8def0c7f1d317799ea486526fa");
        assert_eq!(t.root(), expected, "apply_batch root must match gov5 sorted-insert root");
    }

    #[test]
    fn apply_batch_is_input_order_independent() {
        // Same op set, different INPUT order -> same root (canonical sort inside).
        let ops_fwd: Vec<(Hash, Option<Vec<u8>>)> =
            (0..1500u64).map(|i| (key(i), Some(val(i)))).collect();
        let ops_rev: Vec<(Hash, Option<Vec<u8>>)> = ops_fwd.iter().rev().cloned().collect();

        let mut a = TwigTree::new();
        a.apply_batch(&ops_fwd);
        let ra = a.root();

        let mut b = TwigTree::new();
        b.apply_batch(&ops_rev);
        let rb = b.root();

        assert_eq!(ra, rb, "apply_batch root must not depend on input order");
        // sanity: a proof verifies
        let p = a.prove(&key(700)).unwrap();
        assert!(p.verify(&ra));
    }

    #[test]
    fn compaction_correctness_and_determinism() {
        let build = || {
            let mut t = TwigTree::new();
            for i in 0..5000u64 {
                t.set(key(i), &val(i));
            }
            // Make twig 0 (slots 0..2047) and twig 1 (2048..4095) sparse.
            for i in 0..1900u64 {
                t.delete(&key(i));
            }
            for i in 2048..3900u64 {
                t.delete(&key(i));
            }
            t
        };

        let mut t = build();
        let live_before: usize = t.len();
        let _ = t.root();

        let moved = t.compact(0.5);
        let root_after = t.root();
        assert!(moved > 0, "should have moved live entries out of sparse twigs");

        // Live set unchanged; all live keys still readable + provable.
        assert_eq!(t.len(), live_before, "compaction must not change the live set");
        for i in [1950u64, 2000, 3950, 3999, 4500, 4999] {
            assert_eq!(t.get(&key(i)), Some(val(i).as_slice()), "key {i} live after compact");
            let p = t.prove(&key(i)).unwrap();
            assert!(p.verify(&root_after), "proof for {i} verifies vs post-compaction root");
        }
        // Deleted keys still gone.
        assert!(t.get(&key(10)).is_none());

        // Determinism: an identically-built tree, compacted the same way, yields
        // the same post-compaction root.
        let mut t2 = build();
        let _ = t2.root();
        t2.compact(0.5);
        assert_eq!(root_after, t2.root(), "compaction must be deterministic");
    }

    #[test]
    fn compaction_changes_root() {
        let mut t = TwigTree::new();
        for i in 0..5000u64 {
            t.set(key(i), &val(i));
        }
        for i in 0..1900u64 {
            t.delete(&key(i));
        }
        let before = t.root();
        t.compact(0.5);
        let after = t.root();
        assert_ne!(before, after, "re-slotting entries must change the root");
    }

    #[test]
    fn append_order_determinism() {
        // Same sequence of ops -> same root (deterministic). (Different *order*
        // would differ — that's the append model; the sharded layer enforces a
        // canonical order, see devlog-64.)
        let build = || {
            let mut t = TwigTree::new();
            for i in 0..1000 {
                t.set(key(i), &val(i));
            }
            t.root()
        };
        assert_eq!(build(), build());
    }
}
