//! JMT snapshot persistence.
//!
//! Provides file-based snapshots for the ShardedJmt. Since the jmt crate's
//! internal Node/NodeKey types don't support serde, we take a value-level
//! approach: dump all live key-value pairs and version, then reconstruct
//! the tree from scratch on restore.
//!
//! Snapshots are written atomically (write to temp file + rename) and
//! compressed with zstd for compact storage.

use crate::sharded::{ShardedJmt, SHARD_COUNT};
use crate::store::MemTreeStore;
use crate::tree::N42JmtTree;
use jmt::KeyHash;
use parking_lot::Mutex;
use std::path::Path;
use std::sync::Arc;
use tracing::info;

/// Serializable snapshot of a ShardedJmt's state.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct JmtSnapshot {
    /// JMT version at the time of snapshot.
    pub version: u64,
    /// Combined root hash bytes.
    pub root: [u8; 32],
    /// All live key-value pairs: `(KeyHash bytes, value bytes)`.
    pub entries: Vec<([u8; 32], Vec<u8>)>,
}

impl JmtSnapshot {
    /// Number of entries in the snapshot.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the snapshot is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl ShardedJmt {
    /// Create a snapshot of the current JMT state.
    ///
    /// Collects all live key-value pairs from all 16 shards.
    pub fn snapshot(&self) -> eyre::Result<JmtSnapshot> {
        let root = self.root_hash()?;
        let mut entries = Vec::new();
        for shard in self.shards() {
            let tree = shard.lock();
            let store = tree.store();
            entries.extend(store.dump_latest());
        }
        Ok(JmtSnapshot {
            version: self.version(),
            root: root.0,
            entries,
        })
    }

    /// Reconstruct a ShardedJmt from a snapshot.
    ///
    /// Creates a new tree with 16 shards, distributes all entries by shard index,
    /// and applies them as a single batch per shard. The restored tree will have
    /// the same combined root hash as the original.
    pub fn from_snapshot(snapshot: JmtSnapshot) -> eyre::Result<Self> {
        let start = std::time::Instant::now();

        // Partition entries by shard.
        let mut shard_entries: Vec<Vec<(KeyHash, Option<Vec<u8>>)>> =
            (0..SHARD_COUNT).map(|_| Vec::new()).collect();

        for (key_bytes, value) in &snapshot.entries {
            let key = KeyHash(*key_bytes);
            let idx = (key.0[0] >> 4) as usize;
            shard_entries[idx].push((key, Some(value.clone())));
        }

        // Build each shard.
        let shards: Vec<Mutex<N42JmtTree>> = shard_entries
            .into_iter()
            .map(|entries| {
                let mut tree = N42JmtTree::with_store(Arc::new(MemTreeStore::new()));
                if !entries.is_empty() {
                    tree.apply_batch(entries)
                        .expect("failed to apply snapshot entries to shard");
                }
                Mutex::new(tree)
            })
            .collect();

        let jmt = ShardedJmt::from_parts(shards, snapshot.version);
        let elapsed_ms = start.elapsed().as_millis();
        info!(
            target: "n42::jmt",
            version = snapshot.version,
            entries = snapshot.entries.len(),
            elapsed_ms,
            "JMT restored from snapshot"
        );
        Ok(jmt)
    }
}

/// Save a JMT snapshot to a file (bincode + zstd compressed).
///
/// Writes atomically: temp file → rename.
pub fn save_snapshot(path: &Path, snapshot: &JmtSnapshot) -> eyre::Result<()> {
    let start = std::time::Instant::now();

    // Ensure parent directory exists.
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let raw = bincode::serialize(snapshot)?;
    let compressed = zstd::bulk::compress(&raw, 3)?;

    // Atomic write: write to temp file then rename.
    let tmp_path = path.with_extension("tmp");
    std::fs::write(&tmp_path, &compressed)?;
    std::fs::rename(&tmp_path, path)?;

    let elapsed_ms = start.elapsed().as_millis();
    info!(
        target: "n42::jmt",
        version = snapshot.version,
        entries = snapshot.entries.len(),
        raw_kb = raw.len() / 1024,
        compressed_kb = compressed.len() / 1024,
        elapsed_ms,
        path = %path.display(),
        "JMT snapshot saved"
    );
    Ok(())
}

/// Load a JMT snapshot from a file.
///
/// Returns `None` if the file doesn't exist.
pub fn load_snapshot(path: &Path) -> eyre::Result<Option<JmtSnapshot>> {
    if !path.exists() {
        return Ok(None);
    }

    let start = std::time::Instant::now();
    let compressed = std::fs::read(path)?;
    let raw = zstd::bulk::decompress(&compressed, 256 * 1024 * 1024)?; // 256 MB max
    let snapshot: JmtSnapshot = bincode::deserialize(&raw)?;
    let elapsed_ms = start.elapsed().as_millis();

    info!(
        target: "n42::jmt",
        version = snapshot.version,
        entries = snapshot.entries.len(),
        elapsed_ms,
        path = %path.display(),
        "JMT snapshot loaded"
    );
    Ok(Some(snapshot))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, U256};
    use n42_execution::state_diff::{AccountChangeType, AccountDiff, StateDiff, ValueChange};
    use std::collections::BTreeMap;

    fn make_diff(n: usize) -> StateDiff {
        let mut accounts = BTreeMap::new();
        for i in 0..n {
            accounts.insert(
                Address::with_last_byte(i as u8),
                AccountDiff {
                    change_type: AccountChangeType::Created,
                    balance: Some(ValueChange::new(U256::ZERO, U256::from(1000 + i as u64))),
                    nonce: Some(ValueChange::new(0, 1)),
                    code_change: None,
                    storage: BTreeMap::new(),
                },
            );
        }
        StateDiff { accounts }
    }

    #[test]
    fn snapshot_roundtrip() {
        let mut jmt = ShardedJmt::new();
        let diff = make_diff(50);
        jmt.apply_diff(&diff).unwrap();
        let original_root = jmt.root_hash().unwrap();
        let original_version = jmt.version();

        let snapshot = jmt.snapshot().unwrap();
        assert_eq!(snapshot.version, original_version);
        assert_eq!(snapshot.entries.len(), 50);

        let restored = ShardedJmt::from_snapshot(snapshot).unwrap();
        let restored_root = restored.root_hash().unwrap();
        assert_eq!(restored_root, original_root, "restored root must match original");
        assert_eq!(restored.version(), original_version);
    }

    #[test]
    fn snapshot_file_roundtrip() {
        let mut jmt = ShardedJmt::new();
        let diff = make_diff(100);
        jmt.apply_diff(&diff).unwrap();
        let original_root = jmt.root_hash().unwrap();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jmt.snapshot");

        let snapshot = jmt.snapshot().unwrap();
        save_snapshot(&path, &snapshot).unwrap();

        let loaded = load_snapshot(&path).unwrap().expect("snapshot should exist");
        let restored = ShardedJmt::from_snapshot(loaded).unwrap();
        assert_eq!(restored.root_hash().unwrap(), original_root);
    }

    #[test]
    fn load_nonexistent_file() {
        let result = load_snapshot(Path::new("/tmp/nonexistent_jmt_snapshot_12345"));
        assert!(result.unwrap().is_none());
    }

    #[test]
    fn snapshot_empty_tree() {
        let jmt = ShardedJmt::new();
        let snapshot = jmt.snapshot().unwrap();
        assert!(snapshot.is_empty());
        assert_eq!(snapshot.version, 0);

        let restored = ShardedJmt::from_snapshot(snapshot).unwrap();
        assert_eq!(restored.version(), 0);
    }
}
