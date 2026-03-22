use super::set::ValidatorSet;
use crate::error::{ConsensusError, ConsensusResult};
use alloy_primitives::Address;
use n42_chainspec::ValidatorInfo;
use std::collections::BTreeMap;

/// Maximum number of historical epoch validator sets to retain.
/// Needed for verifying QCs from recent past epochs.
const MAX_HISTORICAL_EPOCHS: usize = 3;

/// Minimum number of validators required for BFT safety (f ≥ 1).
pub const MIN_VALIDATOR_COUNT: usize = 4;

/// Manages validator set transitions across epochs.
///
/// When `epoch_length = 0`, epochs are disabled and the manager acts as
/// a simple wrapper around a single ValidatorSet (backward compatible).
///
/// When `epoch_length > 0`, every `epoch_length` views constitute one epoch.
/// The manager tracks the current, next (staged), and historical validator sets
/// to support cross-epoch QC verification.
///
/// Dynamic validator set changes follow the commit-then-activate protocol:
/// - `propose_add_validator` / `propose_remove_validator` queue changes in-memory.
/// - `commit_pending_changes` is called at CommitQC time; it validates safety
///   constraints and calls `stage_next_epoch` to schedule activation.
/// - `advance_epoch` activates the staged set at the next epoch boundary.
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
    /// Validators proposed for addition at the next CommitQC.
    pending_adds: Vec<ValidatorInfo>,
    /// Validator addresses proposed for removal at the next CommitQC.
    pending_removes: Vec<Address>,
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
            pending_adds: Vec::new(),
            pending_removes: Vec::new(),
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
            pending_adds: Vec::new(),
            pending_removes: Vec::new(),
        }
    }

    /// Creates an EpochManager starting from a specific epoch.
    /// Used when a node starts from a snapshot at a non-genesis epoch.
    pub fn from_epoch(validator_set: ValidatorSet, epoch_length: u64, starting_epoch: u64) -> Self {
        Self {
            epoch_length,
            current_epoch: starting_epoch,
            current_set: validator_set,
            next_set: None,
            staged_info: None,
            historical_sets: BTreeMap::new(),
            pending_adds: Vec::new(),
            pending_removes: Vec::new(),
        }
    }

    /// Creates an EpochManager from a predefined schedule of validator sets.
    /// Each entry in the schedule is (epoch, validators, fault_tolerance).
    /// The last entry's validator set becomes the current set.
    ///
    /// Returns `Err(ConsensusError::EpochScheduleEmpty)` if the schedule is empty.
    pub fn from_schedule(
        epoch_length: u64,
        schedule: &[(u64, Vec<ValidatorInfo>, u32)],
    ) -> ConsensusResult<Self> {
        if schedule.is_empty() {
            return Err(ConsensusError::EpochScheduleEmpty);
        }

        let mut historical = BTreeMap::new();

        // Add all but the last entry to historical
        for (epoch, validators, f) in &schedule[..schedule.len() - 1] {
            let set = ValidatorSet::try_new(validators, *f)?;
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
        let current_set = ValidatorSet::try_new(validators, *f)?;

        Ok(Self {
            epoch_length,
            current_epoch: *current_epoch,
            current_set,
            next_set: None,
            staged_info: None,
            historical_sets: historical,
            pending_adds: Vec::new(),
            pending_removes: Vec::new(),
        })
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
        (view.saturating_sub(1)).is_multiple_of(self.epoch_length)
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

        if epoch == self.current_epoch + 1
            && let Some(next_set) = self.next_set.as_ref()
        {
            return next_set;
        }

        // Look up historical set
        if let Some(set) = self.historical_sets.get(&epoch) {
            return set;
        }

        // Historical set unavailable (epoch too old, beyond MAX_HISTORICAL_EPOCHS).
        // Log a warning so operators can detect sync/verification issues.
        // Falling back to current_set is a best-effort measure; callers that need
        // strict correctness should treat this as an error.
        tracing::warn!(
            target: "n42::cl::epoch",
            view,
            epoch,
            current_epoch = self.current_epoch,
            max_historical = MAX_HISTORICAL_EPOCHS,
            "validator set for epoch not in history (too old); falling back to current set — \
             QC verification for this view may be incorrect"
        );
        &self.current_set
    }

    /// Stages a new validator set for the next epoch.
    /// The set will be activated when `advance_epoch()` is called.
    pub fn stage_next_epoch(
        &mut self,
        validators: &[ValidatorInfo],
        fault_tolerance: u32,
    ) -> ConsensusResult<()> {
        self.next_set = Some(ValidatorSet::try_new(validators, fault_tolerance)?);
        self.staged_info = Some((validators.to_vec(), fault_tolerance));
        Ok(())
    }

    /// Returns the staged epoch transition info for persistence.
    /// Returns `(next_epoch_number, validators, fault_tolerance)`.
    pub fn staged_epoch_info(&self) -> Option<(u64, &[ValidatorInfo], u32)> {
        self.staged_info
            .as_ref()
            .map(|(validators, f)| (self.current_epoch + 1, validators.as_slice(), *f))
    }

    /// Advances to the next epoch, activating the staged validator set.
    /// The current set moves to historical storage.
    /// Returns true if an epoch transition occurred, false if no staged set was available.
    pub fn advance_epoch(&mut self) -> bool {
        let Some(next_set) = self.next_set.take() else {
            return false;
        };

        self.staged_info = None;
        self.historical_sets
            .insert(self.current_epoch, self.current_set.clone());
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

    // ── Commit-then-Activate: dynamic validator set changes ──────────────

    /// Proposes adding a new validator at the next CommitQC.
    ///
    /// Fails if the address already exists in the current set or pending additions.
    pub fn propose_add_validator(&mut self, info: ValidatorInfo) -> ConsensusResult<()> {
        let addr = info.address;

        // Check not in current set
        let current_infos = self.current_set.validator_infos();
        if current_infos.iter().any(|v| v.address == addr) {
            return Err(ConsensusError::ValidatorAlreadyExists { address: addr });
        }

        // Check not already pending add
        if self.pending_adds.iter().any(|v| v.address == addr) {
            return Err(ConsensusError::ValidatorAlreadyExists { address: addr });
        }

        self.pending_adds.push(info);
        tracing::info!(
            target: "n42::cl::epoch",
            %addr,
            "validator queued for addition at next CommitQC"
        );
        Ok(())
    }

    /// Proposes removing a validator at the next CommitQC.
    ///
    /// Fails if the address is not in the current set, if the resulting
    /// count would drop below MIN_VALIDATOR_COUNT, or if already pending removal.
    pub fn propose_remove_validator(&mut self, addr: Address) -> ConsensusResult<()> {
        // Must exist in current set
        let current_infos = self.current_set.validator_infos();
        if !current_infos.iter().any(|v| v.address == addr) {
            return Err(ConsensusError::ValidatorNotFound { address: addr });
        }

        // Already pending removal?
        if self.pending_removes.iter().any(|&a| a == addr) {
            return Err(ConsensusError::ValidatorAlreadyExists { address: addr });
        }

        // Check resulting size ≥ MIN_VALIDATOR_COUNT
        let future_count = current_infos.len() + self.pending_adds.len()
            - self.pending_removes.len()
            - 1; // this removal
        if future_count < MIN_VALIDATOR_COUNT {
            return Err(ConsensusError::InsufficientValidators {
                have: future_count,
                need: MIN_VALIDATOR_COUNT,
            });
        }

        self.pending_removes.push(addr);
        tracing::info!(
            target: "n42::cl::epoch",
            %addr,
            "validator queued for removal at next CommitQC"
        );
        Ok(())
    }

    /// Returns whether there are any pending validator changes.
    pub fn has_pending_changes(&self) -> bool {
        !self.pending_adds.is_empty() || !self.pending_removes.is_empty()
    }

    /// Validates and commits pending changes by staging the new validator set.
    ///
    /// Called at CommitQC time. Validates safety constraints (minimum count,
    /// quorum overlap), then calls `stage_next_epoch`. Clears the pending queues.
    pub fn commit_pending_changes(&mut self) -> ConsensusResult<()> {
        if !self.has_pending_changes() {
            return Ok(());
        }

        let current_infos = self.current_set.validator_infos();
        let remove_set: std::collections::HashSet<Address> =
            self.pending_removes.iter().copied().collect();

        // Build new validator list: current - removes + adds, sorted by address.
        let mut new_validators: Vec<ValidatorInfo> = current_infos
            .into_iter()
            .filter(|v| !remove_set.contains(&v.address))
            .chain(self.pending_adds.iter().cloned())
            .collect();
        new_validators.sort_by_key(|v| v.address);

        // Validate transition safety.
        self.validate_transition(&new_validators)?;

        // Derive new f = (n - 1) / 3.
        let new_f = ((new_validators.len() as u32).saturating_sub(1)) / 3;

        self.stage_next_epoch(&new_validators, new_f)?;

        tracing::info!(
            target: "n42::cl::epoch",
            adds = self.pending_adds.len(),
            removes = self.pending_removes.len(),
            new_count = new_validators.len(),
            new_f,
            "pending validator changes committed; staged for next epoch boundary"
        );

        self.pending_adds.clear();
        self.pending_removes.clear();

        Ok(())
    }

    /// Validates that a proposed new validator set satisfies safety invariants:
    /// - At least MIN_VALIDATOR_COUNT validators.
    /// - The intersection of current and new address sets is ≥ current quorum_size (2f+1),
    ///   ensuring the Jolteon quorum-overlap liveness property.
    fn validate_transition(&self, new_validators: &[ValidatorInfo]) -> ConsensusResult<()> {
        // Minimum size check.
        if new_validators.len() < MIN_VALIDATOR_COUNT {
            return Err(ConsensusError::InsufficientValidators {
                have: new_validators.len(),
                need: MIN_VALIDATOR_COUNT,
            });
        }

        // Quorum overlap check (Jolteon §4.3 liveness).
        let current_addrs: std::collections::HashSet<Address> = self
            .current_set
            .validator_infos()
            .into_iter()
            .map(|v| v.address)
            .collect();
        let new_addrs: std::collections::HashSet<Address> =
            new_validators.iter().map(|v| v.address).collect();
        let overlap = current_addrs.intersection(&new_addrs).count();
        let required = self.current_set.quorum_size(); // 2f+1
        if overlap < required {
            return Err(ConsensusError::InsufficientQuorumOverlap {
                have: overlap,
                need: required,
            });
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Address;
    use n42_primitives::BlsSecretKey;

    fn test_key(seed: u8) -> BlsSecretKey {
        BlsSecretKey::key_gen(&[seed; 32]).expect("deterministic test key should be valid")
    }

    fn make_validator_infos(count: usize) -> Vec<ValidatorInfo> {
        (0..count)
            .map(|i| {
                let sk = test_key(0x50 + i as u8);
                ValidatorInfo {
                    address: Address::with_last_byte(i as u8),
                    bls_public_key: sk.public_key(),
                    p2p_peer_id: None,
                }
            })
            .collect()
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
        assert_eq!(em.epoch_for_view(1), 0); // views 1-10 → epoch 0
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

        assert!(!em.is_epoch_boundary(1)); // first view, not a boundary
        assert!(!em.is_epoch_boundary(10)); // last view of epoch 0
        assert!(em.is_epoch_boundary(11)); // first view of epoch 1
        assert!(!em.is_epoch_boundary(12));
        assert!(em.is_epoch_boundary(21)); // first view of epoch 2
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
        em.stage_next_epoch(&infos2, 1).unwrap();
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

        em.stage_next_epoch(&infos2, 1).unwrap();
        em.advance_epoch();

        // Current epoch (1) should use new set
        assert_eq!(em.validator_set_for_view(11).len(), 5);

        // Historical epoch (0) should use old set
        assert_eq!(em.validator_set_for_view(5).len(), 4);
    }

    #[test]
    fn test_validator_set_for_view_uses_staged_next_epoch_before_advance() {
        let infos1 = make_validator_infos(4);
        let infos2 = make_validator_infos(5);
        let vs = ValidatorSet::new(&infos1, 1);
        let mut em = EpochManager::with_epoch_length(vs, 10);

        em.stage_next_epoch(&infos2, 1).unwrap();

        // View 11 is the first view of epoch 1; it should resolve to the staged set
        // even before advance_epoch() is called.
        assert_eq!(em.validator_set_for_view(11).len(), 5);
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
            em.stage_next_epoch(&new_infos, 1).unwrap();
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

        let em = EpochManager::from_schedule(10, &schedule).unwrap();

        assert_eq!(em.current_epoch(), 2);
        assert_eq!(em.current_validator_set().len(), 6);
        // epochs 0 and 1 should be in history
        assert_eq!(em.historical_epoch_count(), 2);
        assert_eq!(em.validator_set_for_view(5).len(), 4); // epoch 0
        assert_eq!(em.validator_set_for_view(15).len(), 5); // epoch 1
        assert_eq!(em.validator_set_for_view(25).len(), 6); // epoch 2 (current)
    }

    // ── Commit-then-Activate tests ───────────────────────────────────────

    #[test]
    fn test_propose_add_validator_success() {
        let infos = make_validator_infos(4);
        let vs = ValidatorSet::new(&infos, 1);
        let mut em = EpochManager::with_epoch_length(vs, 10);

        // Address 0x10 is not in the set (make_validator_infos uses 0x00..0x03)
        let sk = test_key(0x20);
        let new_validator = ValidatorInfo {
            address: Address::with_last_byte(0x10),
            bls_public_key: sk.public_key(),
            p2p_peer_id: None,
        };

        assert!(!em.has_pending_changes());
        em.propose_add_validator(new_validator).unwrap();
        assert!(em.has_pending_changes());
        assert_eq!(em.pending_adds.len(), 1);
    }

    #[test]
    fn test_propose_remove_validator_success() {
        let infos = make_validator_infos(5); // 5 validators so removal stays ≥ 4
        let vs = ValidatorSet::new(&infos, 1);
        let mut em = EpochManager::with_epoch_length(vs, 10);

        em.propose_remove_validator(Address::with_last_byte(0)).unwrap();
        assert!(em.has_pending_changes());
        assert_eq!(em.pending_removes.len(), 1);
    }

    #[test]
    fn test_reject_below_minimum_validators() {
        let infos = make_validator_infos(4); // exactly 4 = minimum
        let vs = ValidatorSet::new(&infos, 1);
        let mut em = EpochManager::with_epoch_length(vs, 10);

        let err = em.propose_remove_validator(Address::with_last_byte(0)).unwrap_err();
        assert!(
            matches!(err, ConsensusError::InsufficientValidators { have: 3, need: 4 }),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_reject_duplicate_add() {
        let infos = make_validator_infos(4);
        let vs = ValidatorSet::new(&infos, 1);
        let mut em = EpochManager::with_epoch_length(vs, 10);

        // Address 0x00 already exists in current set
        let sk = test_key(0x50);
        let dup = ValidatorInfo {
            address: Address::with_last_byte(0),
            bls_public_key: sk.public_key(),
            p2p_peer_id: None,
        };
        let err = em.propose_add_validator(dup).unwrap_err();
        assert!(
            matches!(err, ConsensusError::ValidatorAlreadyExists { .. }),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_simultaneous_add_and_remove() {
        // Start with 5 validators (0x00..0x04), remove 0x00, add 0x10
        let infos = make_validator_infos(5);
        let vs = ValidatorSet::new(&infos, 1);
        let mut em = EpochManager::with_epoch_length(vs, 10);

        let sk = test_key(0x30);
        let new_v = ValidatorInfo {
            address: Address::with_last_byte(0x10),
            bls_public_key: sk.public_key(),
            p2p_peer_id: None,
        };
        em.propose_add_validator(new_v).unwrap();
        em.propose_remove_validator(Address::with_last_byte(0)).unwrap();

        em.commit_pending_changes().unwrap();

        assert!(!em.has_pending_changes());
        assert!(em.has_staged_next());

        // Advance epoch and verify new set
        em.advance_epoch();
        let new_infos = em.current_validator_set().validator_infos();
        assert_eq!(new_infos.len(), 5); // 5 - 1 + 1 = 5
        // 0x00 should be gone, 0x10 should be present
        assert!(!new_infos.iter().any(|v| v.address == Address::with_last_byte(0)));
        assert!(new_infos.iter().any(|v| v.address == Address::with_last_byte(0x10)));
    }

    #[test]
    fn test_validate_transition_quorum_overlap() {
        // 4 validators (f=1, quorum=3). New set must overlap ≥ 3 with current.
        let infos = make_validator_infos(4); // addresses 0x00..0x03
        let vs = ValidatorSet::new(&infos, 1);
        let em = EpochManager::with_epoch_length(vs, 10);

        // New set: keep only 0x00 (overlap=1 < 3) — should fail
        let sk0 = test_key(0x60);
        let sk1 = test_key(0x61);
        let sk2 = test_key(0x62);
        let completely_new: Vec<ValidatorInfo> = vec![
            ValidatorInfo { address: Address::with_last_byte(0),    bls_public_key: infos[0].bls_public_key.clone(), p2p_peer_id: None },
            ValidatorInfo { address: Address::with_last_byte(0x20), bls_public_key: sk0.public_key(), p2p_peer_id: None },
            ValidatorInfo { address: Address::with_last_byte(0x21), bls_public_key: sk1.public_key(), p2p_peer_id: None },
            ValidatorInfo { address: Address::with_last_byte(0x22), bls_public_key: sk2.public_key(), p2p_peer_id: None },
        ];
        let err = em.validate_transition(&completely_new).unwrap_err();
        assert!(
            matches!(err, ConsensusError::InsufficientQuorumOverlap { have: 1, need: 3 }),
            "unexpected error: {err}"
        );

        // New set: keep 0x00..0x02 (overlap=3 == 3) — should succeed
        let sk3 = test_key(0x63);
        let valid_new: Vec<ValidatorInfo> = vec![
            ValidatorInfo { address: Address::with_last_byte(0), bls_public_key: infos[0].bls_public_key.clone(), p2p_peer_id: None },
            ValidatorInfo { address: Address::with_last_byte(1), bls_public_key: infos[1].bls_public_key.clone(), p2p_peer_id: None },
            ValidatorInfo { address: Address::with_last_byte(2), bls_public_key: infos[2].bls_public_key.clone(), p2p_peer_id: None },
            ValidatorInfo { address: Address::with_last_byte(0x30), bls_public_key: sk3.public_key(), p2p_peer_id: None },
        ];
        em.validate_transition(&valid_new).unwrap();
    }

    #[test]
    fn test_deterministic_ordering() {
        // 5 validators; add one with address < existing addresses to verify sorting
        let infos = make_validator_infos(5); // 0x00..0x04
        let vs = ValidatorSet::new(&infos, 1);
        let mut em = EpochManager::with_epoch_length(vs, 10);

        // We'll add a validator with a high address and check it ends up at the end
        let sk = test_key(0x70);
        let high_addr_v = ValidatorInfo {
            address: Address::with_last_byte(0xFF),
            bls_public_key: sk.public_key(),
            p2p_peer_id: None,
        };
        em.propose_add_validator(high_addr_v).unwrap();
        em.commit_pending_changes().unwrap();
        em.advance_epoch();

        let new_infos = em.current_validator_set().validator_infos();
        // Verify ascending address order
        for w in new_infos.windows(2) {
            assert!(w[0].address <= w[1].address, "validators not sorted by address");
        }
        // Last element should be 0xFF
        assert_eq!(new_infos.last().unwrap().address, Address::with_last_byte(0xFF));
    }

    #[test]
    fn test_full_epoch_flow() {
        // 5 validators, epoch_length=10. Propose add, commit, advance at boundary.
        let infos = make_validator_infos(5);
        let vs = ValidatorSet::new(&infos, 1);
        let mut em = EpochManager::with_epoch_length(vs, 10);

        assert_eq!(em.current_validator_set().len(), 5);
        assert!(!em.has_pending_changes());

        // Stage a new validator
        let sk = test_key(0x80);
        let new_v = ValidatorInfo {
            address: Address::with_last_byte(0x50),
            bls_public_key: sk.public_key(),
            p2p_peer_id: None,
        };
        em.propose_add_validator(new_v).unwrap();
        assert!(em.has_pending_changes());

        // Simulate CommitQC: commit_pending_changes stages the new set
        em.commit_pending_changes().unwrap();
        assert!(!em.has_pending_changes());
        assert!(em.has_staged_next());
        // Current set unchanged yet
        assert_eq!(em.current_validator_set().len(), 5);

        // Simulate epoch boundary: advance_epoch activates the staged set
        assert!(em.advance_epoch());
        assert_eq!(em.current_epoch(), 1);
        assert_eq!(em.current_validator_set().len(), 6);
        assert!(!em.has_staged_next());
    }
}
