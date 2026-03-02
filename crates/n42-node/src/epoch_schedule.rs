use n42_chainspec::ValidatorInfo;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// A single entry in the epoch schedule file.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct EpochEntry {
    /// The epoch number at which this validator set becomes active.
    start_epoch: u64,
    /// The validator set for this epoch (address + BLS public key).
    validators: Vec<ValidatorInfo>,
    /// Byzantine fault tolerance threshold: `2f + 1` votes needed.
    threshold: u32,
}

/// Epoch schedule loaded from `epoch_schedule.json`.
///
/// Maps future epoch numbers to their planned validator configurations.
/// The orchestrator consults this at each `EpochTransition` event to
/// pre-stage the next epoch's validator set via `EpochManager::stage_next_epoch()`,
/// enabling seamless validator set rotation without manual intervention.
///
/// # File Format
///
/// Place `epoch_schedule.json` in the same directory as the genesis file
/// (e.g. `$N42_DATA_DIR/epoch_schedule.json`).
///
/// ```json
/// [
///   {
///     "start_epoch": 2,
///     "validators": [
///       { "address": "0xabc...", "bls_public_key": "0x..." },
///       ...
///     ],
///     "threshold": 14
///   },
///   { "start_epoch": 3, "validators": [...], "threshold": 15 }
/// ]
/// ```
///
/// If the file is absent, epochs proceed without dynamic validator changes.
#[derive(Debug, Clone)]
pub struct EpochSchedule {
    entries: BTreeMap<u64, (Vec<ValidatorInfo>, u32)>,
}

impl EpochSchedule {
    /// Loads the epoch schedule from a JSON file.
    ///
    /// Returns `Ok(None)` if the file does not exist (epochs not configured).
    /// Returns `Err` if the file exists but is malformed.
    pub fn load(path: &Path) -> Result<Option<Self>, String> {
        match std::fs::read_to_string(path) {
            Ok(json) => {
                let entries: Vec<EpochEntry> = serde_json::from_str(&json)
                    .map_err(|e| format!("failed to parse {}: {e}", path.display()))?;

                let map: BTreeMap<u64, (Vec<ValidatorInfo>, u32)> = entries
                    .into_iter()
                    .map(|e| (e.start_epoch, (e.validators, e.threshold)))
                    .collect();

                tracing::info!(
                    epoch_count = map.len(),
                    path = %path.display(),
                    "loaded epoch schedule"
                );

                Ok(Some(Self { entries: map }))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("failed to read {}: {e}", path.display())),
        }
    }

    /// Returns the validator configuration planned for the given epoch, if any.
    pub fn get_for_epoch(&self, epoch: u64) -> Option<(&[ValidatorInfo], u32)> {
        self.entries
            .get(&epoch)
            .map(|(validators, threshold)| (validators.as_slice(), *threshold))
    }

    /// Returns the number of epoch entries in the schedule.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns true if the schedule has no entries.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Address;
    use n42_primitives::BlsSecretKey;
    use tempfile::TempDir;

    fn make_validators(count: usize) -> Vec<ValidatorInfo> {
        (0..count)
            .map(|i| {
                let sk = BlsSecretKey::random().unwrap();
                ValidatorInfo {
                    address: Address::with_last_byte(i as u8),
                    bls_public_key: sk.public_key(),
                }
            })
            .collect()
    }

    fn write_entries(entries: &[EpochEntry], dir: &Path) -> std::path::PathBuf {
        let path = dir.join("epoch_schedule.json");
        let json = serde_json::to_string_pretty(entries).unwrap();
        std::fs::write(&path, json).unwrap();
        path
    }

    #[test]
    fn test_load_nonexistent_returns_none() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nonexistent.json");
        assert!(EpochSchedule::load(&path).unwrap().is_none());
    }

    #[test]
    fn test_load_valid_schedule() {
        let dir = TempDir::new().unwrap();

        let entries = vec![
            EpochEntry { start_epoch: 2, validators: make_validators(5), threshold: 4 },
            EpochEntry { start_epoch: 3, validators: make_validators(7), threshold: 5 },
        ];
        let path = write_entries(&entries, dir.path());

        let schedule = EpochSchedule::load(&path).unwrap().unwrap();
        assert_eq!(schedule.len(), 2);
        assert!(schedule.get_for_epoch(2).is_some());
        assert!(schedule.get_for_epoch(3).is_some());
        assert!(schedule.get_for_epoch(4).is_none());
    }

    #[test]
    fn test_get_for_epoch_returns_correct_data() {
        let dir = TempDir::new().unwrap();

        let validators = make_validators(4);
        let entries = vec![EpochEntry { start_epoch: 5, validators: validators.clone(), threshold: 14 }];
        let path = write_entries(&entries, dir.path());

        let schedule = EpochSchedule::load(&path).unwrap().unwrap();
        let (loaded_validators, threshold) = schedule.get_for_epoch(5).unwrap();
        assert_eq!(loaded_validators.len(), 4);
        assert_eq!(threshold, 14);
    }

    #[test]
    fn test_load_empty_array_ok() {
        let dir = TempDir::new().unwrap();

        let path = dir.path().join("epoch_schedule.json");
        std::fs::write(&path, "[]").unwrap();

        let schedule = EpochSchedule::load(&path).unwrap().unwrap();
        assert!(schedule.is_empty());
    }

    #[test]
    fn test_load_malformed_returns_error() {
        let dir = TempDir::new().unwrap();

        let path = dir.path().join("epoch_schedule.json");
        std::fs::write(&path, "not valid json {{{").unwrap();

        assert!(EpochSchedule::load(&path).is_err());
    }

    #[test]
    fn test_get_for_epoch_missing_epoch_none() {
        let dir = TempDir::new().unwrap();

        let entries = vec![EpochEntry { start_epoch: 10, validators: make_validators(3), threshold: 2 }];
        let path = write_entries(&entries, dir.path());

        let schedule = EpochSchedule::load(&path).unwrap().unwrap();
        assert!(schedule.get_for_epoch(9).is_none());
        assert!(schedule.get_for_epoch(11).is_none());
        assert!(schedule.get_for_epoch(10).is_some());
    }
}
