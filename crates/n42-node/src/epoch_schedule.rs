use n42_chainspec::ValidatorInfo;
use n42_consensus::ValidatorSet;
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
    /// Byzantine fault tolerance parameter `f`.
    ///
    /// The JSON field `threshold` is still accepted as a legacy alias.
    #[serde(alias = "threshold")]
    fault_tolerance: u32,
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
///     "fault_tolerance": 1
///   },
///   { "start_epoch": 3, "validators": [...], "fault_tolerance": 2 }
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

                // Validate entries before inserting.
                let mut map: BTreeMap<u64, (Vec<ValidatorInfo>, u32)> = BTreeMap::new();
                for entry in entries {
                    if entry.validators.is_empty() {
                        return Err(format!(
                            "epoch {} has empty validator set",
                            entry.start_epoch
                        ));
                    }
                    if let Err(reason) =
                        ValidatorSet::validate_params(entry.validators.len(), entry.fault_tolerance)
                    {
                        return Err(format!(
                            "epoch {} has invalid validator set parameters: {reason}",
                            entry.start_epoch
                        ));
                    }
                    let peer_id_presence = entry
                        .validators
                        .iter()
                        .filter(|validator| validator.p2p_peer_id.is_some())
                        .count();
                    if peer_id_presence != 0 && peer_id_presence != entry.validators.len() {
                        return Err(format!(
                            "epoch {} must either define p2p_peer_id for all validators or omit it for all",
                            entry.start_epoch
                        ));
                    }
                    let mut seen_peer_ids = std::collections::HashSet::new();
                    for validator in &entry.validators {
                        if let Some(peer_id) = validator
                            .parsed_p2p_peer_id()
                            .map_err(|error| format!("epoch {} {error}", entry.start_epoch))?
                            && !seen_peer_ids.insert(peer_id)
                        {
                            return Err(format!(
                                "epoch {} has duplicate validator p2p_peer_id: {peer_id}",
                                entry.start_epoch
                            ));
                        }
                    }
                    if map
                        .insert(entry.start_epoch, (entry.validators, entry.fault_tolerance))
                        .is_some()
                    {
                        return Err(format!(
                            "duplicate epoch entry for start_epoch {}",
                            entry.start_epoch
                        ));
                    }
                }

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
            .map(|(validators, fault_tolerance)| (validators.as_slice(), *fault_tolerance))
    }

    pub fn active_config_for_epoch<'a>(
        &'a self,
        epoch: u64,
        initial_validators: &'a [ValidatorInfo],
        initial_fault_tolerance: u32,
    ) -> (&'a [ValidatorInfo], u32) {
        self.entries
            .range(..=epoch)
            .next_back()
            .map(|(_, (validators, fault_tolerance))| (validators.as_slice(), *fault_tolerance))
            .unwrap_or((initial_validators, initial_fault_tolerance))
    }

    pub fn validate_peer_binding_policy(
        &self,
        allow_deterministic_fallback: bool,
    ) -> Result<(), String> {
        if allow_deterministic_fallback {
            return Ok(());
        }

        for (epoch, (validators, _)) in &self.entries {
            if validators.len() > 1
                && validators
                    .iter()
                    .all(|validator| validator.p2p_peer_id.is_none())
            {
                return Err(format!(
                    "epoch {epoch} is missing explicit p2p_peer_id bindings for a multi-validator set"
                ));
            }
        }

        Ok(())
    }

    /// Builds a recovery window for recent epochs ending at `current_epoch`.
    ///
    /// The returned entries are suitable for `EpochManager::from_schedule()`.
    pub fn recovery_window(
        &self,
        initial_validators: &[ValidatorInfo],
        initial_fault_tolerance: u32,
        current_epoch: u64,
        history_depth: usize,
    ) -> Vec<(u64, Vec<ValidatorInfo>, u32)> {
        let start_epoch = current_epoch.saturating_sub(history_depth as u64);
        (start_epoch..=current_epoch)
            .map(|epoch| {
                self.entries
                    .range(..=epoch)
                    .next_back()
                    .map(|(_, (validators, fault_tolerance))| {
                        (epoch, validators.clone(), *fault_tolerance)
                    })
                    .unwrap_or_else(|| {
                        (epoch, initial_validators.to_vec(), initial_fault_tolerance)
                    })
            })
            .collect()
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
                    p2p_peer_id: None,
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
            EpochEntry {
                start_epoch: 2,
                validators: make_validators(4),
                fault_tolerance: 1,
            },
            EpochEntry {
                start_epoch: 3,
                validators: make_validators(7),
                fault_tolerance: 2,
            },
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
        let entries = vec![EpochEntry {
            start_epoch: 5,
            validators: validators.clone(),
            fault_tolerance: 1,
        }];
        let path = write_entries(&entries, dir.path());

        let schedule = EpochSchedule::load(&path).unwrap().unwrap();
        let (loaded_validators, fault_tolerance) = schedule.get_for_epoch(5).unwrap();
        assert_eq!(loaded_validators.len(), 4);
        assert_eq!(fault_tolerance, 1);
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

        let entries = vec![EpochEntry {
            start_epoch: 10,
            validators: make_validators(4),
            fault_tolerance: 1,
        }];
        let path = write_entries(&entries, dir.path());

        let schedule = EpochSchedule::load(&path).unwrap().unwrap();
        assert!(schedule.get_for_epoch(9).is_none());
        assert!(schedule.get_for_epoch(11).is_none());
        assert!(schedule.get_for_epoch(10).is_some());
    }

    #[test]
    fn test_load_legacy_threshold_alias() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("epoch_schedule.json");
        std::fs::write(
            &path,
            r#"[{"start_epoch":2,"validators":[],"threshold":1}]"#,
        )
        .unwrap();

        let err = EpochSchedule::load(&path).unwrap_err();
        assert!(err.contains("empty validator set"));
    }

    #[test]
    fn test_load_rejects_invalid_fault_tolerance() {
        let dir = TempDir::new().unwrap();
        let entries = vec![EpochEntry {
            start_epoch: 2,
            validators: make_validators(4),
            fault_tolerance: 2,
        }];
        let path = write_entries(&entries, dir.path());

        let err = EpochSchedule::load(&path).unwrap_err();
        assert!(err.contains("invalid validator set parameters"));
    }

    #[test]
    fn test_recovery_window_tracks_active_set_per_epoch() {
        let dir = TempDir::new().unwrap();
        let initial = make_validators(4);
        let entries = vec![
            EpochEntry {
                start_epoch: 2,
                validators: make_validators(5),
                fault_tolerance: 1,
            },
            EpochEntry {
                start_epoch: 4,
                validators: make_validators(7),
                fault_tolerance: 2,
            },
        ];
        let path = write_entries(&entries, dir.path());

        let schedule = EpochSchedule::load(&path).unwrap().unwrap();
        let window = schedule.recovery_window(&initial, 1, 4, 3);

        assert_eq!(window.len(), 4);
        assert_eq!(window[0].0, 1);
        assert_eq!(window[0].1.len(), 4);
        assert_eq!(window[1].0, 2);
        assert_eq!(window[1].1.len(), 5);
        assert_eq!(window[2].0, 3);
        assert_eq!(window[2].1.len(), 5);
        assert_eq!(window[3].0, 4);
        assert_eq!(window[3].1.len(), 7);
        assert_eq!(window[3].2, 2);
    }

    #[test]
    fn test_validate_peer_binding_policy_rejects_strict_multi_validator_fallback() {
        let dir = TempDir::new().unwrap();
        let entries = vec![EpochEntry {
            start_epoch: 2,
            validators: make_validators(4),
            fault_tolerance: 1,
        }];
        let path = write_entries(&entries, dir.path());

        let schedule = EpochSchedule::load(&path).unwrap().unwrap();
        let err = schedule.validate_peer_binding_policy(false).unwrap_err();
        assert!(err.contains("missing explicit p2p_peer_id"));
    }

    #[test]
    fn test_active_config_for_epoch_tracks_latest_prior_entry() {
        let dir = TempDir::new().unwrap();
        let initial = make_validators(4);
        let entries = vec![
            EpochEntry {
                start_epoch: 2,
                validators: make_validators(5),
                fault_tolerance: 1,
            },
            EpochEntry {
                start_epoch: 4,
                validators: make_validators(7),
                fault_tolerance: 2,
            },
        ];
        let path = write_entries(&entries, dir.path());

        let schedule = EpochSchedule::load(&path).unwrap().unwrap();
        let (validators0, ft0) = schedule.active_config_for_epoch(0, &initial, 1);
        let (validators3, ft3) = schedule.active_config_for_epoch(3, &initial, 1);
        let (validators5, ft5) = schedule.active_config_for_epoch(5, &initial, 1);

        assert_eq!(validators0.len(), 4);
        assert_eq!(ft0, 1);
        assert_eq!(validators3.len(), 5);
        assert_eq!(ft3, 1);
        assert_eq!(validators5.len(), 7);
        assert_eq!(ft5, 2);
    }
}
