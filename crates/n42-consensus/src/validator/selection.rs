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

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Address;
    use n42_chainspec::ValidatorInfo;
    use n42_primitives::BlsSecretKey;

    /// Helper: create a ValidatorSet with `n` random validators.
    fn make_validator_set(n: usize) -> ValidatorSet {
        let infos: Vec<_> = (0..n)
            .map(|i| {
                let sk = BlsSecretKey::random().unwrap();
                ValidatorInfo {
                    address: Address::with_last_byte(i as u8),
                    bls_public_key: sk.public_key(),
                }
            })
            .collect();
        let f = ((n as u32).saturating_sub(1)) / 3;
        ValidatorSet::new(&infos, f)
    }

    #[test]
    fn test_round_robin() {
        let vs = make_validator_set(4);

        // Views 0-7 should cycle through validators 0,1,2,3,0,1,2,3
        let expected = [0u32, 1, 2, 3, 0, 1, 2, 3];
        for (view, &expected_leader) in expected.iter().enumerate() {
            let leader = LeaderSelector::leader_for_view(view as u64, &vs);
            assert_eq!(
                leader, expected_leader,
                "view {} should select leader {}",
                view, expected_leader
            );
        }
    }

    #[test]
    fn test_is_leader() {
        let vs = make_validator_set(4);

        // View 0: validator 0 is leader
        assert!(LeaderSelector::is_leader(0, 0, &vs));
        assert!(!LeaderSelector::is_leader(1, 0, &vs));
        assert!(!LeaderSelector::is_leader(2, 0, &vs));
        assert!(!LeaderSelector::is_leader(3, 0, &vs));

        // View 1: validator 1 is leader
        assert!(!LeaderSelector::is_leader(0, 1, &vs));
        assert!(LeaderSelector::is_leader(1, 1, &vs));

        // View 5: validator 1 is leader (5 % 4 = 1)
        assert!(LeaderSelector::is_leader(1, 5, &vs));
        assert!(!LeaderSelector::is_leader(0, 5, &vs));
    }

    #[test]
    fn test_leader_for_view_empty_set() {
        let vs = ValidatorSet::new(&[], 0);
        // Empty set should return 0 (guard clause)
        let leader = LeaderSelector::leader_for_view(5, &vs);
        assert_eq!(leader, 0, "empty set should return leader index 0");
    }

    #[test]
    fn test_leader_for_view_single_validator() {
        let vs = make_validator_set(1);
        // Single validator is always the leader
        for view in 0..10u64 {
            let leader = LeaderSelector::leader_for_view(view, &vs);
            assert_eq!(leader, 0, "single validator should always be leader 0");
        }
    }
}
