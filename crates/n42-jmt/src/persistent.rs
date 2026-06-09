//! Firewood-style persistence: in-memory JMT + periodic background snapshots.
//!
//! P0 prototype validating the "internal nodes fully in-memory, no per-update
//! SSD IO" direction from the QMDB/NOMT gap analysis. The tree runs entirely on
//! [`MemTreeStore`] — every internal/leaf node lives in RAM, so Merkleization
//! performs **zero** disk IO on the hot path (contrast with [`crate::DiskTreeStore`],
//! which persists every node to MDBX and pointer-chases on reads). Durability
//! comes from value-level snapshots flushed every `snapshot_interval` versions,
//! optionally on a background thread so the apply path is never blocked on IO.
//!
//! ## Limitation (P0 scope)
//!
//! Crash-recovery granularity == snapshot interval: versions applied after the
//! last successful snapshot are lost on crash. Production durability needs a
//! write-ahead log of `StateDiff`s between snapshots to close this gap (tracked
//! as a follow-up in the gap-analysis devlog). This prototype exists to measure
//! the upper-bound speedup of eliminating node SSD IO, not to provide
//! production-grade durability.

use crate::sharded::ShardedJmt;
use crate::sharded_bmt::ShardedSbmt;
use crate::snapshot::{JmtSnapshot, load_snapshot, save_snapshot};
use crate::store::MemTreeStore;

use alloy_primitives::B256;
use jmt::Version;
use n42_execution::state_diff::StateDiff;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use tracing::debug;

/// In-memory `ShardedJmt` with snapshot-based durability.
///
/// Wraps `ShardedJmt<MemTreeStore>` so that `apply_diff` runs at in-memory speed
/// (no node IO), and persists state via [`JmtSnapshot`] every `snapshot_interval`
/// versions. Snapshots can be flushed on a background thread so serialization +
/// compression + file write stay off the apply hot path.
pub struct PersistentJmt {
    inner: ShardedJmt<MemTreeStore>,
    snapshot_path: PathBuf,
    snapshot_interval: u64,
    last_snapshot_version: Version,
    pending_flush: Option<JoinHandle<eyre::Result<()>>>,
}

impl PersistentJmt {
    /// Open a persistent JMT, restoring from an existing snapshot if present.
    ///
    /// `snapshot_interval` is the number of versions between automatic snapshots
    /// (clamped to a minimum of 1). Pass `u64::MAX` to disable automatic flushing
    /// and snapshot only on explicit [`flush`](Self::flush) calls.
    pub fn open(snapshot_path: impl AsRef<Path>, snapshot_interval: u64) -> eyre::Result<Self> {
        let snapshot_path = snapshot_path.as_ref().to_path_buf();
        let (inner, last_snapshot_version) = match load_snapshot(&snapshot_path)? {
            Some(snap) => {
                let version = snap.version;
                (ShardedJmt::from_snapshot(snap)?, version)
            }
            None => (ShardedJmt::new(), 0),
        };
        Ok(Self {
            inner,
            snapshot_path,
            snapshot_interval: snapshot_interval.max(1),
            last_snapshot_version,
            pending_flush: None,
        })
    }

    /// Current global version.
    pub fn version(&self) -> Version {
        self.inner.version()
    }

    /// Combined root hash across all 16 shards.
    pub fn root_hash(&self) -> eyre::Result<B256> {
        self.inner.root_hash()
    }

    /// Borrow the underlying in-memory sharded tree (reads, proofs, diagnostics).
    pub fn inner(&self) -> &ShardedJmt<MemTreeStore> {
        &self.inner
    }

    /// The version that was most recently persisted to a snapshot.
    pub fn last_snapshot_version(&self) -> Version {
        self.last_snapshot_version
    }

    /// Apply a `StateDiff` to the in-memory tree (zero node IO).
    ///
    /// Triggers a background snapshot once `snapshot_interval` versions have
    /// elapsed since the last persisted snapshot.
    pub fn apply_diff(&mut self, diff: &StateDiff) -> eyre::Result<(Version, B256)> {
        let (version, root) = self.inner.apply_diff(diff)?;
        if version.saturating_sub(self.last_snapshot_version) >= self.snapshot_interval {
            self.flush_background()?;
        }
        Ok((version, root))
    }

    /// Synchronous snapshot flush: blocks until the snapshot is durably written.
    pub fn flush(&mut self) -> eyre::Result<()> {
        self.join_pending()?;
        let snapshot = self.inner.snapshot()?;
        self.last_snapshot_version = snapshot.version;
        save_snapshot(&self.snapshot_path, &snapshot)
    }

    /// Background snapshot flush: keep serialization + IO off the apply path.
    ///
    /// The in-memory `snapshot()` (a cheap value-level dump) runs synchronously
    /// to capture a consistent state, then compression + file write run on a
    /// spawned thread. At most one flush is in flight at a time; a new flush
    /// first joins any pending one.
    pub fn flush_background(&mut self) -> eyre::Result<()> {
        self.join_pending()?;
        let snapshot: JmtSnapshot = self.inner.snapshot()?;
        self.last_snapshot_version = snapshot.version;
        let path = self.snapshot_path.clone();
        debug!(
            target: "n42::jmt",
            version = snapshot.version,
            entries = snapshot.entries.len(),
            "spawning background JMT snapshot flush"
        );
        self.pending_flush = Some(std::thread::spawn(move || save_snapshot(&path, &snapshot)));
        Ok(())
    }

    /// Wait for any in-flight background flush to finish, propagating its result.
    pub fn join_pending(&mut self) -> eyre::Result<()> {
        if let Some(handle) = self.pending_flush.take() {
            handle
                .join()
                .map_err(|_| eyre::eyre!("snapshot flush thread panicked"))??;
        }
        Ok(())
    }
}

impl Drop for PersistentJmt {
    fn drop(&mut self) {
        // Best-effort: don't lose an in-flight snapshot on drop.
        let _ = self.join_pending();
    }
}

/// In-memory [`ShardedSbmt`] with snapshot-based durability — the SBMT analogue
/// of [`PersistentJmt`]. Same model: `apply_diff` runs at in-memory speed (no
/// node IO), snapshots flush every `snapshot_interval` versions, optionally on a
/// background thread. Same P0 limitation: crash-recovery granularity == snapshot
/// interval (production needs a StateDiff WAL between snapshots).
pub struct PersistentSbmt {
    inner: ShardedSbmt,
    snapshot_path: PathBuf,
    snapshot_interval: u64,
    last_snapshot_version: u64,
    pending_flush: Option<JoinHandle<eyre::Result<()>>>,
}

impl PersistentSbmt {
    /// Open a persistent SBMT, restoring from an existing snapshot if present.
    pub fn open(snapshot_path: impl AsRef<Path>, snapshot_interval: u64) -> eyre::Result<Self> {
        let snapshot_path = snapshot_path.as_ref().to_path_buf();
        let (inner, last_snapshot_version) = match load_snapshot(&snapshot_path)? {
            Some(snap) => {
                let version = snap.version;
                (ShardedSbmt::from_snapshot(&snap)?, version)
            }
            None => (ShardedSbmt::new(), 0),
        };
        Ok(Self {
            inner,
            snapshot_path,
            snapshot_interval: snapshot_interval.max(1),
            last_snapshot_version,
            pending_flush: None,
        })
    }

    /// Current global version.
    pub fn version(&self) -> u64 {
        self.inner.version()
    }

    /// Combined root hash across all 16 shards.
    pub fn root_hash(&self) -> B256 {
        self.inner.root_hash()
    }

    /// Borrow the underlying in-memory sharded SBMT.
    pub fn inner(&self) -> &ShardedSbmt {
        &self.inner
    }

    /// The version most recently persisted to a snapshot.
    pub fn last_snapshot_version(&self) -> u64 {
        self.last_snapshot_version
    }

    /// Apply a `StateDiff` (zero node IO), triggering a background snapshot once
    /// `snapshot_interval` versions have elapsed since the last persisted one.
    pub fn apply_diff(&mut self, diff: &StateDiff) -> eyre::Result<(u64, B256)> {
        let (version, root) = self.inner.apply_diff(diff);
        if version.saturating_sub(self.last_snapshot_version) >= self.snapshot_interval {
            self.flush_background()?;
        }
        Ok((version, root))
    }

    /// Synchronous snapshot flush: blocks until durably written.
    pub fn flush(&mut self) -> eyre::Result<()> {
        self.join_pending()?;
        let snapshot = self.inner.snapshot();
        self.last_snapshot_version = snapshot.version;
        save_snapshot(&self.snapshot_path, &snapshot)
    }

    /// Background snapshot flush: serialization + IO off the apply path.
    pub fn flush_background(&mut self) -> eyre::Result<()> {
        self.join_pending()?;
        let snapshot: JmtSnapshot = self.inner.snapshot();
        self.last_snapshot_version = snapshot.version;
        let path = self.snapshot_path.clone();
        debug!(
            target: "n42::jmt",
            version = snapshot.version,
            entries = snapshot.entries.len(),
            "spawning background SBMT snapshot flush"
        );
        self.pending_flush = Some(std::thread::spawn(move || save_snapshot(&path, &snapshot)));
        Ok(())
    }

    /// Wait for any in-flight background flush, propagating its result.
    pub fn join_pending(&mut self) -> eyre::Result<()> {
        if let Some(handle) = self.pending_flush.take() {
            handle
                .join()
                .map_err(|_| eyre::eyre!("snapshot flush thread panicked"))??;
        }
        Ok(())
    }
}

impl Drop for PersistentSbmt {
    fn drop(&mut self) {
        let _ = self.join_pending();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, U256};
    use n42_execution::state_diff::{AccountChangeType, AccountDiff, ValueChange};
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
    fn apply_runs_in_memory_and_flush_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.snapshot");

        let root;
        let version;
        {
            let mut jmt = PersistentJmt::open(&path, u64::MAX).unwrap();
            jmt.apply_diff(&make_diff(100)).unwrap();
            jmt.apply_diff(&make_diff(50)).unwrap();
            root = jmt.root_hash().unwrap();
            version = jmt.version();
            jmt.flush().unwrap();
            assert_eq!(jmt.last_snapshot_version(), version);
        }

        // Reopen from snapshot and confirm the root survives.
        let reopened = PersistentJmt::open(&path, u64::MAX).unwrap();
        assert_eq!(reopened.version(), version);
        assert_eq!(reopened.root_hash().unwrap(), root);
    }

    #[test]
    fn auto_snapshot_triggers_on_interval() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.snapshot");

        let mut jmt = PersistentJmt::open(&path, 3).unwrap();
        // Versions 1,2 — no snapshot yet.
        jmt.apply_diff(&make_diff(10)).unwrap();
        jmt.apply_diff(&make_diff(10)).unwrap();
        assert_eq!(jmt.last_snapshot_version(), 0);
        // Version 3 — interval reached, snapshot fires.
        jmt.apply_diff(&make_diff(10)).unwrap();
        assert_eq!(jmt.last_snapshot_version(), 3);
        jmt.join_pending().unwrap();

        let root = jmt.root_hash().unwrap();
        drop(jmt);

        let reopened = PersistentJmt::open(&path, 3).unwrap();
        assert_eq!(reopened.version(), 3);
        assert_eq!(reopened.root_hash().unwrap(), root);
    }

    #[test]
    fn background_flush_matches_sync_root() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.snapshot");

        let mut jmt = PersistentJmt::open(&path, u64::MAX).unwrap();
        jmt.apply_diff(&make_diff(200)).unwrap();
        jmt.flush_background().unwrap();
        jmt.join_pending().unwrap();
        let root = jmt.root_hash().unwrap();
        drop(jmt);

        let reopened = PersistentJmt::open(&path, u64::MAX).unwrap();
        assert_eq!(reopened.root_hash().unwrap(), root);
    }

    #[test]
    fn sbmt_apply_flush_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sbmt.snapshot");

        let (root, version) = {
            let mut jmt = PersistentSbmt::open(&path, u64::MAX).unwrap();
            jmt.apply_diff(&make_diff(100)).unwrap();
            jmt.apply_diff(&make_diff(40)).unwrap();
            let r = jmt.root_hash();
            let v = jmt.version();
            jmt.flush().unwrap();
            assert_eq!(jmt.last_snapshot_version(), v);
            (r, v)
        };

        let reopened = PersistentSbmt::open(&path, u64::MAX).unwrap();
        assert_eq!(reopened.version(), version);
        assert_eq!(reopened.root_hash(), root);
    }

    #[test]
    fn sbmt_auto_snapshot_and_background_flush() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sbmt.snapshot");

        let mut jmt = PersistentSbmt::open(&path, 3).unwrap();
        jmt.apply_diff(&make_diff(10)).unwrap();
        jmt.apply_diff(&make_diff(10)).unwrap();
        assert_eq!(jmt.last_snapshot_version(), 0);
        jmt.apply_diff(&make_diff(10)).unwrap(); // version 3 → background snapshot
        assert_eq!(jmt.last_snapshot_version(), 3);
        jmt.join_pending().unwrap();
        let root = jmt.root_hash();
        drop(jmt);

        let reopened = PersistentSbmt::open(&path, 3).unwrap();
        assert_eq!(reopened.version(), 3);
        assert_eq!(reopened.root_hash(), root);
    }

    #[test]
    fn open_empty_when_no_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.snapshot");
        let jmt = PersistentJmt::open(&path, 10).unwrap();
        assert_eq!(jmt.version(), 0);
        assert_eq!(jmt.last_snapshot_version(), 0);
        // An empty tree's combined root is blake3 over 16 zero shard-roots —
        // deterministic but non-zero. Match a fresh in-memory tree.
        let expected = ShardedJmt::new().root_hash().unwrap();
        assert_eq!(jmt.root_hash().unwrap(), expected);
    }
}
