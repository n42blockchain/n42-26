use n42_chainspec::ValidatorInfo;
use n42_primitives::consensus::QuorumCertificate;
use serde::{Deserialize, Serialize};
use std::io;
use std::path::Path;

/// Snapshot of consensus state persisted to disk.
///
/// Contains the minimum state needed to safely resume after a crash:
/// - `current_view`: prevents restarting from view 1
/// - `locked_qc`: preserves the locking constraint (safety rule)
/// - `last_committed_qc`: tracks the latest committed block
/// - `consecutive_timeouts`: maintains pacemaker backoff state
/// - `scheduled_epoch_transition`: preserves staged epoch changes across restarts
const SNAPSHOT_VERSION: u32 = 1;

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
        Ok(())
    }
}

/// Atomically saves the consensus snapshot to a JSON file.
///
/// Uses temp-file + rename to prevent corruption from partial writes.
/// On POSIX systems, `rename` is atomic within the same filesystem.
pub fn save_consensus_state(path: &Path, snapshot: &ConsensusSnapshot) -> io::Result<()> {
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
/// Returns `Err` if the file exists but cannot be parsed.
pub fn load_consensus_state(path: &Path) -> io::Result<Option<ConsensusSnapshot>> {
    match std::fs::read_to_string(path) {
        Ok(json) => {
            let snapshot: ConsensusSnapshot = serde_json::from_str(&json)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

            if snapshot.version > SNAPSHOT_VERSION {
                tracing::warn!(
                    snapshot_version = snapshot.version,
                    supported_version = SNAPSHOT_VERSION,
                    "consensus snapshot has newer version than supported; \
                     loading anyway but some fields may be ignored"
                );
            }

            if let Err(reason) = snapshot.validate() {
                tracing::warn!(reason, "consensus snapshot failed validation; loading anyway");
            }

            Ok(Some(snapshot))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use n42_primitives::consensus::QuorumCertificate;

    fn genesis_snapshot(current_view: u64) -> ConsensusSnapshot {
        ConsensusSnapshot {
            version: 1,
            current_view,
            locked_qc: QuorumCertificate::genesis(),
            last_committed_qc: QuorumCertificate::genesis(),
            consecutive_timeouts: 0,
            scheduled_epoch_transition: None,
        }
    }

    #[test]
    fn test_save_and_load_snapshot() {
        let dir = std::env::temp_dir().join("n42-test-persistence");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("consensus_state.json");

        let snapshot = ConsensusSnapshot { consecutive_timeouts: 3, ..genesis_snapshot(42) };
        save_consensus_state(&path, &snapshot).expect("save should succeed");
        assert!(path.exists());

        let loaded = load_consensus_state(&path).unwrap().unwrap();
        assert_eq!(loaded.version, 1);
        assert_eq!(loaded.current_view, 42);
        assert_eq!(loaded.consecutive_timeouts, 3);
        assert_eq!(loaded.locked_qc.view, 0);

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
    fn test_load_legacy_snapshot_without_version() {
        let dir = std::env::temp_dir().join("n42-test-legacy-version");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("consensus_state.json");

        let mut json_value: serde_json::Value =
            serde_json::to_value(&genesis_snapshot(10)).unwrap();
        json_value.as_object_mut().unwrap().remove("version");
        std::fs::write(&path, serde_json::to_string_pretty(&json_value).unwrap()).unwrap();

        let loaded = load_consensus_state(&path).unwrap().unwrap();
        assert_eq!(loaded.version, 1, "default version should be 1");
        assert_eq!(loaded.current_view, 10);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_validate_valid_snapshot() {
        assert!(genesis_snapshot(10).validate().is_ok());
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

        let dir = std::env::temp_dir().join("n42-test-epoch-transition");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("consensus_state.json");

        let sk = BlsSecretKey::random().expect("BLS key gen");
        let validator = ValidatorInfo {
            address: Address::with_last_byte(42),
            bls_public_key: sk.public_key(),
        };

        let snapshot = ConsensusSnapshot {
            consecutive_timeouts: 2,
            scheduled_epoch_transition: Some((3, vec![validator.clone()], 1)),
            ..genesis_snapshot(100)
        };

        save_consensus_state(&path, &snapshot).unwrap();
        let loaded = load_consensus_state(&path).unwrap().unwrap();

        assert_eq!(loaded.current_view, 100);
        let (epoch, validators, ft) = loaded.scheduled_epoch_transition.unwrap();
        assert_eq!(epoch, 3);
        assert_eq!(validators.len(), 1);
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
    fn test_load_legacy_snapshot_without_epoch_transition() {
        let dir = std::env::temp_dir().join("n42-test-legacy-no-epoch");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("consensus_state.json");

        let snapshot = ConsensusSnapshot { consecutive_timeouts: 1, ..genesis_snapshot(50) };
        let mut json_value: serde_json::Value = serde_json::to_value(&snapshot).unwrap();
        let obj = json_value.as_object_mut().unwrap();
        obj.remove("version");
        obj.remove("scheduled_epoch_transition");
        std::fs::write(&path, serde_json::to_string_pretty(&json_value).unwrap()).unwrap();

        let loaded = load_consensus_state(&path).unwrap().unwrap();
        assert_eq!(loaded.version, 1);
        assert_eq!(loaded.current_view, 50);
        assert!(loaded.scheduled_epoch_transition.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
