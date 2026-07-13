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
use crate::twig::TwigState;

use alloy_primitives::B256;
use jmt::Version;
use n42_execution::state_diff::StateDiff;
use n42_twig_core::TwigSnapshot;
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
    /// Set once a WAL append or apply fails: the sink is poisoned and refuses
    /// every further apply, so it can never silently produce a root that has
    /// diverged from healthy nodes (F1a/F1b). Carries the failing version +
    /// cause for diagnostics.
    poisoned: Option<String>,
    /// Whether any state was restored from a snapshot or WAL on open (F6).
    restored_from_disk: bool,
}

/// Read all `(version, StateDiff)` records from a WAL file (empty if absent).
///
/// Record format: `[version: u64 LE][len: u32 LE][bincode(StateDiff): len]`.
///
/// A *truncated trailing* record (a crash mid-append whose declared length runs
/// past EOF) is tolerated — every complete record before it is durable. But a
/// record whose bytes are fully present yet fail to decode is *interior*
/// corruption: it is an error, not a truncation, because silently `break`ing
/// there would discard every later, fully durable record (F1a). The append path
/// rolls back a partial write so this pathological "partial followed by valid"
/// layout should never be produced in the first place; failing loud here is the
/// backstop for on-disk corruption the append path could not have prevented.
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
            break; // truncated trailing record — tolerate
        }
        match bincode::deserialize::<StateDiff>(&data[start..start + len]) {
            Ok(diff) => entries.push((version, diff)),
            Err(e) => {
                return Err(eyre::eyre!(
                    "WAL record at offset {off} (version {version}, len {len}) is fully present but \
                     does not decode: {e}; refusing to silently drop later durable records"
                ));
            }
        }
        off = start + len;
    }
    Ok(entries)
}

fn twig_snapshot_entry_count(snapshot: &TwigSnapshot) -> usize {
    snapshot.shards.iter().map(|s| s.entries.len()).sum()
}

fn save_twig_snapshot(path: &Path, snapshot: &TwigSnapshot) -> eyre::Result<()> {
    let start = std::time::Instant::now();

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let raw = bincode::serialize(snapshot)?;
    let compressed = zstd::bulk::compress(&raw, 3)?;
    let tmp_path = path.with_extension("tmp");
    {
        let mut f = File::create(&tmp_path)?;
        f.write_all(&compressed)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp_path, path)?;
    #[cfg(unix)]
    if let Some(parent) = path.parent()
        && let Ok(dir) = File::open(parent)
    {
        let _ = dir.sync_all();
    }

    let elapsed_ms = start.elapsed().as_millis();
    tracing::info!(
        target: "n42::jmt",
        version = snapshot.version,
        entries = twig_snapshot_entry_count(snapshot),
        raw_kb = raw.len() / 1024,
        compressed_kb = compressed.len() / 1024,
        elapsed_ms,
        path = %path.display(),
        "Twig snapshot saved"
    );
    Ok(())
}

fn load_twig_snapshot(path: &Path) -> eyre::Result<Option<TwigSnapshot>> {
    if !path.exists() {
        return Ok(None);
    }

    let start = std::time::Instant::now();
    let compressed = std::fs::read(path)?;
    let raw = zstd::stream::decode_all(&compressed[..])?;
    let snapshot: TwigSnapshot = bincode::deserialize(&raw)?;
    let elapsed_ms = start.elapsed().as_millis();

    tracing::info!(
        target: "n42::jmt",
        version = snapshot.version,
        entries = twig_snapshot_entry_count(&snapshot),
        elapsed_ms,
        path = %path.display(),
        "Twig snapshot loaded"
    );
    Ok(Some(snapshot))
}

impl PersistentSbmt {
    /// Open a persistent SBMT, restoring from an existing snapshot if present.
    pub fn open(snapshot_path: impl AsRef<Path>, snapshot_interval: u64) -> eyre::Result<Self> {
        let snapshot_path = snapshot_path.as_ref().to_path_buf();
        let wal_path = snapshot_path.with_extension("wal");
        let (mut inner, last_snapshot_version, snapshot_present) =
            match load_snapshot(&snapshot_path)? {
                Some(snap) => {
                    let version = snap.version;
                    (ShardedSbmt::from_snapshot(&snap)?, version, true)
                }
                None => (ShardedSbmt::new(), 0, false),
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
            inner.apply_diff(&diff)?;
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
            poisoned: None,
            restored_from_disk: snapshot_present || replayed > 0,
        })
    }

    /// Current global version.
    pub fn version(&self) -> u64 {
        self.inner.version()
    }

    /// Whether the sink has been poisoned by a WAL/apply failure.
    pub fn is_poisoned(&self) -> bool {
        self.poisoned.is_some()
    }

    /// The reason this sink was poisoned, if any.
    pub fn poison_reason(&self) -> Option<&str> {
        self.poisoned.as_deref()
    }

    /// Whether any state was restored from a snapshot or WAL on open (F6).
    /// SBMT is root-neutral to a re-seed (map semantics), but the guard keeps
    /// the two sinks symmetric and avoids a redundant reseed on every restart.
    pub fn restored_from_disk(&self) -> bool {
        self.restored_from_disk
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
        // Once poisoned, refuse without touching the WAL or memory. A sink that
        // silently kept producing wrong roots is worse than one that stops.
        if let Some(reason) = &self.poisoned {
            return Err(eyre::eyre!("SBMT sink is poisoned, refusing apply: {reason}"));
        }
        // WAL-ahead: append the record durably BEFORE mutating memory, so a crash
        // can only lose in-memory state (recoverable by replay) and never leave
        // memory ahead of the WAL. `apply_diff` will assign exactly this version.
        let next_version = self.inner.version() + 1;
        if let Err(e) = self.append_wal(next_version, diff) {
            // The WAL append is the only fallible step before memory mutation.
            // If it fails, this block's diff is not durable; poison rather than
            // continue, so block N+1 can never be applied as version N and drift
            // the version→block mapping (F1b).
            let reason = format!("WAL append failed at version {next_version}: {e}");
            self.poisoned = Some(reason.clone());
            return Err(eyre::eyre!(reason));
        }
        let (version, root) = match self.inner.apply_diff(diff) {
            Ok(vr) => vr,
            Err(e) => {
                // A read-miss (F5) or other apply error must poison the sink,
                // not silently continue with a diverged root. The WAL record is
                // already durable (a valid block diff); a clean replay from a
                // good snapshot may still apply it, so this halts only the
                // current session rather than corrupting persisted state.
                let reason = format!("state-tree apply failed at version {next_version}: {e}");
                self.poisoned = Some(reason.clone());
                return Err(eyre::eyre!(reason));
            }
        };
        debug_assert_eq!(version, next_version);
        if version.saturating_sub(self.last_snapshot_version) >= self.snapshot_interval {
            // Synchronous checkpoint so the WAL can be safely truncated afterward.
            self.flush()?;
        }
        Ok((version, root))
    }

    /// Append a `(version, diff)` record to the WAL and flush it to the OS,
    /// rolling the file back to the previous record boundary on any write/sync
    /// error so a partial record can never be followed by a later append (F1a).
    fn append_wal(&mut self, version: u64, diff: &StateDiff) -> eyre::Result<()> {
        let bytes = bincode::serialize(diff)?;
        // Durable length before this record; the rollback target.
        let prev_len = self.wal.metadata()?.len();
        match Self::write_wal_record(&mut self.wal, version, &bytes) {
            Ok(()) => Ok(()),
            Err(e) => {
                // Truncate the partial record and re-sync. If the rollback
                // itself fails the file may still hold a partial record, so the
                // caller must poison the sink (no further appends).
                match self.wal.set_len(prev_len).and_then(|()| self.wal.sync_data()) {
                    Ok(()) => Err(eyre::eyre!(
                        "WAL append failed at version {version}, rolled back to {prev_len} bytes: {e}"
                    )),
                    Err(re) => Err(eyre::eyre!(
                        "WAL append failed at version {version} and rollback failed ({re}); \
                         a partial record may remain: {e}"
                    )),
                }
            }
        }
    }

    /// Write one `[version][len][bincode(diff)]` record and fsync it. A free
    /// function so `append_wal` can hold the rollback borrow of `self.wal`.
    ///
    /// fsync makes the record durable against OS/power crash, not just a process
    /// crash. SBMT applies run in a background task (off the consensus critical
    /// path), so the per-block fsync is affordable.
    fn write_wal_record(wal: &mut File, version: u64, bytes: &[u8]) -> std::io::Result<()> {
        wal.write_all(&version.to_le_bytes())?;
        wal.write_all(&(bytes.len() as u32).to_le_bytes())?;
        wal.write_all(bytes)?;
        wal.sync_data()
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

    /// Test-only: swap the WAL handle for a read-only one so the next append
    /// deterministically fails, exercising the F1a rollback + F1b poison path.
    #[cfg(test)]
    pub(crate) fn break_wal_handle_for_test(&mut self) {
        self.wal = OpenOptions::new()
            .read(true)
            .open(&self.wal_path)
            .expect("reopen WAL read-only");
    }
}

impl Drop for PersistentSbmt {
    fn drop(&mut self) {
        let _ = self.join_pending();
    }
}

/// In-memory [`TwigState`] with snapshot + write-ahead-log durability.
///
/// Mirrors [`PersistentSbmt`]'s crash-recovery contract for the twig engine:
/// every committed block is appended to a durable WAL before in-memory mutation,
/// periodic snapshots checkpoint the full append-slot history, and recovery
/// replays only strictly contiguous WAL records newer than the snapshot.
pub struct PersistentTwig {
    inner: TwigState,
    snapshot_path: PathBuf,
    wal_path: PathBuf,
    wal: File,
    snapshot_interval: u64,
    last_snapshot_version: u64,
    pending_flush: Option<JoinHandle<eyre::Result<()>>>,
    /// Set once a WAL append or apply fails: the sink is poisoned and refuses
    /// every further apply, so it can never silently produce a root that has
    /// diverged from healthy nodes (F1a/F1b). Carries the failing version +
    /// cause for diagnostics.
    poisoned: Option<String>,
    /// Whether any state was restored from a snapshot or WAL on open (F6). The
    /// caller uses this to skip idempotency-breaking genesis reseeding even when
    /// the restored version is still 0 (genesis seeding does not bump version).
    restored_from_disk: bool,
}

impl PersistentTwig {
    /// Open a persistent twig state tree, restoring from snapshot + WAL if present.
    pub fn open(snapshot_path: impl AsRef<Path>, snapshot_interval: u64) -> eyre::Result<Self> {
        let snapshot_path = snapshot_path.as_ref().to_path_buf();
        let wal_path = snapshot_path.with_extension("wal");
        let (mut inner, last_snapshot_version, snapshot_present) =
            match load_twig_snapshot(&snapshot_path)? {
                Some(snap) => {
                    let version = snap.version;
                    (TwigState::from_snapshot(&snap), version, true)
                }
                None => (TwigState::new(), 0, false),
            };

        let mut replayed = 0u64;
        for (version, diff) in read_wal_entries(&wal_path)? {
            let expected = inner.version() + 1;
            if version < expected {
                continue;
            }
            if version != expected {
                return Err(eyre::eyre!(
                    "Twig WAL is non-contiguous: expected version {expected}, found {version}; \
                     refusing to replay to avoid silent state corruption"
                ));
            }
            inner.apply_diff(&diff)?;
            replayed += 1;
        }
        if replayed > 0 {
            tracing::info!(
                target: "n42::jmt",
                replayed,
                version = inner.version(),
                "replayed Twig WAL on open"
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
            poisoned: None,
            restored_from_disk: snapshot_present || replayed > 0,
        })
    }

    /// Current global version.
    pub fn version(&self) -> u64 {
        self.inner.version()
    }

    /// Whether the sink has been poisoned by a WAL/apply failure.
    pub fn is_poisoned(&self) -> bool {
        self.poisoned.is_some()
    }

    /// The reason this sink was poisoned, if any.
    pub fn poison_reason(&self) -> Option<&str> {
        self.poisoned.as_deref()
    }

    /// Whether any state was restored from a snapshot or WAL on open. When true,
    /// the caller must NOT reseed genesis even at version 0 (F6): re-`set`ting an
    /// existing key in the append-slot model deactivates the old slot and
    /// appends a new one, producing a different root than an un-restarted node.
    pub fn restored_from_disk(&self) -> bool {
        self.restored_from_disk
    }

    /// Combined root hash across all twig shards.
    pub fn root_hash(&mut self) -> B256 {
        self.inner.root()
    }

    /// Borrow the underlying in-memory twig state tree.
    pub fn inner(&self) -> &TwigState {
        &self.inner
    }

    /// Mutable access to the underlying tree, for genesis seeding at version 0 only.
    pub fn inner_mut(&mut self) -> &mut TwigState {
        &mut self.inner
    }

    /// The version most recently persisted to a snapshot.
    pub fn last_snapshot_version(&self) -> u64 {
        self.last_snapshot_version
    }

    /// Apply a `StateDiff`, triggering a durable checkpoint on snapshot interval.
    pub fn apply_diff(&mut self, diff: &StateDiff) -> eyre::Result<(u64, B256)> {
        // Once poisoned, refuse without touching the WAL or memory. The twig
        // root is a function of append history, so a silent continue after a
        // failed step diverges the root from every healthy node permanently.
        if let Some(reason) = &self.poisoned {
            return Err(eyre::eyre!("Twig sink is poisoned, refusing apply: {reason}"));
        }
        let next_version = self.inner.version() + 1;
        if let Err(e) = self.append_wal(next_version, diff) {
            // WAL append is the only fallible step before memory mutation; on
            // failure poison rather than continue, so block N+1 can never be
            // applied as version N and drift the version→block mapping (F1b).
            let reason = format!("WAL append failed at version {next_version}: {e}");
            self.poisoned = Some(reason.clone());
            return Err(eyre::eyre!(reason));
        }
        let (version, root) = match self.inner.apply_diff(diff) {
            Ok(vr) => vr,
            Err(e) => {
                // A read-miss (F5) or other apply error must poison the sink,
                // not silently continue with a diverged root. The WAL record is
                // already durable (a valid block diff); a clean replay from a
                // good snapshot may still apply it, so this halts only the
                // current session rather than corrupting persisted state.
                let reason = format!("state-tree apply failed at version {next_version}: {e}");
                self.poisoned = Some(reason.clone());
                return Err(eyre::eyre!(reason));
            }
        };
        debug_assert_eq!(version, next_version);
        if version.saturating_sub(self.last_snapshot_version) >= self.snapshot_interval {
            self.flush()?;
        }
        Ok((version, root))
    }

    /// Append a `(version, diff)` record to the WAL, rolling the file back to the
    /// previous record boundary on any write/sync error so a partial record can
    /// never be followed by a later append (F1a).
    fn append_wal(&mut self, version: u64, diff: &StateDiff) -> eyre::Result<()> {
        let bytes = bincode::serialize(diff)?;
        let prev_len = self.wal.metadata()?.len();
        match Self::write_wal_record(&mut self.wal, version, &bytes) {
            Ok(()) => Ok(()),
            Err(e) => match self.wal.set_len(prev_len).and_then(|()| self.wal.sync_data()) {
                Ok(()) => Err(eyre::eyre!(
                    "WAL append failed at version {version}, rolled back to {prev_len} bytes: {e}"
                )),
                Err(re) => Err(eyre::eyre!(
                    "WAL append failed at version {version} and rollback failed ({re}); \
                     a partial record may remain: {e}"
                )),
            },
        }
    }

    /// Write one `[version][len][bincode(diff)]` record and fsync it. A free
    /// function so `append_wal` can hold the rollback borrow of `self.wal`.
    fn write_wal_record(wal: &mut File, version: u64, bytes: &[u8]) -> std::io::Result<()> {
        wal.write_all(&version.to_le_bytes())?;
        wal.write_all(&(bytes.len() as u32).to_le_bytes())?;
        wal.write_all(bytes)?;
        wal.sync_data()
    }

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

    /// Synchronous snapshot flush: durably writes snapshot, then truncates WAL.
    pub fn flush(&mut self) -> eyre::Result<()> {
        self.join_pending()?;
        let snapshot = self.inner.snapshot();
        self.last_snapshot_version = snapshot.version;
        save_twig_snapshot(&self.snapshot_path, &snapshot)?;
        self.truncate_wal()
    }

    /// Background snapshot flush: serialization + IO off the apply path.
    ///
    /// This mirrors [`PersistentSbmt::flush_background`]: it does not truncate the
    /// WAL because the snapshot is not yet durable when the method returns.
    pub fn flush_background(&mut self) -> eyre::Result<()> {
        self.join_pending()?;
        let snapshot = self.inner.snapshot();
        self.last_snapshot_version = snapshot.version;
        let path = self.snapshot_path.clone();
        debug!(
            target: "n42::jmt",
            version = snapshot.version,
            entries = twig_snapshot_entry_count(&snapshot),
            "spawning background Twig snapshot flush"
        );
        self.pending_flush = Some(std::thread::spawn(move || {
            save_twig_snapshot(&path, &snapshot)
        }));
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

    /// Test-only: swap the WAL handle for a read-only one so the next append
    /// deterministically fails, exercising the F1a rollback + F1b poison path.
    #[cfg(test)]
    pub(crate) fn break_wal_handle_for_test(&mut self) {
        self.wal = OpenOptions::new()
            .read(true)
            .open(&self.wal_path)
            .expect("reopen WAL read-only");
    }
}

impl Drop for PersistentTwig {
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
            assert_eq!(
                jmt.last_snapshot_version(),
                0,
                "no snapshot should have fired"
            );
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
    fn twig_wal_recovers_unsnapshotted_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("twig.snapshot");

        let (root, version) = {
            let mut twig = PersistentTwig::open(&path, u64::MAX).unwrap();
            twig.apply_diff(&make_diff(50)).unwrap();
            twig.apply_diff(&make_diff(30)).unwrap();
            twig.apply_diff(&make_diff(10)).unwrap();
            assert_eq!(
                twig.last_snapshot_version(),
                0,
                "no snapshot should have fired"
            );
            (twig.root_hash(), twig.version())
        };

        let mut reopened = PersistentTwig::open(&path, u64::MAX).unwrap();
        assert_eq!(
            reopened.version(),
            version,
            "WAL must recover every committed block"
        );
        assert_eq!(reopened.root_hash(), root);
    }

    #[test]
    fn twig_wal_truncated_after_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("twig.snapshot");
        let wal = path.with_extension("wal");

        let mut twig = PersistentTwig::open(&path, 2).unwrap();
        twig.apply_diff(&make_diff(10)).unwrap();
        twig.apply_diff(&make_diff(10)).unwrap();
        assert_eq!(twig.last_snapshot_version(), 2);
        let wal_len = std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);
        assert_eq!(wal_len, 0, "WAL must be empty after a checkpoint");

        let root = twig.root_hash();
        drop(twig);
        let mut reopened = PersistentTwig::open(&path, 2).unwrap();
        assert_eq!(reopened.version(), 2);
        assert_eq!(reopened.root_hash(), root);
    }

    #[test]
    fn twig_apply_flush_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("twig.snapshot");

        let (root, version) = {
            let mut twig = PersistentTwig::open(&path, u64::MAX).unwrap();
            twig.apply_diff(&make_diff(100)).unwrap();
            twig.apply_diff(&make_diff(40)).unwrap();
            let root = twig.root_hash();
            let version = twig.version();
            twig.flush().unwrap();
            assert_eq!(twig.last_snapshot_version(), version);
            (root, version)
        };

        let mut reopened = PersistentTwig::open(&path, u64::MAX).unwrap();
        assert_eq!(reopened.version(), version);
        assert_eq!(reopened.root_hash(), root);
    }

    #[test]
    fn twig_genesis_seed_snapshot_baseline_survives_wal_replay() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("twig.snapshot");

        let (root, version) = {
            let mut twig = PersistentTwig::open(&path, u64::MAX).unwrap();
            twig.inner_mut().seed_genesis_account(
                Address::with_last_byte(0x42),
                U256::from(1_000_000u64),
                7,
                B256::ZERO,
                std::iter::empty::<(U256, U256)>(),
            );
            twig.flush().unwrap();
            assert_eq!(twig.last_snapshot_version(), 0);

            twig.apply_diff(&make_diff(25)).unwrap();
            (twig.root_hash(), twig.version())
        };

        let mut reopened = PersistentTwig::open(&path, u64::MAX).unwrap();
        assert_eq!(reopened.version(), version);
        assert_eq!(reopened.root_hash(), root);
    }

    /// Build a raw WAL byte stream from `(version, StateDiff)` records for the
    /// corruption tests (mirrors `append_wal`'s on-disk format).
    fn encode_wal_records(records: &[(u64, StateDiff)]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for (version, diff) in records {
            let body = bincode::serialize(diff).unwrap();
            bytes.extend_from_slice(&version.to_le_bytes());
            bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
            bytes.extend_from_slice(&body);
        }
        bytes
    }

    /// F1a: a truncated trailing record (crash mid-append) is tolerated — every
    /// complete record before it still replays.
    #[test]
    fn wal_truncated_trailing_record_is_tolerated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("twig.snapshot");
        let wal_path = path.with_extension("wal");

        let mut bytes = encode_wal_records(&[(1, make_diff(3)), (2, make_diff(4))]);
        // Truncated trailing record: header claims len=100 but only 10 body bytes.
        bytes.extend_from_slice(&3u64.to_le_bytes());
        bytes.extend_from_slice(&100u32.to_le_bytes());
        bytes.extend_from_slice(&[0u8; 10]);
        std::fs::write(&wal_path, &bytes).unwrap();

        let reopened = PersistentTwig::open(&path, u64::MAX).unwrap();
        assert_eq!(
            reopened.version(),
            2,
            "both complete records must replay; the truncated tail is dropped"
        );
    }

    /// F1a: a record whose bytes are fully present but do not decode is interior
    /// corruption — `open` must fail loud rather than silently drop later records.
    #[test]
    fn wal_interior_corruption_fails_loud() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("twig.snapshot");
        let wal_path = path.with_extension("wal");

        let mut bytes = encode_wal_records(&[(1, make_diff(3))]);
        // A fully-present but undecodable record (huge bincode map length → EOF).
        let garbage = vec![0xFFu8; 32];
        bytes.extend_from_slice(&2u64.to_le_bytes());
        bytes.extend_from_slice(&(garbage.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&garbage);
        std::fs::write(&wal_path, &bytes).unwrap();

        let err = match PersistentTwig::open(&path, u64::MAX) {
            Ok(_) => panic!("interior WAL corruption must fail open, not succeed"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("does not decode"),
            "interior WAL corruption must fail loud, got: {err}"
        );
    }

    /// F1b: a WAL append failure poisons the sink; memory does not advance and
    /// every later apply fails fast without touching the WAL or the tree.
    #[test]
    fn twig_apply_failure_poisons_sink_and_fails_fast() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("twig.snapshot");
        let mut twig = PersistentTwig::open(&path, u64::MAX).unwrap();
        twig.apply_diff(&make_diff(5)).unwrap();
        let version_before = twig.version();
        assert!(!twig.is_poisoned());

        twig.break_wal_handle_for_test();
        let err = twig.apply_diff(&make_diff(5)).unwrap_err();
        assert!(
            err.to_string().contains("WAL append failed"),
            "the first failure should be a WAL append error: {err}"
        );
        assert!(twig.is_poisoned(), "a WAL failure must poison the sink");
        assert_eq!(
            twig.version(),
            version_before,
            "memory must not advance past a non-durable block"
        );

        let err2 = twig.apply_diff(&make_diff(5)).unwrap_err();
        assert!(
            err2.to_string().contains("poisoned"),
            "further applies must fail fast: {err2}"
        );
        assert_eq!(twig.version(), version_before);
    }

    /// F1b mirror on the SBMT sink.
    #[test]
    fn sbmt_apply_failure_poisons_sink_and_fails_fast() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sbmt.snapshot");
        let mut sbmt = PersistentSbmt::open(&path, u64::MAX).unwrap();
        sbmt.apply_diff(&make_diff(5)).unwrap();
        let version_before = sbmt.version();

        sbmt.break_wal_handle_for_test();
        let _ = sbmt.apply_diff(&make_diff(5)).unwrap_err();
        assert!(sbmt.is_poisoned(), "a WAL failure must poison the SBMT sink");
        assert_eq!(sbmt.version(), version_before);
        let err2 = sbmt.apply_diff(&make_diff(5)).unwrap_err();
        assert!(err2.to_string().contains("poisoned"));
    }

    /// F6: after a restart-before-first-block, a restored twig must NOT reseed
    /// genesis. The guard keeps the append-ordered root identical; the same test
    /// shows that reseeding a restored tree WOULD change the root (the bug).
    #[test]
    fn twig_genesis_reseed_guard_keeps_root_stable_across_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("twig.snapshot");

        let seed = |twig: &mut PersistentTwig| {
            twig.inner_mut().seed_genesis_account(
                Address::with_last_byte(0x42),
                U256::from(1_000_000u64),
                7,
                B256::ZERO,
                std::iter::empty::<(U256, U256)>(),
            );
        };

        // First boot: fresh start, seed genesis, flush a version-0 snapshot.
        let root_after_seed = {
            let mut twig = PersistentTwig::open(&path, u64::MAX).unwrap();
            assert!(!twig.restored_from_disk(), "a fresh start is not restored");
            assert!(twig.version() == 0 && !twig.restored_from_disk());
            seed(&mut twig);
            twig.flush().unwrap();
            assert_eq!(twig.version(), 0, "genesis seeding does not bump the version");
            twig.root_hash()
        };

        // Restart before the first block: the version-0 snapshot is restored.
        {
            let mut twig = PersistentTwig::open(&path, u64::MAX).unwrap();
            assert!(
                twig.restored_from_disk(),
                "a persisted snapshot marks the tree restored"
            );
            assert!(
                !(twig.version() == 0 && !twig.restored_from_disk()),
                "the reseed guard must be false for a restored tree"
            );
            assert_eq!(
                twig.root_hash(),
                root_after_seed,
                "the root must be identical after a guarded restart"
            );
        }

        // Sanity / bug demonstration: reseeding a restored append-slot tree DOES
        // change the root — exactly what the guard prevents.
        {
            let mut twig = PersistentTwig::open(&path, u64::MAX).unwrap();
            seed(&mut twig);
            assert_ne!(
                twig.root_hash(),
                root_after_seed,
                "reseeding a restored append-slot tree changes the root (the F6 bug)"
            );
        }
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
