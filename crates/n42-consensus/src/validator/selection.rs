use n42_primitives::consensus::ViewNumber;
use super::set::ValidatorSet;

/// Leader selection strategy based on round-robin rotation.
///
/// The leader for view `v` is `v % n` where `n` is the validator set size.
/// This is deterministic and requires no communication—every honest node
/// can independently compute who the leader is for any view.
#[derive(Debug, Clone)]
pub struct LeaderSelector;

impl LeaderSelector {
    /// Returns the leader's validator index for the given view.
    pub fn leader_for_view(view: ViewNumber, validator_set: &ValidatorSet) -> u32 {
        if validator_set.is_empty() {
            return 0;
        }
        (view % validator_set.len() as u64) as u32
    }

    /// Checks if the given validator is the leader for the given view.
    pub fn is_leader(
        validator_index: u32,
        view: ViewNumber,
        validator_set: &ValidatorSet,
    ) -> bool {
        Self::leader_for_view(view, validator_set) == validator_index
    }
}
