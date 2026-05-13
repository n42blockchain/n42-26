//! MDBX-backed persistent JMT store.
//!
//! Replaces `MemTreeStore` for production use. Each `DiskTreeStore` instance
//! manages a single shard's nodes, values, and metadata within a shared MDBX
//! environment. Tree nodes are cached in an LRU for hot-path reads.

use crate::store::TreeStore;

use anyhow::Result;
use borsh::BorshDeserialize;
use jmt::storage::{LeafNode, Node, NodeBatch, NodeKey, TreeReader, TreeWriter};
use jmt::{KeyHash, OwnedValue, Version};
use lru::LruCache;
use parking_lot::Mutex;
use reth_libmdbx::{
    Database, DatabaseFlags, Environment, Geometry, RO, RW, Transaction, WriteFlags,
};
use std::num::NonZeroUsize;
use std::path::Path;
use tracing::debug;

/// Default LRU cache capacity per shard (node entries).
pub const DEFAULT_CACHE_SIZE: usize = 10_000;

/// Meta key for latest version stored in the metadata database.
const META_LATEST_VERSION: &[u8] = b"latest_version";

// ---------------------------------------------------------------------------
// Value encoding: 1-byte tag + payload
// ---------------------------------------------------------------------------

const TAG_LIVE: u8 = 0x01;
const TAG_TOMBSTONE: u8 = 0x00;

fn encode_value_entry(value: &Option<Vec<u8>>) -> Vec<u8> {
    match value {
        Some(v) => {
            let mut buf = Vec::with_capacity(1 + v.len());
            buf.push(TAG_LIVE);
            buf.extend_from_slice(v);
            buf
        }
        None => vec![TAG_TOMBSTONE],
    }
}

fn decode_value_entry(bytes: &[u8]) -> Option<Vec<u8>> {
    match bytes.first() {
        Some(&TAG_LIVE) => Some(bytes[1..].to_vec()),
        _ => None, // tombstone or empty
    }
}

// ---------------------------------------------------------------------------
// Values-table key: [key_hash: 32][version: 8 BE]
// ---------------------------------------------------------------------------

fn encode_value_key(key_hash: &KeyHash, version: Version) -> [u8; 40] {
    let mut buf = [0u8; 40];
    buf[..32].copy_from_slice(&key_hash.0);
    buf[32..].copy_from_slice(&version.to_be_bytes());
    buf
}

fn decode_value_key(bytes: &[u8]) -> Option<(KeyHash, Version)> {
    if bytes.len() < 40 {
        return None;
    }
    let mut kh = [0u8; 32];
    kh.copy_from_slice(&bytes[..32]);
    let version = u64::from_be_bytes(bytes[32..40].try_into().ok()?);
    Some((KeyHash(kh), version))
}

// ---------------------------------------------------------------------------
// Node key/value serialization via Borsh
// ---------------------------------------------------------------------------

fn encode_node_key(nk: &NodeKey) -> Vec<u8> {
    borsh::to_vec(nk).expect("NodeKey borsh serialization cannot fail")
}

fn encode_node(node: &Node) -> Vec<u8> {
    borsh::to_vec(node).expect("Node borsh serialization cannot fail")
}

fn decode_node(bytes: &[u8]) -> Result<Node> {
    Node::try_from_slice(bytes).map_err(|e| anyhow::anyhow!("Node borsh decode: {e}"))
}

fn decode_node_key(bytes: &[u8]) -> Result<NodeKey> {
    NodeKey::try_from_slice(bytes).map_err(|e| anyhow::anyhow!("NodeKey borsh decode: {e}"))
}

// ---------------------------------------------------------------------------
// DiskTreeStore
// ---------------------------------------------------------------------------

/// MDBX-backed JMT store for a single shard.
///
/// Uses three named databases within a shared MDBX environment:
/// - `jmt_nodes_XX`: tree structure nodes (NodeKey → Node, Borsh-encoded)
/// - `jmt_values_XX`: versioned leaf values ([KeyHash‖Version] → tagged value)
/// - `jmt_meta_XX`: metadata (e.g. latest_version)
///
/// An LRU cache sits in front of the nodes database for hot reads.
pub struct DiskTreeStore {
    env: Environment,
    nodes_db: Database,
    values_db: Database,
    meta_db: Database,
    node_cache: Mutex<LruCache<NodeKey, Node>>,
    shard_id: u8,
}

impl std::fmt::Debug for DiskTreeStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiskTreeStore")
            .field("shard_id", &self.shard_id)
            .finish_non_exhaustive()
    }
}

impl DiskTreeStore {
    /// Open (or create) a shard's databases within the given MDBX environment.
    pub fn open(env: Environment, shard_id: u8, cache_size: usize) -> eyre::Result<Self> {
        let nodes_name = format!("jmt_nodes_{shard_id:02x}");
        let values_name = format!("jmt_values_{shard_id:02x}");
        let meta_name = format!("jmt_meta_{shard_id:02x}");

        let tx = env.begin_rw_txn()?;
        let nodes_db = tx
            .create_db(Some(&nodes_name), DatabaseFlags::default())
            .map_err(|e| eyre::eyre!("create {nodes_name}: {e}"))?;
        let values_db = tx
            .create_db(Some(&values_name), DatabaseFlags::default())
            .map_err(|e| eyre::eyre!("create {values_name}: {e}"))?;
        let meta_db = tx
            .create_db(Some(&meta_name), DatabaseFlags::default())
            .map_err(|e| eyre::eyre!("create {meta_name}: {e}"))?;
        tx.commit()?;

        let cap = NonZeroUsize::new(cache_size.max(1)).unwrap();
        Ok(Self {
            env,
            nodes_db,
            values_db,
            meta_db,
            node_cache: Mutex::new(LruCache::new(cap)),
            shard_id,
        })
    }

    /// Read the latest version stored in metadata, or 0 if not set.
    pub fn read_latest_version(&self) -> Version {
        let Ok(tx) = self.env.begin_ro_txn() else {
            return 0;
        };
        match tx.get::<Vec<u8>>(self.meta_db.dbi(), META_LATEST_VERSION) {
            Ok(Some(bytes)) if bytes.len() == 8 => {
                u64::from_be_bytes(bytes[..8].try_into().unwrap())
            }
            _ => 0,
        }
    }

    /// Write latest_version to metadata within an existing transaction.
    fn write_latest_version_in_txn(&self, tx: &Transaction<RW>, version: Version) -> Result<()> {
        tx.put(
            self.meta_db.dbi(),
            META_LATEST_VERSION,
            version.to_be_bytes(),
            WriteFlags::default(),
        )
        .map_err(|e| anyhow::anyhow!("meta write: {e}"))
    }

    /// Write a node batch within an externally-managed RW transaction.
    ///
    /// Used by `ShardedJmt` for single-transaction atomic writes across shards.
    pub fn write_batch_in_txn(&self, tx: &Transaction<RW>, node_batch: &NodeBatch) -> Result<()> {
        let mut max_version = 0u64;

        // Write nodes + update cache in a single lock scope
        {
            let mut cache = self.node_cache.lock();
            for (node_key, node) in node_batch.nodes() {
                let encoded = encode_node_key(node_key);
                let val = encode_node(node);
                tx.put(self.nodes_db.dbi(), &encoded, &val, WriteFlags::default())
                    .map_err(|e| anyhow::anyhow!("nodes put: {e}"))?;
                cache.put(node_key.clone(), node.clone());
            }
        }

        // Write values
        for ((version, key_hash), maybe_value) in node_batch.values() {
            let key = encode_value_key(key_hash, *version);
            let val = encode_value_entry(maybe_value);
            tx.put(self.values_db.dbi(), key, val, WriteFlags::default())
                .map_err(|e| anyhow::anyhow!("values put: {e}"))?;
            max_version = max_version.max(*version);
        }

        // Update metadata
        if max_version > 0 {
            self.write_latest_version_in_txn(tx, max_version)?;
        }

        Ok(())
    }

    /// Clone the MDBX environment handle (for shared transaction creation in ShardedJmt).
    ///
    /// `Environment` is internally `Arc`-wrapped, so clone is cheap.
    pub fn env(&self) -> Environment {
        self.env.clone()
    }

    // -----------------------------------------------------------------------
    // Internal helpers for value range queries
    // -----------------------------------------------------------------------

    /// Find the latest value entry for `key_hash` with version <= `max_version`.
    fn find_value_le(
        &self,
        tx: &Transaction<RO>,
        max_version: Version,
        key_hash: KeyHash,
    ) -> Result<Option<OwnedValue>> {
        let mut cursor = tx
            .cursor(self.values_db.dbi())
            .map_err(|e| anyhow::anyhow!("cursor open: {e}"))?;

        let search_key = encode_value_key(&key_hash, max_version);

        // set_range positions at first key >= search_key.
        match cursor.set_range::<Vec<u8>, Vec<u8>>(&search_key) {
            Ok(Some((found_key, found_val))) => {
                // Exact match: same key_hash and exact version
                if let Some((fk, fv)) = decode_value_key(&found_key)
                    && fk.0 == key_hash.0
                    && fv == max_version
                {
                    return Ok(decode_value_entry(&found_val));
                }
                // Overshot (higher version or different key_hash) — go back one
                self.try_prev_for_key(&mut cursor, &key_hash, max_version)
            }
            Ok(None) => {
                // All DB keys < search_key: check the very last entry
                self.cursor_last_for_key(&mut cursor, &key_hash, max_version)
            }
            Err(e) => Err(anyhow::anyhow!("set_range: {e}")),
        }
    }

    fn try_prev_for_key(
        &self,
        cursor: &mut reth_libmdbx::Cursor<RO>,
        key_hash: &KeyHash,
        max_version: Version,
    ) -> Result<Option<OwnedValue>> {
        match cursor.prev::<Vec<u8>, Vec<u8>>() {
            Ok(Some((prev_key, prev_val))) => {
                if let Some((pk, pv)) = decode_value_key(&prev_key)
                    && pk.0 == key_hash.0
                    && pv <= max_version
                {
                    return Ok(decode_value_entry(&prev_val));
                }
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    fn cursor_last_for_key(
        &self,
        cursor: &mut reth_libmdbx::Cursor<RO>,
        key_hash: &KeyHash,
        max_version: Version,
    ) -> Result<Option<OwnedValue>> {
        match cursor.last::<Vec<u8>, Vec<u8>>() {
            Ok(Some((last_key, last_val))) => {
                if let Some((lk, lv)) = decode_value_key(&last_key)
                    && lk.0 == key_hash.0
                    && lv <= max_version
                {
                    return Ok(decode_value_entry(&last_val));
                }
                Ok(None)
            }
            _ => Ok(None),
        }
    }
}

// ---------------------------------------------------------------------------
// TreeReader implementation
// ---------------------------------------------------------------------------

impl TreeReader for DiskTreeStore {
    fn get_node_option(&self, node_key: &NodeKey) -> Result<Option<Node>> {
        // Check cache first (keyed by NodeKey, no serialization needed)
        {
            let mut cache = self.node_cache.lock();
            if let Some(node) = cache.get(node_key) {
                return Ok(Some(node.clone()));
            }
        }

        // Fall through to MDBX
        let encoded_key = encode_node_key(node_key);
        let tx = self
            .env
            .begin_ro_txn()
            .map_err(|e| anyhow::anyhow!("ro txn: {e}"))?;
        match tx.get::<Vec<u8>>(self.nodes_db.dbi(), &encoded_key) {
            Ok(Some(bytes)) => {
                let node = decode_node(&bytes)?;
                self.node_cache.lock().put(node_key.clone(), node.clone());
                Ok(Some(node))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(anyhow::anyhow!("nodes get: {e}")),
        }
    }

    fn get_rightmost_leaf(&self) -> Result<Option<(NodeKey, LeafNode)>> {
        let tx = self
            .env
            .begin_ro_txn()
            .map_err(|e| anyhow::anyhow!("ro txn: {e}"))?;
        let mut cursor = tx
            .cursor(self.nodes_db.dbi())
            .map_err(|e| anyhow::anyhow!("cursor: {e}"))?;

        let mut best: Option<(NodeKey, LeafNode)> = None;
        let mut entry = cursor
            .first::<Vec<u8>, Vec<u8>>()
            .map_err(|e| anyhow::anyhow!("cursor first: {e}"))?;

        while let Some((key_bytes, val_bytes)) = entry {
            let nk = decode_node_key(&key_bytes)?;
            let node = decode_node(&val_bytes)?;
            if let Node::Leaf(leaf) = node {
                match &best {
                    None => best = Some((nk, leaf)),
                    Some((best_key, _)) if &nk > best_key => best = Some((nk, leaf)),
                    _ => {}
                }
            }
            entry = cursor
                .next::<Vec<u8>, Vec<u8>>()
                .map_err(|e| anyhow::anyhow!("cursor next: {e}"))?;
        }
        Ok(best)
    }

    fn get_value_option(
        &self,
        max_version: Version,
        key_hash: KeyHash,
    ) -> Result<Option<OwnedValue>> {
        let tx = self
            .env
            .begin_ro_txn()
            .map_err(|e| anyhow::anyhow!("ro txn: {e}"))?;
        self.find_value_le(&tx, max_version, key_hash)
    }
}

// ---------------------------------------------------------------------------
// TreeWriter implementation
// ---------------------------------------------------------------------------

impl TreeWriter for DiskTreeStore {
    fn write_node_batch(&self, node_batch: &NodeBatch) -> Result<()> {
        let tx = self
            .env
            .begin_rw_txn()
            .map_err(|e| anyhow::anyhow!("rw txn: {e}"))?;
        self.write_batch_in_txn(&tx, node_batch)?;
        tx.commit().map_err(|e| anyhow::anyhow!("commit: {e}"))?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// TreeStore implementation
// ---------------------------------------------------------------------------

impl TreeStore for DiskTreeStore {
    fn node_count(&self) -> usize {
        let Ok(tx) = self.env.begin_ro_txn() else {
            return 0;
        };
        match tx.db_stat(self.nodes_db.dbi()) {
            Ok(stat) => stat.entries(),
            Err(_) => 0,
        }
    }

    fn key_count(&self) -> usize {
        // Count distinct key hashes in values_db.
        // This requires a full cursor scan since MDBX stat only gives total entries.
        let Ok(tx) = self.env.begin_ro_txn() else {
            return 0;
        };
        let Ok(mut cursor) = tx.cursor(self.values_db.dbi()) else {
            return 0;
        };

        let mut count = 0usize;
        let mut last_kh: Option<[u8; 32]> = None;
        let mut entry = cursor.first::<Vec<u8>, Vec<u8>>().ok().flatten();
        while let Some((key, _)) = entry {
            if key.len() >= 32 {
                let kh: [u8; 32] = key[..32].try_into().unwrap();
                if last_kh.as_ref() != Some(&kh) {
                    count += 1;
                    last_kh = Some(kh);
                }
            }
            entry = cursor.next::<Vec<u8>, Vec<u8>>().ok().flatten();
        }
        count
    }

    fn prune(&self, latest_version: Version, keep_versions: u64) {
        let cutoff = latest_version.saturating_sub(keep_versions);
        let Ok(tx) = self.env.begin_rw_txn() else {
            return;
        };
        let Ok(mut cursor) = tx.cursor(self.values_db.dbi()) else {
            return;
        };

        // Single pass: MDBX keys are sorted by [key_hash || version_be], so all
        // entries for the same key_hash are contiguous and version-ordered.
        // Track per-key_hash candidates <= cutoff; the last one is the anchor.
        let mut to_delete: Vec<Vec<u8>> = Vec::new();
        let mut current_kh: Option<[u8; 32]> = None;
        let mut candidates: Vec<Vec<u8>> = Vec::new(); // keys <= cutoff for current key_hash

        let mut entry = cursor.first::<Vec<u8>, Vec<u8>>().ok().flatten();
        while let Some((key, _)) = entry {
            if let Some((kh, version)) = decode_value_key(&key) {
                if current_kh.as_ref() != Some(&kh.0) {
                    // New key_hash: flush previous candidates (all but last = anchor)
                    if candidates.len() > 1 {
                        to_delete.extend(candidates.drain(..candidates.len() - 1));
                    }
                    candidates.clear();
                    current_kh = Some(kh.0);
                }
                if version <= cutoff {
                    candidates.push(key);
                }
            }
            entry = cursor.next::<Vec<u8>, Vec<u8>>().ok().flatten();
        }
        // Flush last key_hash's candidates
        if candidates.len() > 1 {
            to_delete.extend(candidates.drain(..candidates.len() - 1));
        }
        drop(cursor);

        for key in &to_delete {
            let _ = tx.del(self.values_db.dbi(), key, None);
        }

        if let Err(e) = tx.commit() {
            tracing::warn!(target: "n42::jmt", shard = self.shard_id, error = %e, "prune commit failed");
        } else {
            debug!(
                target: "n42::jmt",
                shard = self.shard_id,
                cutoff,
                deleted = to_delete.len(),
                "pruned old value versions"
            );
        }
    }

    fn dump_latest(&self) -> Vec<([u8; 32], Vec<u8>)> {
        let Ok(tx) = self.env.begin_ro_txn() else {
            return Vec::new();
        };
        // Read latest_version from the same txn to avoid snapshot skew.
        let latest = match tx.get::<Vec<u8>>(self.meta_db.dbi(), META_LATEST_VERSION) {
            Ok(Some(bytes)) if bytes.len() == 8 => {
                u64::from_be_bytes(bytes[..8].try_into().unwrap())
            }
            _ => 0,
        };
        let Ok(mut cursor) = tx.cursor(self.values_db.dbi()) else {
            return Vec::new();
        };

        let mut result = Vec::new();
        let mut last_kh: Option<[u8; 32]> = None;
        let mut last_live: Option<([u8; 32], Vec<u8>)> = None;

        let mut entry = cursor.first::<Vec<u8>, Vec<u8>>().ok().flatten();
        while let Some((key, val)) = entry {
            if let Some((kh, version)) = decode_value_key(&key) {
                if last_kh.as_ref() != Some(&kh.0) {
                    // New key_hash: flush previous
                    if let Some(prev) = last_live.take() {
                        result.push(prev);
                    }
                    last_kh = Some(kh.0);
                }
                if version <= latest {
                    match decode_value_entry(&val) {
                        Some(v) => last_live = Some((kh.0, v)),
                        None => last_live = None, // tombstone clears it
                    }
                }
            }
            entry = cursor.next::<Vec<u8>, Vec<u8>>().ok().flatten();
        }
        // Flush last key
        if let Some(prev) = last_live.take() {
            result.push(prev);
        }

        result
    }
}

// ---------------------------------------------------------------------------
// Environment builder helper
// ---------------------------------------------------------------------------

/// Create a new MDBX environment suitable for JMT storage.
///
/// Returns an `Environment` that should be shared across all 16 shard `DiskTreeStore`s.
pub fn open_jmt_env(path: impl AsRef<Path>) -> eyre::Result<Environment> {
    let path = path.as_ref();
    std::fs::create_dir_all(path)?;

    let mut builder = Environment::builder();
    builder.set_max_dbs(55); // 16 shards * 3 dbs + evidence table + headroom
    builder.set_geometry(Geometry {
        size: Some(64 * 1024 * 1024..1024 * 1024 * 1024 * 1024), // 64 MB .. 1 TB
        // Use OS default page size (4KB). Matches SSD internal page, filesystem
        // block size, and Linux page cache granularity. Minimizes write amplification
        // for small JMT node updates (~200-500 bytes per node).
        ..Default::default()
    });
    let env = builder
        .open(path)
        .map_err(|e| eyre::eyre!("open MDBX env at {}: {e}", path.display()))?;

    Ok(env)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Blake3Hasher;
    use jmt::JellyfishMerkleTree;

    fn test_env() -> (tempfile::TempDir, Environment) {
        let dir = tempfile::tempdir().unwrap();
        let env = open_jmt_env(dir.path()).unwrap();
        (dir, env)
    }

    fn test_store() -> (tempfile::TempDir, DiskTreeStore) {
        let (dir, env) = test_env();
        let store = DiskTreeStore::open(env, 0, DEFAULT_CACHE_SIZE).unwrap();
        (dir, store)
    }

    #[test]
    fn empty_store() {
        let (_dir, store) = test_store();
        assert_eq!(store.node_count(), 0);
        assert_eq!(store.key_count(), 0);
        assert_eq!(store.read_latest_version(), 0);
    }

    #[test]
    fn put_and_get_via_jmt() {
        let (_dir, store) = test_store();
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
        let (_dir, store) = test_store();
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
        let (_dir, store) = test_store();
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

        assert_eq!(store.get_value_option(1, key).unwrap(), None);
        assert_eq!(
            store.get_value_option(0, key).unwrap(),
            Some(b"alive".to_vec())
        );
    }

    #[test]
    fn delete_then_recreate() {
        let (_dir, store) = test_store();
        let tree = JellyfishMerkleTree::<_, Blake3Hasher>::new(&store);

        let key = KeyHash([0x42; 32]);

        let (_, batch) = tree
            .put_value_set(vec![(key, Some(b"v0".to_vec()))], 0)
            .unwrap();
        store.write_node_batch(&batch.node_batch).unwrap();

        let (_, batch) = tree.put_value_set(vec![(key, None)], 1).unwrap();
        store.write_node_batch(&batch.node_batch).unwrap();

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
    fn latest_version_persists() {
        let (_dir, store) = test_store();
        let tree = JellyfishMerkleTree::<_, Blake3Hasher>::new(&store);

        let key = KeyHash([0x42; 32]);
        for v in 0..5u64 {
            let val = format!("v{v}").into_bytes();
            let (_, batch) = tree.put_value_set(vec![(key, Some(val))], v).unwrap();
            store.write_node_batch(&batch.node_batch).unwrap();
        }

        assert_eq!(store.read_latest_version(), 4);
    }

    #[test]
    fn persistence_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let key = KeyHash([0x42; 32]);
        let value = b"persisted".to_vec();

        // Write
        {
            let env = open_jmt_env(dir.path()).unwrap();
            let store = DiskTreeStore::open(env, 0, DEFAULT_CACHE_SIZE).unwrap();
            let tree = JellyfishMerkleTree::<_, Blake3Hasher>::new(&store);
            let (_, batch) = tree
                .put_value_set(vec![(key, Some(value.clone()))], 0)
                .unwrap();
            store.write_node_batch(&batch.node_batch).unwrap();
        }

        // Reopen and verify
        {
            let env = open_jmt_env(dir.path()).unwrap();
            let store = DiskTreeStore::open(env, 0, DEFAULT_CACHE_SIZE).unwrap();
            let got = store.get_value_option(0, key).unwrap();
            assert_eq!(got, Some(value));
            assert!(store.node_count() > 0);
            assert_eq!(store.read_latest_version(), 0);
        }
    }

    #[test]
    fn dump_latest_returns_live_values() {
        let (_dir, store) = test_store();
        let tree = JellyfishMerkleTree::<_, Blake3Hasher>::new(&store);

        let k1 = KeyHash([0x01; 32]);
        let k2 = KeyHash([0x02; 32]);

        let (_, batch) = tree
            .put_value_set(
                vec![(k1, Some(b"a".to_vec())), (k2, Some(b"b".to_vec()))],
                0,
            )
            .unwrap();
        store.write_node_batch(&batch.node_batch).unwrap();

        // Delete k2 at version 1
        let (_, batch) = tree.put_value_set(vec![(k2, None)], 1).unwrap();
        store.write_node_batch(&batch.node_batch).unwrap();

        let dump = store.dump_latest();
        assert_eq!(dump.len(), 1);
        assert_eq!(dump[0].0, k1.0);
    }
}
