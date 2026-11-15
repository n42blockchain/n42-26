use super::set::ValidatorSet;
use n42_primitives::consensus::ViewNumber;

/// Leader selection strategy based on round-robin rotation.
///
/// The leader for view `v` is `v % n` where `n` is the validator set size.
/// This is deterministic and requires no communication—every honest node
/// can independently compute who the leader is for any view.
#[derive(Debug, Clone)]
pub struct LeaderSelector;

/// Consecutive views one validator leads before the role moves on.
///
/// This is a rule of the chain: every validator must run with the same
/// value, or they disagree about who may propose. The default of 1 is the
/// rotation every existing network uses. `N42_LEADER_TENURE` raises it for
/// the measurement route, where a leader that keeps the role can prepare its
/// next block while the current one is being voted on; a network that adopts
/// it has to carry the value in its genesis instead of the environment.
pub fn leader_tenure() -> u64 {
    static TENURE: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *TENURE.get_or_init(|| {
        std::env::var("N42_LEADER_TENURE")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|tenure| *tenure > 0)
            .unwrap_or(1)
    })
}

/// The leader of `view` among `validators` validators when each leads
/// `tenure` consecutive views. `validators` and `tenure` must not be zero.
pub fn leader_index(view: ViewNumber, validators: u64, tenure: u64) -> u32 {
    ((view / tenure) % validators) as u32
}

impl LeaderSelector {
    /// Returns the leader's validator index for the given view.
    ///
    /// # Panics
    /// Panics if the validator set is empty. Returning a synthetic validator
    /// index would let a malformed release configuration appear to have a
    /// leader and fail open.
    pub fn leader_for_view(view: ViewNumber, validator_set: &ValidatorSet) -> u32 {
        assert!(
            !validator_set.is_empty(),
            "leader_for_view called with empty validator set"
        );
        leader_index(view, validator_set.len() as u64, leader_tenure())
    }

    /// Checks if the given validator is the leader for the given view.
    pub fn is_leader(validator_index: u32, view: ViewNumber, validator_set: &ValidatorSet) -> bool {
        Self::leader_for_view(view, validator_set) == validator_index
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tenure_keeps_the_leader_for_that_many_views_then_rotates() {
        assert_eq!(
            (0..8)
                .map(|view| leader_index(view, 3, 1))
                .collect::<Vec<_>>(),
            [0, 1, 2, 0, 1, 2, 0, 1]
        );
        assert_eq!(
            (0..10)
                .map(|view| leader_index(view, 3, 4))
                .collect::<Vec<_>>(),
            [0, 0, 0, 0, 1, 1, 1, 1, 2, 2]
        );
        assert_eq!(leader_index(4 * 3, 3, 4), 0);
    }
    use alloy_primitives::Address;
    use n42_chainspec::ValidatorInfo;
    use n42_primitives::BlsSecretKey;

    fn test_key(seed: u8) -> BlsSecretKey {
        BlsSecretKey::key_gen(&[seed; 32]).expect("deterministic test key should be valid")
    }

    /// Helper: create a ValidatorSet with `n` deterministic validators.
    fn make_validator_set(n: usize) -> ValidatorSet {
        let infos: Vec<_> = (0..n)
            .map(|i| {
                let sk = test_key(0x30 + i as u8);
                ValidatorInfo {
                    address: Address::with_last_byte(i as u8),
                    bls_public_key: sk.public_key(),
                    p2p_peer_id: None,
                }
            })
            .collect();
        let f = ((n as u32).saturating_sub(1)) / 3;
        ValidatorSet::new(&infos, f)
    }

    #[test]
    fn test_round_robin() {
        let vs = make_validator_set(4);

        let expected = [0u32, 1, 2, 3, 0, 1, 2, 3];
        for (view, &expected_leader) in expected.iter().enumerate() {
            let leader = LeaderSelector::leader_for_view(view as u64, &vs);
            assert_eq!(leader, expected_leader);
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
    #[should_panic(expected = "leader_for_view called with empty validator set")]
    fn test_leader_for_view_empty_set() {
        let vs = ValidatorSet::new(&[], 0);
        let _leader = LeaderSelector::leader_for_view(5, &vs);
    }

    #[test]
    fn test_leader_for_view_single_validator() {
        let vs = make_validator_set(1);
        for view in 0..10u64 {
            let leader = LeaderSelector::leader_for_view(view, &vs);
            assert_eq!(leader, 0);
        }
    }
}
