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
///
/// Approximately 520 bytes when serialized as JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsensusSnapshot {
    pub current_view: u64,
    pub locked_qc: QuorumCertificate,
    pub last_committed_qc: QuorumCertificate,
    pub consecutive_timeouts: u32,
}

/// Atomically saves the consensus snapshot to a JSON file.
///
/// Uses the temp-file + rename pattern to prevent corruption from partial writes
/// (e.g., if the process crashes mid-write). On POSIX systems, `rename` is atomic
/// within the same filesystem.
pub fn save_consensus_state(path: &Path, snapshot: &ConsensusSnapshot) -> io::Result<()> {
    let json = serde_json::to_string_pretty(snapshot)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    let tmp_path = path.with_extension("json.tmp");

    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    std::fs::write(&tmp_path, json.as_bytes())?;
    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

/// Loads a consensus snapshot from a JSON file.
///
/// Returns `Ok(None)` if the file does not exist (fresh start).
/// Returns `Err` if the file exists but cannot be parsed (corruption).
pub fn load_consensus_state(path: &Path) -> io::Result<Option<ConsensusSnapshot>> {
    match std::fs::read_to_string(path) {
        Ok(json) => {
            let snapshot: ConsensusSnapshot = serde_json::from_str(&json)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
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

    #[test]
    fn test_save_and_load_snapshot() {
        let dir = std::env::temp_dir().join("n42-test-persistence");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("consensus_state.json");

        let snapshot = ConsensusSnapshot {
            current_view: 42,
            locked_qc: QuorumCertificate::genesis(),
            last_committed_qc: QuorumCertificate::genesis(),
            consecutive_timeouts: 3,
        };

        save_consensus_state(&path, &snapshot).expect("save should succeed");
        assert!(path.exists(), "file should exist after save");

        let loaded = load_consensus_state(&path)
            .expect("load should succeed")
            .expect("should return Some");

        assert_eq!(loaded.current_view, 42);
        assert_eq!(loaded.consecutive_timeouts, 3);
        assert_eq!(loaded.locked_qc.view, 0);
        assert_eq!(loaded.last_committed_qc.view, 0);

        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_nonexistent_returns_none() {
        let path = std::env::temp_dir().join("n42-test-nonexistent-state.json");
        let _ = std::fs::remove_file(&path);

        let result = load_consensus_state(&path).expect("should not error");
        assert!(result.is_none(), "should return None for missing file");
    }

    #[test]
    fn test_load_corrupt_returns_error() {
        let dir = std::env::temp_dir().join("n42-test-corrupt");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("consensus_state.json");

        std::fs::write(&path, "not valid json {{{").unwrap();

        let result = load_consensus_state(&path);
        assert!(result.is_err(), "should error on corrupt file");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_atomic_write_creates_parent_dirs() {
        let dir = std::env::temp_dir().join("n42-test-deep/nested/dirs");
        let _ = std::fs::remove_dir_all(std::env::temp_dir().join("n42-test-deep"));
        let path = dir.join("consensus_state.json");

        let snapshot = ConsensusSnapshot {
            current_view: 1,
            locked_qc: QuorumCertificate::genesis(),
            last_committed_qc: QuorumCertificate::genesis(),
            consecutive_timeouts: 0,
        };

        save_consensus_state(&path, &snapshot).expect("save should create parent dirs");
        assert!(path.exists());

        let _ = std::fs::remove_dir_all(std::env::temp_dir().join("n42-test-deep"));
    }
}
