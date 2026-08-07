use alloy_primitives::B256;
use n42_chainspec::ValidatorInfo;
use n42_consensus::error::{ConsensusError, ConsensusResult};
use n42_consensus::vote_log::map_io_err;
use n42_consensus::{ValidatorSet, VoteLogWriter};
use n42_primitives::consensus::QuorumCertificate;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const EXECUTION_LINEAGE_MAGIC: &[u8; 4] = b"N42E";
const EXECUTION_LINEAGE_RECORD_LEN: usize = 4 + 8 + 32 + 16;
const MAX_DURABLE_VOTE_WATERMARK_AHEAD: u64 = 128;

fn execution_lineage_path(snapshot_path: &Path) -> PathBuf {
    let mut path = snapshot_path.as_os_str().to_os_string();
    path.push(".execution-lineage");
    PathBuf::from(path)
}

/// Appends and fsyncs an exact execution-valid `(view, hash)` proof.
///
/// This log is deliberately separate from the replace-in-place consensus JSON:
/// after FCU returns Valid, Reth and the snapshot can become durable at
/// different instants. Retaining every recent proof lets restart map a Reth
/// head that fell back to any previously validated ancestor without guessing.
pub fn append_execution_lineage_proof(
    snapshot_path: &Path,
    view: u64,
    hash: B256,
) -> io::Result<()> {
    let path = execution_lineage_path(snapshot_path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut record = Vec::with_capacity(EXECUTION_LINEAGE_RECORD_LEN);
    record.extend_from_slice(EXECUTION_LINEAGE_MAGIC);
    record.extend_from_slice(&view.to_le_bytes());
    record.extend_from_slice(hash.as_slice());
    let checksum = blake3::hash(&record);
    record.extend_from_slice(&checksum.as_bytes()[..16]);
    debug_assert_eq!(record.len(), EXECUTION_LINEAGE_RECORD_LEN);

    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(&record)?;
    file.sync_data()
}

/// Finds the newest durable proof for `canonical_head_hash`.
///
/// A partial final record is tolerated as a crash tear. Any corruption in a
/// complete record fails closed, because silently skipping it could turn an
/// unproven hash/view pair into an accepted recovery point.
pub fn recover_execution_lineage_proof(
    snapshot_path: &Path,
    canonical_head_hash: B256,
) -> io::Result<Option<(u64, B256)>> {
    let path = execution_lineage_path(snapshot_path);
    let mut bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let complete_len = bytes.len() - (bytes.len() % EXECUTION_LINEAGE_RECORD_LEN);
    bytes.truncate(complete_len);

    let mut recovered = None;
    for record in bytes.chunks_exact(EXECUTION_LINEAGE_RECORD_LEN) {
        let payload = &record[..44];
        if &payload[..4] != EXECUTION_LINEAGE_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid execution-lineage record magic",
            ));
        }
        let expected = blake3::hash(payload);
        if record[44..] != expected.as_bytes()[..16] {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "execution-lineage record checksum mismatch",
            ));
        }
        let view = u64::from_le_bytes(
            payload[4..12]
                .try_into()
                .expect("fixed execution-lineage view width"),
        );
        let hash = B256::from_slice(&payload[12..44]);
        if hash == canonical_head_hash {
            recovered = Some((view, hash));
        }
    }
    Ok(recovered)
}

/// Snapshot of consensus state persisted to disk.
///
/// Contains the minimum state needed to safely resume after a crash:
/// - `current_view`: prevents restarting from view 1
/// - `locked_qc`: preserves the locking constraint (safety rule)
/// - `last_committed_qc`: tracks the latest committed block
/// - `consecutive_timeouts`: maintains pacemaker backoff state
/// - `scheduled_epoch_transition`: preserves staged epoch changes across restarts
/// - `current_epoch_validators`: the active validator set for the current epoch,
///   so dynamic `proposeAddValidator` changes survive restart without requiring a
///   static `epoch_schedule.json` update
///
/// v4 adds `execution_validated_head_{view,hash}` so the execution-validity
/// guard survives a restart. Without them the guard resets to view 0 while
/// reth's canonical head and the consensus view stream stay persistent — a
/// sync-imported old block could then regress `head_block_hash` and the
/// post-restart catch-up floor would be 0 (see the PR #21 restart-boundary
/// re-audit, findings F1/F2).
///
/// v5 adds `last_commit_voted_view`. Together with the expanded 16-byte vote
/// log, it extends the crash-safe same-view voting guard from R1 to R2.
const SNAPSHOT_VERSION: u32 = 5;

fn default_version() -> u32 {
    SNAPSHOT_VERSION
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsensusSnapshot {
    /// Format version for forward compatibility. Defaults to 1 for legacy snapshots.
    #[serde(default = "default_version")]
    pub version: u32,
    pub current_view: u64,
    pub locked_qc: QuorumCertificate,
    pub last_committed_qc: QuorumCertificate,
    pub consecutive_timeouts: u32,
    /// Staged epoch transition; persisted so a crash before `advance_epoch()` doesn't lose it.
    #[serde(default)]
    pub scheduled_epoch_transition: Option<(u64, Vec<ValidatorInfo>, u32)>,
    /// Legacy field kept for snapshot-format compatibility.
    /// Authorization is a live session property and is no longer restored across restart.
    #[serde(default, with = "authorized_verifiers_hex")]
    pub authorized_verifiers: Vec<[u8; 48]>,
    /// Number of blocks committed since genesis. Persisted so that the
    /// `MobileRewardManager` does not recompute epoch boundaries from 0
    /// on restart, which could cause duplicate reward payouts.
    #[serde(default)]
    pub committed_block_count: u64,
    /// Last view in which this node cast a Round-1 vote.
    /// Persisted to prevent double-voting after crash recovery (BFT safety).
    #[serde(default)]
    pub last_voted_view: u64,
    /// Last view in which this node cast an R2 CommitVote. Persisted
    /// independently from R1 so crash recovery cannot double-sign either phase.
    #[serde(default)]
    pub last_commit_voted_view: u64,
    /// Active validator set for the current epoch at the time of the snapshot.
    ///
    /// Stored as `(epoch_number, validators, fault_tolerance)`.  On restart this
    /// is used to seed the EpochManager with the correct validator set even when
    /// the static `epoch_schedule.json` has not been updated (e.g. after a
    /// `proposeAddValidator` that crossed an epoch boundary).  The field is
    /// `None` for snapshots written before this field was introduced (v2 →
    /// treated as epoch 0 with the genesis validator set).
    #[serde(default)]
    pub current_epoch_validators: Option<(u64, Vec<ValidatorInfo>, u32)>,
    /// Highest committed view whose block reth confirmed executable at snapshot
    /// time. Restored into `execution_validated_head_view` so the
    /// stale/same-view guards and the catch-up floor keep a persistent
    /// reference clock across restart. `0` for pre-v4 snapshots (missing field).
    #[serde(default)]
    pub execution_validated_head_view: u64,
    /// Block hash paired with `execution_validated_head_view` — the
    /// execution-validated head reth held at snapshot time. Restored into
    /// `head_block_hash` when it matches reth's canonical head on boot.
    /// `B256::ZERO` for pre-v4 snapshots.
    #[serde(default)]
    pub execution_validated_head_hash: B256,
}

/// Selects a restart-safe view for reth's canonical head.
///
/// A view and hash must describe the same block. Restoring a stale view next
/// to reth's newer canonical hash re-opens the backward-FCU window this state
/// is meant to close. Prefer the exact v4 pair, then independently persisted
/// consensus evidence, and finally the last committed QC when both its hash
/// and block count identify reth's tip. If none of those prove the mapping,
/// fail closed instead of guessing a lower view.
pub fn recover_execution_validated_head_view(
    snapshot: &ConsensusSnapshot,
    canonical_head_hash: B256,
    canonical_head_number: u64,
    evidence_head: Option<(u64, B256)>,
) -> Result<(u64, &'static str), String> {
    if canonical_head_number == 0 {
        return Ok((0, "genesis"));
    }

    if snapshot.execution_validated_head_hash != B256::ZERO
        && snapshot.execution_validated_head_hash == canonical_head_hash
    {
        return Ok((snapshot.execution_validated_head_view, "snapshot-exact"));
    }

    if let Some((view, hash)) = evidence_head
        && hash == canonical_head_hash
    {
        return Ok((view, "evidence-exact"));
    }

    if snapshot.committed_block_count == canonical_head_number
        && snapshot.last_committed_qc.block_hash == canonical_head_hash
    {
        return Ok((snapshot.last_committed_qc.view, "commit-qc-exact"));
    }

    Err(format!(
        "cannot map canonical head {canonical_head_hash} at block {canonical_head_number} to a consensus view (snapshot validated view={}, hash={}, committed count={}, last committed view={}, hash={})",
        snapshot.execution_validated_head_view,
        snapshot.execution_validated_head_hash,
        snapshot.committed_block_count,
        snapshot.last_committed_qc.view,
        snapshot.last_committed_qc.block_hash,
    ))
}

/// Serde helper: serialize/deserialize `Vec<[u8; 48]>` as hex strings for human-readable JSON.
mod authorized_verifiers_hex {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(keys: &[[u8; 48]], s: S) -> Result<S::Ok, S::Error> {
        let hex_keys: Vec<String> = keys.iter().map(hex::encode).collect();
        hex_keys.serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<[u8; 48]>, D::Error> {
        let hex_keys: Vec<String> = Vec::deserialize(d)?;
        hex_keys
            .iter()
            .map(|s| {
                let bytes = hex::decode(s).map_err(serde::de::Error::custom)?;
                let arr: [u8; 48] = bytes
                    .try_into()
                    .map_err(|_| serde::de::Error::custom("expected 48-byte BLS pubkey"))?;
                Ok(arr)
            })
            .collect()
    }
}

impl ConsensusSnapshot {
    /// Validates internal consistency: `last_committed ≤ locked ≤ current_view`.
    pub fn validate(&self) -> Result<(), String> {
        if self.locked_qc.view > self.current_view {
            return Err(format!(
                "locked_qc.view ({}) > current_view ({})",
                self.locked_qc.view, self.current_view
            ));
        }
        if self.last_committed_qc.view > self.locked_qc.view {
            return Err(format!(
                "last_committed_qc.view ({}) > locked_qc.view ({})",
                self.last_committed_qc.view, self.locked_qc.view
            ));
        }
        let maximum_vote_watermark = self
            .current_view
            .saturating_add(MAX_DURABLE_VOTE_WATERMARK_AHEAD);
        if self.last_voted_view > maximum_vote_watermark {
            return Err(format!(
                "last_voted_view ({}) exceeds bounded recovery watermark {} for current_view ({})",
                self.last_voted_view, maximum_vote_watermark, self.current_view
            ));
        }
        if self.last_commit_voted_view > maximum_vote_watermark {
            return Err(format!(
                "last_commit_voted_view ({}) exceeds bounded recovery watermark {} for current_view ({})",
                self.last_commit_voted_view, maximum_vote_watermark, self.current_view
            ));
        }
        if let Some((target_epoch, validators, fault_tolerance)) = &self.scheduled_epoch_transition
        {
            if *target_epoch == 0 {
                return Err("scheduled_epoch_transition target epoch must be > 0".to_string());
            }
            if validators.is_empty() {
                return Err("scheduled_epoch_transition has empty validator set".to_string());
            }
            ValidatorSet::validate_params(validators.len(), *fault_tolerance)
                .map_err(|e| format!("scheduled_epoch_transition invalid: {e}"))?;
            let peer_id_presence = validators
                .iter()
                .filter(|validator| validator.p2p_peer_id.is_some())
                .count();
            if peer_id_presence != 0 && peer_id_presence != validators.len() {
                return Err(
                    "scheduled_epoch_transition must either define p2p_peer_id for all validators or omit it for all"
                        .to_string(),
                );
            }
            let mut seen_peer_ids = std::collections::HashSet::new();
            for validator in validators {
                if let Some(peer_id) = validator.parsed_p2p_peer_id().map_err(|error| {
                    format!("scheduled_epoch_transition has invalid p2p_peer_id: {error}")
                })? && !seen_peer_ids.insert(peer_id)
                {
                    return Err(format!(
                        "scheduled_epoch_transition has duplicate validator p2p_peer_id: {peer_id}"
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Atomically saves the consensus snapshot to a JSON file.
///
/// Uses temp-file + rename to prevent corruption from partial writes.
/// On POSIX systems, `rename` is atomic within the same filesystem.
pub fn save_consensus_state(path: &Path, snapshot: &ConsensusSnapshot) -> io::Result<()> {
    // Validate snapshot before persisting to avoid writing corrupted state.
    snapshot.validate().map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("refusing to persist invalid snapshot: {e}"),
        )
    })?;

    let json = serde_json::to_string_pretty(snapshot)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let tmp_path = path.with_extension("json.tmp");
    {
        use std::io::Write;
        let mut file = std::fs::File::create(&tmp_path)?;
        file.write_all(json.as_bytes())?;
        // fsync ensures data hits disk before rename; without it a crash after
        // rename could leave a zero-length file (data still in page cache).
        file.sync_all()?;
    }
    std::fs::rename(&tmp_path, path)
}

/// Loads a consensus snapshot from a JSON file.
///
/// Returns `Ok(None)` if the file does not exist (fresh start).
/// Returns `Err` if the file exists but cannot be parsed or fails validation.
pub fn load_consensus_state(path: &Path) -> io::Result<Option<ConsensusSnapshot>> {
    match std::fs::read_to_string(path) {
        Ok(json) => {
            // v2 changed BitVec serde from per-bool to packed_bits.
            // Old snapshots cannot be deserialized into the new format,
            // so we probe the version field first before full deserialization.
            let probe: serde_json::Value = serde_json::from_str(&json)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            let probed_version = probe.get("version").and_then(|v| v.as_u64()).unwrap_or(1);
            if probed_version < 2 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "consensus snapshot v{probed_version} predates packed_bits format v2; refusing unsafe fresh start"
                    ),
                ));
            }
            let has_r2_vote_view = probe.get("last_commit_voted_view").is_some();

            let mut snapshot: ConsensusSnapshot = serde_json::from_str(&json)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

            if snapshot.version < 5 {
                // Pre-v5 snapshots did not distinguish R1 from R2. Assuming
                // the R1 view was also R2-voted can cost liveness for one view,
                // but guessing zero could let a restarted validator double-sign.
                snapshot.last_commit_voted_view = snapshot
                    .last_commit_voted_view
                    .max(snapshot.last_voted_view);
            } else if !has_r2_vote_view {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "consensus snapshot v5+ is missing last_commit_voted_view",
                ));
            }

            if snapshot.version > SNAPSHOT_VERSION {
                tracing::warn!(
                    snapshot_version = snapshot.version,
                    supported_version = SNAPSHOT_VERSION,
                    "consensus snapshot has newer version than supported; \
                     loading anyway but some fields may be ignored"
                );
            }

            if let Err(reason) = snapshot.validate() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("consensus snapshot failed validation: {reason}"),
                ));
            }

            Ok(Some(snapshot))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

// ── Vote log: durable last-voted-view storage ───────────────────────────────
//
// Fixed-size file containing two u64 LE records: highest R1 vote view at
// offset 0 and highest R2 CommitVote view at offset 8. Each field is
// overwritten in place + fsync'd before its signature is produced.

const VOTE_LOG_FIELD_BYTES: u64 = 8;
const VOTE_LOG_BYTES: u64 = 16;

/// Reads the persisted `(last_r1_view, last_r2_view)`. A missing file means no
/// votes. The legacy 8-byte R1-only format conservatively treats the same view
/// as already R2-voted; this may suppress one vote after upgrade but cannot
/// enable a double signature.
pub fn load_last_vote_views(path: &Path) -> io::Result<(u64, u64)> {
    match std::fs::File::open(path) {
        Ok(mut f) => {
            let len = f.metadata()?.len();
            if len != VOTE_LOG_FIELD_BYTES && len != VOTE_LOG_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid vote log length {len}; expected 8 or 16 bytes"),
                ));
            }
            let mut prepare = [0u8; 8];
            f.read_exact(&mut prepare)?;
            let prepare_view = u64::from_le_bytes(prepare);
            if len == VOTE_LOG_FIELD_BYTES {
                return Ok((prepare_view, prepare_view));
            }
            let mut commit = [0u8; 8];
            f.read_exact(&mut commit)?;
            Ok((prepare_view, u64::from_le_bytes(commit)))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok((0, 0)),
        Err(e) => Err(e),
    }
}

/// Backwards-compatible R1-only accessor used by existing diagnostics/tests.
pub fn load_last_voted_view(path: &Path) -> io::Result<u64> {
    load_last_vote_views(path).map(|(prepare_view, _)| prepare_view)
}

/// Persistent file-backed vote log. Always 16 bytes; fields are overwritten in place.
pub struct FileVoteLog {
    path: PathBuf,
    file: Mutex<File>,
}

impl std::fmt::Debug for FileVoteLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileVoteLog")
            .field("path", &self.path)
            .finish()
    }
}

impl FileVoteLog {
    /// Opens (creating if needed) the vote-log file at `path`. Initializes a
    /// new file to 16 zero bytes and conservatively migrates the legacy 8-byte
    /// R1-only format. Performs one fsync so initialization/migration is
    /// durable before consensus starts.
    pub fn open(path: PathBuf) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        match file.metadata()?.len() {
            0 => {
                file.write_all(&[0u8; VOTE_LOG_BYTES as usize])?;
                file.sync_all()?;
            }
            VOTE_LOG_FIELD_BYTES => {
                let mut prepare = [0u8; 8];
                file.seek(SeekFrom::Start(0))?;
                file.read_exact(&mut prepare)?;
                // Conservative migration: assume the last R1 view may also
                // have reached R2 before the old process crashed.
                file.seek(SeekFrom::Start(VOTE_LOG_FIELD_BYTES))?;
                file.write_all(&prepare)?;
                file.sync_all()?;
            }
            VOTE_LOG_BYTES => {}
            len => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid vote log length {len}; expected 0, 8, or 16 bytes"),
                ));
            }
        }
        Ok(Self {
            path,
            file: Mutex::new(file),
        })
    }

    fn record_at(&self, offset: u64, view: u64) -> ConsensusResult<()> {
        let mut guard = self
            .file
            .lock()
            .map_err(|e| ConsensusError::VoteLogFsync(format!("vote log mutex poisoned: {e}")))?;
        guard.seek(SeekFrom::Start(offset)).map_err(map_io_err)?;
        guard.write_all(&view.to_le_bytes()).map_err(map_io_err)?;
        guard.sync_data().map_err(map_io_err)?;
        crate::qualification_abort_at("vote_persisted");
        Ok(())
    }
}

impl VoteLogWriter for FileVoteLog {
    fn record_vote(&self, view: u64) -> ConsensusResult<()> {
        self.record_at(0, view)
    }

    fn record_commit_vote(&self, view: u64) -> ConsensusResult<()> {
        self.record_at(VOTE_LOG_FIELD_BYTES, view)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use n42_primitives::consensus::QuorumCertificate;

    #[test]
    fn vote_log_open_initializes_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vote_log.bin");
        assert_eq!(load_last_vote_views(&path).unwrap(), (0, 0));
        let _ = FileVoteLog::open(path.clone()).unwrap();
        assert_eq!(load_last_vote_views(&path).unwrap(), (0, 0));
        assert_eq!(std::fs::metadata(&path).unwrap().len(), VOTE_LOG_BYTES);
    }

    #[test]
    fn vote_log_records_and_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vote_log.bin");
        let log = FileVoteLog::open(path.clone()).unwrap();
        log.record_vote(42).unwrap();
        log.record_commit_vote(41).unwrap();
        // Drop the writer to release the file handle, then load.
        drop(log);
        assert_eq!(load_last_vote_views(&path).unwrap(), (42, 41));
    }

    #[test]
    fn vote_log_overwrites_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vote_log.bin");
        let log = FileVoteLog::open(path.clone()).unwrap();
        for v in [1u64, 5, 100, u64::MAX] {
            log.record_vote(v).unwrap();
            log.record_commit_vote(v.saturating_sub(1)).unwrap();
            // File stays exactly 16 bytes — overwrite, not append.
            assert_eq!(std::fs::metadata(&path).unwrap().len(), VOTE_LOG_BYTES);
        }
        drop(log);
        assert_eq!(
            load_last_vote_views(&path).unwrap(),
            (u64::MAX, u64::MAX - 1)
        );
    }

    #[test]
    fn vote_log_round_trip_after_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vote_log.bin");
        {
            let log = FileVoteLog::open(path.clone()).unwrap();
            log.record_vote(7).unwrap();
            log.record_commit_vote(6).unwrap();
        }
        // Reopen and verify the previous record persists.
        assert_eq!(load_last_vote_views(&path).unwrap(), (7, 6));
        let log = FileVoteLog::open(path.clone()).unwrap();
        log.record_vote(8).unwrap();
        log.record_commit_vote(9).unwrap();
        drop(log);
        assert_eq!(load_last_vote_views(&path).unwrap(), (8, 9));
    }

    #[test]
    fn vote_log_migrates_legacy_r1_record_conservatively() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vote_log.bin");
        std::fs::write(&path, 17u64.to_le_bytes()).unwrap();

        assert_eq!(
            load_last_vote_views(&path).unwrap(),
            (17, 17),
            "legacy reads must conservatively treat R2 as already voted"
        );
        let _ = FileVoteLog::open(path.clone()).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), VOTE_LOG_BYTES);
        assert_eq!(load_last_vote_views(&path).unwrap(), (17, 17));
    }

    #[test]
    fn vote_log_rejects_corrupt_lengths() {
        let dir = tempfile::tempdir().unwrap();
        for len in [1usize, 7, 9, 15, 17] {
            let path = dir.path().join(format!("vote_log_{len}.bin"));
            std::fs::write(&path, vec![0u8; len]).unwrap();
            assert!(load_last_vote_views(&path).is_err());
            assert!(FileVoteLog::open(path).is_err());
        }
    }

    fn genesis_snapshot(current_view: u64) -> ConsensusSnapshot {
        ConsensusSnapshot {
            version: SNAPSHOT_VERSION,
            current_view,
            locked_qc: QuorumCertificate::genesis(),
            last_committed_qc: QuorumCertificate::genesis(),
            consecutive_timeouts: 0,
            scheduled_epoch_transition: None,
            authorized_verifiers: Vec::new(),
            committed_block_count: 0,
            last_voted_view: 0,
            last_commit_voted_view: 0,
            current_epoch_validators: None,
            execution_validated_head_view: 0,
            execution_validated_head_hash: B256::ZERO,
        }
    }

    #[test]
    fn test_save_and_load_snapshot() {
        let dir = std::env::temp_dir().join("n42-test-persistence");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("consensus_state.json");

        let snapshot = ConsensusSnapshot {
            consecutive_timeouts: 3,
            ..genesis_snapshot(42)
        };
        save_consensus_state(&path, &snapshot).expect("save should succeed");
        assert!(path.exists());

        let loaded = load_consensus_state(&path).unwrap().unwrap();
        assert_eq!(loaded.version, SNAPSHOT_VERSION);
        assert_eq!(loaded.current_view, 42);
        assert_eq!(loaded.consecutive_timeouts, 3);
        assert_eq!(loaded.locked_qc.view, 0);
        assert_eq!(loaded.last_voted_view, 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_both_last_voted_views_persisted() {
        let dir = std::env::temp_dir().join("n42-test-last-voted");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("consensus_state.json");

        let snapshot = ConsensusSnapshot {
            last_voted_view: 99,
            last_commit_voted_view: 98,
            ..genesis_snapshot(100)
        };
        save_consensus_state(&path, &snapshot).unwrap();

        let loaded = load_consensus_state(&path).unwrap().unwrap();
        assert_eq!(loaded.last_voted_view, 99);
        assert_eq!(loaded.last_commit_voted_view, 98);
        assert_eq!(loaded.current_view, 100);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_nonexistent_returns_none() {
        let path = std::env::temp_dir().join("n42-test-nonexistent-state.json");
        let _ = std::fs::remove_file(&path);
        assert!(load_consensus_state(&path).unwrap().is_none());
    }

    #[test]
    fn test_load_corrupt_returns_error() {
        let dir = std::env::temp_dir().join("n42-test-corrupt");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("consensus_state.json");
        std::fs::write(&path, "not valid json {{{").unwrap();

        assert!(load_consensus_state(&path).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_atomic_write_creates_parent_dirs() {
        let dir = std::env::temp_dir().join("n42-test-deep/nested/dirs");
        let _ = std::fs::remove_dir_all(std::env::temp_dir().join("n42-test-deep"));
        let path = dir.join("consensus_state.json");

        save_consensus_state(&path, &genesis_snapshot(1)).expect("save should create parent dirs");
        assert!(path.exists());

        let _ = std::fs::remove_dir_all(std::env::temp_dir().join("n42-test-deep"));
    }

    #[test]
    fn test_load_legacy_snapshot_without_version_fails_closed() {
        let dir = std::env::temp_dir().join("n42-test-legacy-version");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("consensus_state.json");

        let mut json_value: serde_json::Value = serde_json::to_value(genesis_snapshot(10)).unwrap();
        json_value.as_object_mut().unwrap().remove("version");
        std::fs::write(&path, serde_json::to_string_pretty(&json_value).unwrap()).unwrap();

        // A no-version snapshot is v1. Its safety state cannot be decoded by
        // the packed-bit format, so startup must require operator migration.
        assert!(load_consensus_state(&path).is_err());
        assert!(
            path.exists(),
            "failed recovery must preserve the operator's data"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_validate_valid_snapshot() {
        assert!(genesis_snapshot(10).validate().is_ok());
    }

    #[test]
    fn test_validate_allows_bounded_vote_watermarks_ahead_of_snapshot_view() {
        assert!(
            ConsensusSnapshot {
                last_voted_view: 11,
                ..genesis_snapshot(10)
            }
            .validate()
            .is_ok()
        );
        assert!(
            ConsensusSnapshot {
                last_commit_voted_view: 11,
                ..genesis_snapshot(10)
            }
            .validate()
            .is_ok()
        );
        assert!(
            ConsensusSnapshot {
                last_commit_voted_view: 139,
                ..genesis_snapshot(10)
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn test_validate_locked_qc_exceeds_current_view() {
        let mut locked = QuorumCertificate::genesis();
        locked.view = 20;
        let snapshot = ConsensusSnapshot {
            current_view: 10,
            locked_qc: locked,
            ..genesis_snapshot(10)
        };
        assert!(snapshot.validate().is_err());
    }

    #[test]
    fn test_validate_committed_exceeds_locked() {
        let mut locked = QuorumCertificate::genesis();
        locked.view = 5;
        let mut committed = QuorumCertificate::genesis();
        committed.view = 8;
        let snapshot = ConsensusSnapshot {
            current_view: 10,
            locked_qc: locked,
            last_committed_qc: committed,
            ..genesis_snapshot(10)
        };
        assert!(snapshot.validate().is_err());
    }

    #[test]
    fn test_load_invalid_snapshot_rejected() {
        // Persist a snapshot whose locked_qc.view > current_view (violates invariant).
        let dir = std::env::temp_dir().join("n42-test-invalid-snapshot");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("consensus_state.json");

        let mut locked = QuorumCertificate::genesis();
        locked.view = 99;
        let snapshot = ConsensusSnapshot {
            current_view: 10,
            locked_qc: locked,
            ..genesis_snapshot(10)
        };
        // Write without validation (bypass save_consensus_state sanity checks).
        let json = serde_json::to_string_pretty(&snapshot).unwrap();
        std::fs::write(&path, json).unwrap();

        let result = load_consensus_state(&path);
        assert!(
            result.is_err(),
            "load_consensus_state must reject a snapshot that fails validation"
        );
        let err = result.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("failed validation"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_invalid_snapshot_with_bad_staged_epoch_rejected() {
        let dir = std::env::temp_dir().join("n42-test-invalid-staged-epoch");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("consensus_state.json");

        let snapshot = ConsensusSnapshot {
            scheduled_epoch_transition: Some((1, vec![], 1)),
            ..genesis_snapshot(10)
        };
        std::fs::write(&path, serde_json::to_string_pretty(&snapshot).unwrap()).unwrap();

        let result = load_consensus_state(&path);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidData);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_invalid_snapshot_must_fail_startup_closed() {
        let dir = std::env::temp_dir().join("n42-test-invalid-snapshot-startup-fallback");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("consensus_state.json");

        let mut locked = QuorumCertificate::genesis();
        locked.view = 99;
        let snapshot = ConsensusSnapshot {
            current_view: 10,
            locked_qc: locked,
            ..genesis_snapshot(10)
        };
        std::fs::write(&path, serde_json::to_string_pretty(&snapshot).unwrap()).unwrap();

        // The node propagates this error and refuses to restart without its
        // safety state; falling back to a fresh engine could double-sign.
        let startup_snapshot = load_consensus_state(&path);
        assert!(startup_snapshot.is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_validate_all_views_equal() {
        let mut locked = QuorumCertificate::genesis();
        locked.view = 5;
        let mut committed = QuorumCertificate::genesis();
        committed.view = 5;
        let snapshot = ConsensusSnapshot {
            current_view: 5,
            locked_qc: locked,
            last_committed_qc: committed,
            ..genesis_snapshot(5)
        };
        assert!(snapshot.validate().is_ok());
    }

    #[test]
    fn test_save_and_load_with_epoch_transition() {
        use alloy_primitives::Address;
        use n42_primitives::BlsSecretKey;

        fn test_key(seed: u8) -> BlsSecretKey {
            BlsSecretKey::key_gen(&[seed; 32]).expect("deterministic test key should be valid")
        }

        let dir = std::env::temp_dir().join("n42-test-epoch-transition");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("consensus_state.json");

        let validators: Vec<_> = (0..4u8)
            .map(|i| {
                let sk = test_key(0x50 + i);
                ValidatorInfo {
                    address: Address::with_last_byte(42 + i),
                    bls_public_key: sk.public_key(),
                    p2p_peer_id: None,
                }
            })
            .collect();

        let snapshot = ConsensusSnapshot {
            consecutive_timeouts: 2,
            scheduled_epoch_transition: Some((3, validators.clone(), 1)),
            ..genesis_snapshot(100)
        };

        save_consensus_state(&path, &snapshot).unwrap();
        let loaded = load_consensus_state(&path).unwrap().unwrap();

        assert_eq!(loaded.current_view, 100);
        let (epoch, validators, ft) = loaded.scheduled_epoch_transition.unwrap();
        assert_eq!(epoch, 3);
        assert_eq!(validators.len(), 4);
        assert_eq!(validators[0].address, Address::with_last_byte(42));
        assert_eq!(ft, 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_consecutive_save_load_cycles() {
        let dir = std::env::temp_dir().join("n42-test-consecutive-saves");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("consensus_state.json");

        for view in [1u64, 10, 100, 1000] {
            save_consensus_state(&path, &genesis_snapshot(view)).unwrap();
        }

        let loaded = load_consensus_state(&path).unwrap().unwrap();
        assert_eq!(loaded.current_view, 1000);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_legacy_snapshot_without_epoch_transition_fails_closed() {
        let dir = std::env::temp_dir().join("n42-test-legacy-no-epoch");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("consensus_state.json");

        let snapshot = ConsensusSnapshot {
            consecutive_timeouts: 1,
            ..genesis_snapshot(50)
        };
        let mut json_value: serde_json::Value = serde_json::to_value(&snapshot).unwrap();
        let obj = json_value.as_object_mut().unwrap();
        obj.remove("version");
        obj.remove("scheduled_epoch_transition");
        std::fs::write(&path, serde_json::to_string_pretty(&json_value).unwrap()).unwrap();

        assert!(load_consensus_state(&path).is_err());
        assert!(path.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_snapshot_v4_execution_validated_head_round_trip() {
        let dir = std::env::temp_dir().join("n42-test-v4-validated-head");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("consensus_state.json");

        let snapshot = ConsensusSnapshot {
            execution_validated_head_view: 4242,
            execution_validated_head_hash: B256::repeat_byte(0x7E),
            ..genesis_snapshot(5000)
        };
        save_consensus_state(&path, &snapshot).unwrap();

        let loaded = load_consensus_state(&path).unwrap().unwrap();
        assert_eq!(loaded.version, SNAPSHOT_VERSION);
        assert_eq!(loaded.execution_validated_head_view, 4242);
        assert_eq!(
            loaded.execution_validated_head_hash,
            B256::repeat_byte(0x7E)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn restart_head_view_requires_an_exact_hash_mapping() {
        let canonical = B256::repeat_byte(0xA1);
        let stale = B256::repeat_byte(0xB2);
        let mut snapshot = genesis_snapshot(105);
        snapshot.committed_block_count = 100;
        snapshot.last_committed_qc.view = 105;
        snapshot.last_committed_qc.block_hash = canonical;
        snapshot.execution_validated_head_view = 104;
        snapshot.execution_validated_head_hash = stale;

        let recovered = recover_execution_validated_head_view(&snapshot, canonical, 100, None)
            .expect("the matching committed QC proves the canonical view");
        assert_eq!(recovered, (105, "commit-qc-exact"));

        snapshot.last_committed_qc.block_hash = B256::repeat_byte(0xC3);
        let error = recover_execution_validated_head_view(&snapshot, canonical, 100, None)
            .expect_err("a mismatched hash/view pair must fail closed");
        assert!(error.contains("cannot map canonical head"));
    }

    #[test]
    fn restart_head_view_can_use_matching_evidence() {
        let canonical = B256::repeat_byte(0xA1);
        let mut snapshot = genesis_snapshot(105);
        snapshot.committed_block_count = 100;
        snapshot.execution_validated_head_view = 90;
        snapshot.execution_validated_head_hash = B256::repeat_byte(0xB2);

        assert_eq!(
            recover_execution_validated_head_view(
                &snapshot,
                canonical,
                101,
                Some((107, canonical)),
            )
            .unwrap(),
            (107, "evidence-exact")
        );
        assert!(
            recover_execution_validated_head_view(
                &snapshot,
                canonical,
                101,
                Some((107, B256::repeat_byte(0xD4))),
            )
            .is_err(),
            "evidence for another hash must not seed the guard"
        );
    }

    #[test]
    fn test_load_v3_snapshot_defaults_execution_validated_head() {
        let dir = std::env::temp_dir().join("n42-test-v3-to-v4-upgrade");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("consensus_state.json");

        // A v3 snapshot: valid, versioned 3, missing the v4 fields.
        let snapshot = genesis_snapshot(77);
        let mut json_value: serde_json::Value = serde_json::to_value(&snapshot).unwrap();
        let obj = json_value.as_object_mut().unwrap();
        obj.insert("version".into(), serde_json::json!(3));
        obj.remove("execution_validated_head_view");
        obj.remove("execution_validated_head_hash");
        std::fs::write(&path, serde_json::to_string_pretty(&json_value).unwrap()).unwrap();

        let loaded = load_consensus_state(&path).unwrap().unwrap();
        assert_eq!(loaded.version, 3, "a v3 snapshot loads unchanged");
        assert_eq!(
            loaded.execution_validated_head_view, 0,
            "missing v4 view field defaults to 0"
        );
        assert_eq!(
            loaded.execution_validated_head_hash,
            B256::ZERO,
            "missing v4 hash field defaults to zero"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_v4_snapshot_conservatively_recovers_r2_vote_view() {
        let dir = std::env::temp_dir().join("n42-test-v4-to-v5-upgrade");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("consensus_state.json");

        let snapshot = ConsensusSnapshot {
            last_voted_view: 76,
            ..genesis_snapshot(77)
        };
        let mut json_value = serde_json::to_value(&snapshot).unwrap();
        let obj = json_value.as_object_mut().unwrap();
        obj.insert("version".into(), serde_json::json!(4));
        obj.remove("last_commit_voted_view");
        std::fs::write(&path, serde_json::to_string_pretty(&json_value).unwrap()).unwrap();

        let loaded = load_consensus_state(&path).unwrap().unwrap();
        assert_eq!(loaded.version, 4);
        assert_eq!(loaded.last_voted_view, 76);
        assert_eq!(loaded.last_commit_voted_view, 76);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_v5_snapshot_missing_r2_vote_view_fails_closed() {
        let dir = std::env::temp_dir().join("n42-test-v5-missing-r2-view");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("consensus_state.json");

        let mut json_value = serde_json::to_value(genesis_snapshot(77)).unwrap();
        json_value
            .as_object_mut()
            .unwrap()
            .remove("last_commit_voted_view");
        std::fs::write(&path, serde_json::to_string_pretty(&json_value).unwrap()).unwrap();

        assert!(load_consensus_state(&path).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_save_and_load_authorized_verifiers() {
        let dir = std::env::temp_dir().join("n42-test-authorized-verifiers");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("consensus_state.json");

        let pubkey1 = [0x11u8; 48];
        let pubkey2 = [0x22u8; 48];
        let snapshot = ConsensusSnapshot {
            authorized_verifiers: vec![pubkey1, pubkey2],
            ..genesis_snapshot(55)
        };
        save_consensus_state(&path, &snapshot).unwrap();

        let loaded = load_consensus_state(&path).unwrap().unwrap();
        assert_eq!(loaded.authorized_verifiers.len(), 2);
        assert!(loaded.authorized_verifiers.contains(&pubkey1));
        assert!(loaded.authorized_verifiers.contains(&pubkey2));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_save_and_load_empty_authorized_verifiers() {
        let dir = std::env::temp_dir().join("n42-test-empty-verifiers");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("consensus_state.json");

        let snapshot = ConsensusSnapshot {
            authorized_verifiers: vec![],
            ..genesis_snapshot(60)
        };
        save_consensus_state(&path, &snapshot).unwrap();

        let loaded = load_consensus_state(&path).unwrap().unwrap();
        assert!(
            loaded.authorized_verifiers.is_empty(),
            "empty vec should round-trip correctly"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_invalid_hex_pubkey() {
        let dir = std::env::temp_dir().join("n42-test-invalid-hex-pubkey");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("consensus_state.json");

        // Build valid JSON with an invalid hex string in authorized_verifiers.
        let snapshot = genesis_snapshot(80);
        let mut json_value: serde_json::Value = serde_json::to_value(&snapshot).unwrap();
        json_value["authorized_verifiers"] = serde_json::json!(["not_valid_hex!!"]);
        std::fs::write(&path, serde_json::to_string_pretty(&json_value).unwrap()).unwrap();

        let result = load_consensus_state(&path);
        assert!(
            result.is_err(),
            "invalid hex in authorized_verifiers must cause a parse error"
        );
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidData);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_wrong_length_hex_pubkey() {
        let dir = std::env::temp_dir().join("n42-test-wrong-len-hex-pubkey");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("consensus_state.json");

        // Valid hex but only 32 bytes (64 hex chars) instead of 48 bytes (96 hex chars).
        let short_hex = hex::encode([0xAAu8; 32]);
        let snapshot = genesis_snapshot(81);
        let mut json_value: serde_json::Value = serde_json::to_value(&snapshot).unwrap();
        json_value["authorized_verifiers"] = serde_json::json!([short_hex]);
        std::fs::write(&path, serde_json::to_string_pretty(&json_value).unwrap()).unwrap();

        let result = load_consensus_state(&path);
        assert!(
            result.is_err(),
            "wrong-length hex pubkey must cause a parse error"
        );
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidData);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_legacy_snapshot_without_authorized_verifiers() {
        let dir = std::env::temp_dir().join("n42-test-legacy-no-verifiers");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("consensus_state.json");

        // Write a snapshot JSON that omits `authorized_verifiers` (legacy format).
        let snapshot = genesis_snapshot(77);
        let mut json_value: serde_json::Value = serde_json::to_value(&snapshot).unwrap();
        json_value
            .as_object_mut()
            .unwrap()
            .remove("authorized_verifiers");
        std::fs::write(&path, serde_json::to_string_pretty(&json_value).unwrap()).unwrap();

        let loaded = load_consensus_state(&path).unwrap().unwrap();
        assert_eq!(loaded.current_view, 77);
        assert!(
            loaded.authorized_verifiers.is_empty(),
            "missing field should default to empty vec"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn execution_lineage_recovers_newest_exact_hash_view() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot_path = dir.path().join("consensus_state.json");
        let first = B256::repeat_byte(0x11);
        let second = B256::repeat_byte(0x22);

        append_execution_lineage_proof(&snapshot_path, 41, first).unwrap();
        append_execution_lineage_proof(&snapshot_path, 42, second).unwrap();
        append_execution_lineage_proof(&snapshot_path, 43, first).unwrap();

        assert_eq!(
            recover_execution_lineage_proof(&snapshot_path, first).unwrap(),
            Some((43, first))
        );
        assert_eq!(
            recover_execution_lineage_proof(&snapshot_path, second).unwrap(),
            Some((42, second))
        );
    }

    #[test]
    fn execution_lineage_tolerates_only_a_partial_final_record() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot_path = dir.path().join("consensus_state.json");
        let hash = B256::repeat_byte(0x33);
        append_execution_lineage_proof(&snapshot_path, 77, hash).unwrap();

        let path = execution_lineage_path(&snapshot_path);
        let mut file = OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(b"N42Epartial").unwrap();
        file.sync_data().unwrap();

        assert_eq!(
            recover_execution_lineage_proof(&snapshot_path, hash).unwrap(),
            Some((77, hash))
        );
    }

    #[test]
    fn execution_lineage_rejects_complete_record_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot_path = dir.path().join("consensus_state.json");
        let hash = B256::repeat_byte(0x44);
        append_execution_lineage_proof(&snapshot_path, 88, hash).unwrap();

        let path = execution_lineage_path(&snapshot_path);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[20] ^= 0x01;
        std::fs::write(path, bytes).unwrap();

        assert_eq!(
            recover_execution_lineage_proof(&snapshot_path, hash)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }
}
