use std::collections::BTreeMap;
use super::set::ValidatorSet;
use n42_chainspec::ValidatorInfo;

/// Maximum number of historical epoch validator sets to retain.
/// Needed for verifying QCs from recent past epochs.
const MAX_HISTORICAL_EPOCHS: usize = 3;

/// Manages validator set transitions across epochs.
///
/// When `epoch_length = 0`, epochs are disabled and the manager acts as
/// a simple wrapper around a single ValidatorSet (backward compatible).
///
/// When `epoch_length > 0`, every `epoch_length` views constitute one epoch.
/// The manager tracks the current, next (staged), and historical validator sets
/// to support cross-epoch QC verification.
#[derive(Debug, Clone)]
pub struct EpochManager {
    /// Number of views per epoch (0 = epochs disabled).
    epoch_length: u64,
    /// Current epoch number (starts at 0).
    current_epoch: u64,
    /// Active validator set for the current epoch.
    current_set: ValidatorSet,
    /// Staged validator set for the next epoch (if any).
    next_set: Option<ValidatorSet>,
    /// Raw validator info for the staged set (kept for persistence).
    staged_info: Option<(Vec<ValidatorInfo>, u32)>,
    /// Historical validator sets keyed by epoch number (most recent MAX_HISTORICAL_EPOCHS).
    historical_sets: BTreeMap<u64, ValidatorSet>,
}

impl EpochManager {
    /// Creates an EpochManager with epochs disabled (single static validator set).
    pub fn new(validator_set: ValidatorSet) -> Self {
        Self {
            epoch_length: 0,
            current_epoch: 0,
            current_set: validator_set,
            next_set: None,
            staged_info: None,
            historical_sets: BTreeMap::new(),
        }
    }

    /// Creates an EpochManager with epoch transitions enabled.
    pub fn with_epoch_length(validator_set: ValidatorSet, epoch_length: u64) -> Self {
        Self {
            epoch_length,
            current_epoch: 0,
            current_set: validator_set,
            next_set: None,
            staged_info: None,
            historical_sets: BTreeMap::new(),
        }
    }

    /// Creates an EpochManager starting from a specific epoch.
    /// Used when a node starts from a snapshot at a non-genesis epoch.
    pub fn from_epoch(
        validator_set: ValidatorSet,
        epoch_length: u64,
        starting_epoch: u64,
    ) -> Self {
        Self {
            epoch_length,
            current_epoch: starting_epoch,
            current_set: validator_set,
            next_set: None,
            staged_info: None,
            historical_sets: BTreeMap::new(),
        }
    }

    /// Creates an EpochManager from a predefined schedule of validator sets.
    /// Each entry in the schedule is (epoch, validators, fault_tolerance).
    /// The last entry's validator set becomes the current set.
    pub fn from_schedule(
        epoch_length: u64,
        schedule: &[(u64, Vec<ValidatorInfo>, u32)],
    ) -> Self {
        assert!(!schedule.is_empty(), "epoch schedule must not be empty");

        let mut historical = BTreeMap::new();

        // Add all but the last entry to historical
        for (epoch, validators, f) in &schedule[..schedule.len() - 1] {
            let set = ValidatorSet::new(validators, *f);
            historical.insert(*epoch, set);
        }

        // Trim historical to MAX_HISTORICAL_EPOCHS
        while historical.len() > MAX_HISTORICAL_EPOCHS {
            if let Some(oldest) = historical.keys().next().copied() {
                historical.remove(&oldest);
            }
        }

        // Use the last entry as current
        let (current_epoch, validators, f) = &schedule[schedule.len() - 1];
        let current_set = ValidatorSet::new(validators, *f);

        Self {
            epoch_length,
            current_epoch: *current_epoch,
            current_set,
            next_set: None,
            staged_info: None,
            historical_sets: historical,
        }
    }

    /// Returns the epoch length (0 = disabled).
    pub fn epoch_length(&self) -> u64 {
        self.epoch_length
    }

    /// Returns the current epoch number.
    pub fn current_epoch(&self) -> u64 {
        self.current_epoch
    }

    /// Returns a reference to the current validator set.
    pub fn current_validator_set(&self) -> &ValidatorSet {
        &self.current_set
    }

    /// Returns whether epochs are enabled.
    pub fn epochs_enabled(&self) -> bool {
        self.epoch_length > 0
    }

    /// Computes which epoch a given view belongs to.
    /// Returns 0 when epochs are disabled.
    pub fn epoch_for_view(&self, view: u64) -> u64 {
        if self.epoch_length == 0 {
            0
        } else {
            // Views start at 1, so view 1..=epoch_length → epoch 0,
            // view (epoch_length+1)..=(2*epoch_length) → epoch 1, etc.
            view.saturating_sub(1) / self.epoch_length
        }
    }

    /// Checks if a given view is the first view of a new epoch.
    pub fn is_epoch_boundary(&self, view: u64) -> bool {
        if self.epoch_length == 0 || view <= 1 {
            return false;
        }
        (view.saturating_sub(1)) % self.epoch_length == 0
    }

    /// Returns the validator set that should be used for verifying a QC at the given view.
    /// Falls back to the current set if the historical set is unavailable.
    pub fn validator_set_for_view(&self, view: u64) -> &ValidatorSet {
        if self.epoch_length == 0 {
            return &self.current_set;
        }

        let epoch = self.epoch_for_view(view);

        if epoch == self.current_epoch {
            return &self.current_set;
        }

        // Look up historical set
        if let Some(set) = self.historical_sets.get(&epoch) {
            return set;
        }

        // Fallback to current set (best effort for very old QCs)
        &self.current_set
    }

    /// Stages a new validator set for the next epoch.
    /// The set will be activated when `advance_epoch()` is called.
    pub fn stage_next_epoch(&mut self, validators: &[ValidatorInfo], fault_tolerance: u32) {
        self.next_set = Some(ValidatorSet::new(validators, fault_tolerance));
        self.staged_info = Some((validators.to_vec(), fault_tolerance));
    }

    /// Returns the staged epoch transition info for persistence.
    /// Returns `(next_epoch_number, validators, fault_tolerance)`.
    pub fn staged_epoch_info(&self) -> Option<(u64, &[ValidatorInfo], u32)> {
        self.staged_info.as_ref().map(|(validators, f)| {
            (self.current_epoch + 1, validators.as_slice(), *f)
        })
    }

    /// Advances to the next epoch, activating the staged validator set.
    /// The current set moves to historical storage.
    /// Returns true if an epoch transition occurred, false if no staged set was available.
    pub fn advance_epoch(&mut self) -> bool {
        let Some(next_set) = self.next_set.take() else {
            return false;
        };

        self.staged_info = None;
        self.historical_sets.insert(self.current_epoch, self.current_set.clone());
        self.trim_historical();

        self.current_epoch += 1;
        self.current_set = next_set;

        tracing::info!(
            epoch = self.current_epoch,
            validators = self.current_set.len(),
            "epoch advanced to new validator set"
        );

        true
    }

    /// Returns the number of historical epoch sets stored.
    pub fn historical_epoch_count(&self) -> usize {
        self.historical_sets.len()
    }

    /// Returns whether a next epoch validator set has been staged.
    pub fn has_staged_next(&self) -> bool {
        self.next_set.is_some()
    }

    /// Trims historical sets to maintain MAX_HISTORICAL_EPOCHS limit.
    fn trim_historical(&mut self) {
        while self.historical_sets.len() > MAX_HISTORICAL_EPOCHS {
            if let Some(oldest) = self.historical_sets.keys().next().copied() {
                self.historical_sets.remove(&oldest);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Address;
    use n42_primitives::BlsSecretKey;

    fn make_validator_infos(count: usize) -> Vec<ValidatorInfo> {
        (0..count).map(|i| {
            let sk = BlsSecretKey::random().unwrap();
            ValidatorInfo {
                address: Address::with_last_byte(i as u8),
                bls_public_key: sk.public_key(),
            }
        }).collect()
    }

    #[test]
    fn test_epochs_disabled() {
        let infos = make_validator_infos(4);
        let vs = ValidatorSet::new(&infos, 1);
        let em = EpochManager::new(vs.clone());

        assert!(!em.epochs_enabled());
        assert_eq!(em.epoch_length(), 0);
        assert_eq!(em.current_epoch(), 0);
        assert_eq!(em.epoch_for_view(1), 0);
        assert_eq!(em.epoch_for_view(100), 0);
        assert!(!em.is_epoch_boundary(1));
        assert!(!em.is_epoch_boundary(100));
        assert_eq!(em.current_validator_set().len(), 4);
    }

    #[test]
    fn test_epoch_for_view() {
        let infos = make_validator_infos(4);
        let vs = ValidatorSet::new(&infos, 1);
        let em = EpochManager::with_epoch_length(vs, 10);

        assert!(em.epochs_enabled());
        assert_eq!(em.epoch_for_view(1), 0);  // views 1-10 → epoch 0
        assert_eq!(em.epoch_for_view(10), 0);
        assert_eq!(em.epoch_for_view(11), 1); // views 11-20 → epoch 1
        assert_eq!(em.epoch_for_view(20), 1);
        assert_eq!(em.epoch_for_view(21), 2); // views 21-30 → epoch 2
    }

    #[test]
    fn test_is_epoch_boundary() {
        let infos = make_validator_infos(4);
        let vs = ValidatorSet::new(&infos, 1);
        let em = EpochManager::with_epoch_length(vs, 10);

        assert!(!em.is_epoch_boundary(1));  // first view, not a boundary
        assert!(!em.is_epoch_boundary(10)); // last view of epoch 0
        assert!(em.is_epoch_boundary(11));  // first view of epoch 1
        assert!(!em.is_epoch_boundary(12));
        assert!(em.is_epoch_boundary(21));  // first view of epoch 2
    }

    #[test]
    fn test_advance_epoch() {
        let infos1 = make_validator_infos(4);
        let infos2 = make_validator_infos(5);
        let vs = ValidatorSet::new(&infos1, 1);
        let mut em = EpochManager::with_epoch_length(vs, 10);

        assert_eq!(em.current_epoch(), 0);
        assert_eq!(em.current_validator_set().len(), 4);

        // Stage next epoch
        em.stage_next_epoch(&infos2, 1);
        assert!(em.has_staged_next());

        // Advance
        assert!(em.advance_epoch());
        assert_eq!(em.current_epoch(), 1);
        assert_eq!(em.current_validator_set().len(), 5);
        assert!(!em.has_staged_next());
        assert_eq!(em.historical_epoch_count(), 1);
    }

    #[test]
    fn test_advance_epoch_no_staged() {
        let infos = make_validator_infos(4);
        let vs = ValidatorSet::new(&infos, 1);
        let mut em = EpochManager::with_epoch_length(vs, 10);

        // No staged set → advance fails
        assert!(!em.advance_epoch());
        assert_eq!(em.current_epoch(), 0);
    }

    #[test]
    fn test_validator_set_for_view() {
        let infos1 = make_validator_infos(4);
        let infos2 = make_validator_infos(5);
        let vs = ValidatorSet::new(&infos1, 1);
        let mut em = EpochManager::with_epoch_length(vs, 10);

        em.stage_next_epoch(&infos2, 1);
        em.advance_epoch();

        // Current epoch (1) should use new set
        assert_eq!(em.validator_set_for_view(11).len(), 5);

        // Historical epoch (0) should use old set
        assert_eq!(em.validator_set_for_view(5).len(), 4);
    }

    #[test]
    fn test_historical_limit() {
        let infos = make_validator_infos(4);
        let vs = ValidatorSet::new(&infos, 1);
        let mut em = EpochManager::with_epoch_length(vs, 10);

        // Advance through 5 epochs
        for i in 0..5 {
            let new_infos = make_validator_infos(4 + i);
            em.stage_next_epoch(&new_infos, 1);
            em.advance_epoch();
        }

        // Should only keep MAX_HISTORICAL_EPOCHS (3)
        assert!(em.historical_epoch_count() <= MAX_HISTORICAL_EPOCHS);
        assert_eq!(em.current_epoch(), 5);
    }

    #[test]
    fn test_from_epoch() {
        let infos = make_validator_infos(4);
        let vs = ValidatorSet::new(&infos, 1);
        let em = EpochManager::from_epoch(vs, 10, 5);

        assert_eq!(em.current_epoch(), 5);
        assert_eq!(em.epoch_length(), 10);
        assert_eq!(em.current_validator_set().len(), 4);
    }

    #[test]
    fn test_from_schedule() {
        let infos1 = make_validator_infos(4);
        let infos2 = make_validator_infos(5);
        let infos3 = make_validator_infos(6);

        let schedule = vec![
            (0, infos1.clone(), 1),
            (1, infos2.clone(), 1),
            (2, infos3.clone(), 1),
        ];

        let em = EpochManager::from_schedule(10, &schedule);

        assert_eq!(em.current_epoch(), 2);
        assert_eq!(em.current_validator_set().len(), 6);
        // epochs 0 and 1 should be in history
        assert_eq!(em.historical_epoch_count(), 2);
        assert_eq!(em.validator_set_for_view(5).len(), 4);  // epoch 0
        assert_eq!(em.validator_set_for_view(15).len(), 5); // epoch 1
        assert_eq!(em.validator_set_for_view(25).len(), 6); // epoch 2 (current)
    }
}
