use anyhow::Result;
use jmt::storage::{LeafNode, Node, NodeBatch, NodeKey, TreeReader, TreeWriter};
use jmt::{KeyHash, OwnedValue, Version};
use parking_lot::RwLock;
use std::collections::{BTreeMap, HashMap};

/// Tombstone-aware value entry: tracks both writes and deletions.
#[derive(Debug, Clone)]
enum ValueEntry {
    /// Key has a value at this version.
    Live(OwnedValue),
    /// Key was deleted at this version.
    Deleted,
}

/// In-memory JMT node store.
///
/// Stores all tree nodes (internal + leaf) in a `HashMap`, and leaf values
/// indexed by `(KeyHash, Version)` for range lookups. Deletions are recorded
/// as tombstones so that `get_value_option` correctly returns `None` for
/// deleted keys rather than falling through to an older live version.
///
/// Thread-safe via `parking_lot::RwLock` — readers never block each other,
/// writers only block briefly on HashMap insert.
///
/// Future: swap inner maps for reth DB tables via `TreeReader`/`TreeWriter` traits.
#[derive(Debug, Default)]
pub struct MemTreeStore {
    nodes: RwLock<HashMap<NodeKey, Node>>,
    /// Values indexed by key hash, sorted by version. Tombstones tracked.
    values: RwLock<HashMap<KeyHash, BTreeMap<Version, ValueEntry>>>,
    /// Versions that have been pruned (stale nodes removed).
    /// Tracks the minimum retained version for future GC.
    min_retained_version: RwLock<Version>,
    /// The latest version written to this store.
    latest_version: RwLock<Version>,
}

impl MemTreeStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of nodes currently stored.
    pub fn node_count(&self) -> usize {
        self.nodes.read().len()
    }

    /// Number of distinct key hashes with at least one value version.
    pub fn key_count(&self) -> usize {
        self.values.read().len()
    }

    /// Prune stale nodes and old value versions older than `keep_versions`.
    ///
    /// Retains only versions >= (latest_version - keep_versions).
    /// Call periodically (e.g., every 100 blocks) to bound memory growth.
    pub fn prune(&self, latest_version: Version, keep_versions: u64) {
        let cutoff = latest_version.saturating_sub(keep_versions);

        // Prune old value versions.
        let mut values = self.values.write();
        values.retain(|_, versions| {
            // Remove all entries older than cutoff, but keep at least the
            // latest entry at or before cutoff (the "anchor" value).
            let anchor_key = versions.range(..=cutoff).next_back().map(|(v, _)| *v);
            versions.retain(|v, _| *v > cutoff || Some(*v) == anchor_key);
            !versions.is_empty()
        });

        *self.min_retained_version.write() = cutoff;
    }

    /// Dump all live key-value pairs at the latest version.
    ///
    /// Returns `(KeyHash bytes, value bytes)` for each key that has a live value.
    /// Used for snapshot persistence.
    pub fn dump_latest(&self) -> Vec<([u8; 32], Vec<u8>)> {
        let values = self.values.read();
        let latest = *self.latest_version.read();
        let mut result = Vec::new();
        for (key_hash, versions) in values.iter() {
            if let Some((_, ValueEntry::Live(val))) = versions.range(..=latest).next_back() {
                result.push((key_hash.0, val.clone()));
            }
        }
        result
    }
}

impl TreeReader for MemTreeStore {
    fn get_node_option(&self, node_key: &NodeKey) -> Result<Option<Node>> {
        Ok(self.nodes.read().get(node_key).cloned())
    }

    fn get_rightmost_leaf(&self) -> Result<Option<(NodeKey, LeafNode)>> {
        let nodes = self.nodes.read();
        let mut best: Option<(NodeKey, LeafNode)> = None;
        for (key, node) in nodes.iter() {
            if let Node::Leaf(leaf) = node {
                match &best {
                    None => best = Some((key.clone(), leaf.clone())),
                    Some((best_key, _)) if key > best_key => {
                        best = Some((key.clone(), leaf.clone()));
                    }
                    _ => {}
                }
            }
        }
        Ok(best)
    }

    fn get_value_option(
        &self,
        max_version: Version,
        key_hash: KeyHash,
    ) -> Result<Option<OwnedValue>> {
        let values = self.values.read();
        let Some(versions) = values.get(&key_hash) else {
            return Ok(None);
        };
        // Find the latest entry <= max_version.
        match versions.range(..=max_version).next_back() {
            Some((_, ValueEntry::Live(v))) => Ok(Some(v.clone())),
            Some((_, ValueEntry::Deleted)) => Ok(None), // tombstone → key deleted
            None => Ok(None),
        }
    }
}

impl TreeWriter for MemTreeStore {
    fn write_node_batch(&self, node_batch: &NodeBatch) -> Result<()> {
        let mut nodes = self.nodes.write();
        for (key, node) in node_batch.nodes() {
            nodes.insert(key.clone(), node.clone());
        }

        let mut values = self.values.write();
        let mut max_version = 0u64;
        for ((version, key_hash), maybe_value) in node_batch.values() {
            let entry = match maybe_value {
                Some(value) => ValueEntry::Live(value.clone()),
                None => ValueEntry::Deleted, // record tombstone for deletions
            };
            values.entry(*key_hash).or_default().insert(*version, entry);
            max_version = max_version.max(*version);
        }

        // Track the latest version written.
        let mut latest = self.latest_version.write();
        if max_version > *latest {
            *latest = max_version;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Blake3Hasher;
    use jmt::JellyfishMerkleTree;

    #[test]
    fn empty_store() {
        let store = MemTreeStore::new();
        assert_eq!(store.node_count(), 0);
        assert_eq!(store.key_count(), 0);
    }

    #[test]
    fn put_and_get_via_jmt() {
        let store = MemTreeStore::new();
        let tree = JellyfishMerkleTree::<_, Blake3Hasher>::new(&store);

        let key = KeyHash([0x42; 32]);
        let value = b"hello".to_vec();

        let (_root, batch) = tree
            .put_value_set(vec![(key, Some(value.clone()))], 0)
            .unwrap();
        store.write_node_batch(&batch.node_batch).unwrap();

        assert!(store.node_count() > 0);
        assert_eq!(store.key_count(), 1);

        let got = store.get_value_option(0, key).unwrap();
        assert_eq!(got, Some(value));
    }

    #[test]
    fn version_range_query() {
        let store = MemTreeStore::new();
        let tree = JellyfishMerkleTree::<_, Blake3Hasher>::new(&store);

        let key = KeyHash([0x01; 32]);

        let (_, batch) = tree
            .put_value_set(vec![(key, Some(b"v0".to_vec()))], 0)
            .unwrap();
        store.write_node_batch(&batch.node_batch).unwrap();

        let (_, batch) = tree
            .put_value_set(vec![(key, Some(b"v1".to_vec()))], 1)
            .unwrap();
        store.write_node_batch(&batch.node_batch).unwrap();

        assert_eq!(
            store.get_value_option(0, key).unwrap(),
            Some(b"v0".to_vec())
        );
        assert_eq!(
            store.get_value_option(1, key).unwrap(),
            Some(b"v1".to_vec())
        );
    }

    #[test]
    fn tombstone_hides_deleted_value() {
        let store = MemTreeStore::new();
        let tree = JellyfishMerkleTree::<_, Blake3Hasher>::new(&store);

        let key = KeyHash([0x42; 32]);

        // Version 0: write value.
        let (_, batch) = tree
            .put_value_set(vec![(key, Some(b"alive".to_vec()))], 0)
            .unwrap();
        store.write_node_batch(&batch.node_batch).unwrap();
        assert_eq!(
            store.get_value_option(0, key).unwrap(),
            Some(b"alive".to_vec())
        );

        // Version 1: delete key.
        let (_, batch) = tree.put_value_set(vec![(key, None)], 1).unwrap();
        store.write_node_batch(&batch.node_batch).unwrap();

        // At version 1, key should be gone.
        assert_eq!(store.get_value_option(1, key).unwrap(), None);
        // At version 0, key should still be visible.
        assert_eq!(
            store.get_value_option(0, key).unwrap(),
            Some(b"alive".to_vec())
        );
    }

    #[test]
    fn delete_then_recreate() {
        let store = MemTreeStore::new();
        let tree = JellyfishMerkleTree::<_, Blake3Hasher>::new(&store);

        let key = KeyHash([0x42; 32]);

        // v0: create
        let (_, batch) = tree
            .put_value_set(vec![(key, Some(b"v0".to_vec()))], 0)
            .unwrap();
        store.write_node_batch(&batch.node_batch).unwrap();

        // v1: delete
        let (_, batch) = tree.put_value_set(vec![(key, None)], 1).unwrap();
        store.write_node_batch(&batch.node_batch).unwrap();

        // v2: recreate with different value
        let (_, batch) = tree
            .put_value_set(vec![(key, Some(b"v2".to_vec()))], 2)
            .unwrap();
        store.write_node_batch(&batch.node_batch).unwrap();

        assert_eq!(
            store.get_value_option(0, key).unwrap(),
            Some(b"v0".to_vec())
        );
        assert_eq!(store.get_value_option(1, key).unwrap(), None);
        assert_eq!(
            store.get_value_option(2, key).unwrap(),
            Some(b"v2".to_vec())
        );
    }

    #[test]
    fn prune_removes_old_versions() {
        let store = MemTreeStore::new();
        let tree = JellyfishMerkleTree::<_, Blake3Hasher>::new(&store);

        let key = KeyHash([0x42; 32]);

        for v in 0..10u64 {
            let val = format!("v{v}").into_bytes();
            let (_, batch) = tree.put_value_set(vec![(key, Some(val))], v).unwrap();
            store.write_node_batch(&batch.node_batch).unwrap();
        }

        // Keep last 3 versions (7, 8, 9) + anchor at cutoff.
        store.prune(9, 3);

        // Version 9 should still work.
        assert_eq!(
            store.get_value_option(9, key).unwrap(),
            Some(b"v9".to_vec())
        );
    }
}
