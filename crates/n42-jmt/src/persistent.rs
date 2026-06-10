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
use std::fs::{File, OpenOptions};
use std::io::Write;
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

/// In-memory [`ShardedSbmt`] with snapshot + write-ahead-log durability.
///
/// Every `apply_diff` appends the `StateDiff` to a WAL (durable per block); a
/// snapshot every `snapshot_interval` versions checkpoints full state and then
/// truncates the WAL. Recovery = load snapshot + replay WAL, so **no committed
/// block is lost on crash** — this closes the earlier "crash-recovery granularity
/// == snapshot interval" limitation of the snapshot-only design.
pub struct PersistentSbmt {
    inner: ShardedSbmt,
    snapshot_path: PathBuf,
    /// WAL path (`<snapshot>.wal`).
    wal_path: PathBuf,
    /// Append-only log of per-block `StateDiff`s since the last snapshot.
    wal: File,
    snapshot_interval: u64,
    last_snapshot_version: u64,
    pending_flush: Option<JoinHandle<eyre::Result<()>>>,
}

/// Read all `(version, StateDiff)` records from a WAL file (empty if absent).
///
/// Record format: `[version: u64 LE][len: u32 LE][bincode(StateDiff): len]`.
/// A truncated or corrupt trailing record (crash mid-write) is discarded.
fn read_wal_entries(path: &Path) -> eyre::Result<Vec<(u64, StateDiff)>> {
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(_) => return Ok(Vec::new()),
    };
    let mut entries = Vec::new();
    let mut off = 0usize;
    while off + 12 <= data.len() {
        let version = u64::from_le_bytes(data[off..off + 8].try_into().unwrap());
        let len = u32::from_le_bytes(data[off + 8..off + 12].try_into().unwrap()) as usize;
        let start = off + 12;
        if start + len > data.len() {
            break; // truncated trailing record
        }
        match bincode::deserialize::<StateDiff>(&data[start..start + len]) {
            Ok(diff) => entries.push((version, diff)),
            Err(_) => break, // corrupt trailing record
        }
        off = start + len;
    }
    Ok(entries)
}

impl PersistentSbmt {
    /// Open a persistent SBMT, restoring from an existing snapshot if present.
    pub fn open(snapshot_path: impl AsRef<Path>, snapshot_interval: u64) -> eyre::Result<Self> {
        let snapshot_path = snapshot_path.as_ref().to_path_buf();
        let wal_path = snapshot_path.with_extension("wal");
        let (mut inner, last_snapshot_version) = match load_snapshot(&snapshot_path)? {
            Some(snap) => {
                let version = snap.version;
                (ShardedSbmt::from_snapshot(&snap)?, version)
            }
            None => (ShardedSbmt::new(), 0),
        };

        // Replay WAL records newer than the snapshot to recover committed blocks
        // that were not yet checkpointed when the process stopped. Records must be
        // strictly contiguous (each is the next version): `apply_diff` derives the
        // version internally, so a gap would be silently misapplied at the wrong
        // version. Fail loud instead of corrupting state.
        let mut replayed = 0u64;
        for (version, diff) in read_wal_entries(&wal_path)? {
            let expected = inner.version() + 1;
            if version < expected {
                continue; // already covered by the snapshot
            }
            if version != expected {
                return Err(eyre::eyre!(
                    "SBMT WAL is non-contiguous: expected version {expected}, found {version}; \
                     refusing to replay to avoid silent state corruption"
                ));
            }
            inner.apply_diff(&diff);
            replayed += 1;
        }
        if replayed > 0 {
            tracing::info!(
                target: "n42::jmt",
                replayed,
                version = inner.version(),
                "replayed SBMT WAL on open"
            );
        }

        let wal = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&wal_path)?;

        Ok(Self {
            inner,
            snapshot_path,
            wal_path,
            wal,
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

    /// Mutable access to the underlying tree, for **genesis seeding at version 0
    /// only**. This bypasses the WAL: genesis state is re-derivable from the
    /// chain spec and is only seeded when no snapshot exists (version 0), so it
    /// needs no WAL record. Do not use for block application — use `apply_diff`.
    pub fn inner_mut(&mut self) -> &mut ShardedSbmt {
        &mut self.inner
    }

    /// The version most recently persisted to a snapshot.
    pub fn last_snapshot_version(&self) -> u64 {
        self.last_snapshot_version
    }

    /// Apply a `StateDiff` (zero node IO), triggering a background snapshot once
    /// `snapshot_interval` versions have elapsed since the last persisted one.
    pub fn apply_diff(&mut self, diff: &StateDiff) -> eyre::Result<(u64, B256)> {
        // WAL-ahead: append the record durably BEFORE mutating memory, so a crash
        // can only lose in-memory state (recoverable by replay) and never leave
        // memory ahead of the WAL. `apply_diff` will assign exactly this version.
        let next_version = self.inner.version() + 1;
        self.append_wal(next_version, diff)?;
        let (version, root) = self.inner.apply_diff(diff);
        debug_assert_eq!(version, next_version);
        if version.saturating_sub(self.last_snapshot_version) >= self.snapshot_interval {
            // Synchronous checkpoint so the WAL can be safely truncated afterward.
            self.flush()?;
        }
        Ok((version, root))
    }

    /// Append a `(version, diff)` record to the WAL and flush it to the OS.
    fn append_wal(&mut self, version: u64, diff: &StateDiff) -> eyre::Result<()> {
        let bytes = bincode::serialize(diff)?;
        self.wal.write_all(&version.to_le_bytes())?;
        self.wal.write_all(&(bytes.len() as u32).to_le_bytes())?;
        self.wal.write_all(&bytes)?;
        // fsync so the record is durable against OS/power crash, not just a
        // process crash. SBMT applies run in a background spawn_blocking task
        // (off the consensus critical path), so the per-block fsync is affordable.
        self.wal.sync_data()?;
        Ok(())
    }

    /// Clear the WAL after a durable snapshot, then reopen it for append.
    fn truncate_wal(&mut self) -> eyre::Result<()> {
        drop(
            OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&self.wal_path)?,
        );
        self.wal = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.wal_path)?;
        Ok(())
    }

    /// Synchronous snapshot flush: blocks until durably written, then truncates
    /// the WAL (the snapshot now covers everything up to `last_snapshot_version`).
    pub fn flush(&mut self) -> eyre::Result<()> {
        self.join_pending()?;
        let snapshot = self.inner.snapshot();
        self.last_snapshot_version = snapshot.version;
        save_snapshot(&self.snapshot_path, &snapshot)?;
        self.truncate_wal()
    }

    /// Background snapshot flush: serialization + IO off the apply path.
    ///
    /// Unlike [`flush`](Self::flush), this does **not** truncate the WAL (the
    /// snapshot is not yet durable when this returns). WAL checkpointing happens
    /// via the synchronous `flush` path; use this only when you manage durability
    /// ordering yourself.
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
    fn sbmt_wal_recovers_unsnapshotted_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sbmt.snapshot");

        let (root, version) = {
            // interval = MAX → no snapshot ever fires; durability is purely WAL.
            let mut jmt = PersistentSbmt::open(&path, u64::MAX).unwrap();
            jmt.apply_diff(&make_diff(50)).unwrap();
            jmt.apply_diff(&make_diff(30)).unwrap();
            jmt.apply_diff(&make_diff(10)).unwrap();
            assert_eq!(jmt.last_snapshot_version(), 0, "no snapshot should have fired");
            (jmt.root_hash(), jmt.version())
            // drop without flush() — simulates a crash with no snapshot taken.
        };

        // Reopen: the snapshot file is absent, so recovery must come from WAL replay.
        let reopened = PersistentSbmt::open(&path, u64::MAX).unwrap();
        assert_eq!(
            reopened.version(),
            version,
            "WAL must recover every committed block"
        );
        assert_eq!(reopened.root_hash(), root);
    }

    #[test]
    fn sbmt_wal_truncated_after_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sbmt.snapshot");
        let wal = path.with_extension("wal");

        let mut jmt = PersistentSbmt::open(&path, 2).unwrap(); // snapshot every 2 versions
        jmt.apply_diff(&make_diff(10)).unwrap();
        jmt.apply_diff(&make_diff(10)).unwrap(); // version 2 → snapshot + WAL truncate
        assert_eq!(jmt.last_snapshot_version(), 2);
        let wal_len = std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);
        assert_eq!(wal_len, 0, "WAL must be empty after a checkpoint");

        let root = jmt.root_hash();
        drop(jmt);
        let reopened = PersistentSbmt::open(&path, 2).unwrap();
        assert_eq!(reopened.root_hash(), root);
        assert_eq!(reopened.version(), 2);
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
