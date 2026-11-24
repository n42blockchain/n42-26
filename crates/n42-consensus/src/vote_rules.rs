//! The vote rules of the mixed production standard's Profile 1
//! (`MIXED_PRODUCTION_STANDARD_V1_RC.md` A13, A14, E6-E9, AM19, AM23), as one
//! switch. Ported from n42-rs `crates/n42/h2-consensus/src/vote_rules.rs`
//! (the rules this engine has; progress votes, the direct vote protocol and
//! the bench vote-before flags do not exist in n42-26).
//!
//! [`VoteRules::default`] is today's behaviour byte for byte.
//! [`VoteRules::PROFILE1`] closes these roads:
//!
//! | Rule | Today | Profile 1 |
//! |---|---|---|
//! | Signing profile | native, H2-v4 or gov5 legacy | H2-v4 (chain-bound) only; any other refuses every event |
//! | Round 1 | import-gated on the gov5 profiles, a block with no known parent passes the extends rule; optimistic on the native profile | only after this node executed the block (`BlockImportedWithParent`) and its parent is the justify QC's block; block data without its execution (`BlockImported`) is not evidence |
//! | CommitVote | on the PrepareQC | only after this node's own execution of the block |
//!
//! The rules are a chain constant: [`VoteRules::installed`] reads the profile
//! the node installed from its genesis (`n42_chainspec::profile1`).

/// See the [module documentation](self).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VoteRules {
    /// The chain runs the mixed production standard's Profile 1.
    pub profile1: bool,
}

impl VoteRules {
    /// Today's behaviour.
    pub const TODAY: Self = Self { profile1: false };

    /// Profile 1's rules.
    pub const PROFILE1: Self = Self { profile1: true };

    /// The rules of the profile installed in this process (today's when none
    /// is installed).
    pub fn installed() -> Self {
        match n42_chainspec::profile1::n42_profile() {
            Some(n42_chainspec::profile1::N42Profile::Profile1) => Self::PROFILE1,
            None => Self::TODAY,
        }
    }

    /// Round 1 waits for this node's execution of the block, whatever the
    /// signing profile.
    pub const fn round1_needs_execution(self) -> bool {
        self.profile1
    }

    /// The extends rule abstains on a block whose parent is not known here
    /// instead of passing it (AM19: the block's parent is the justify block).
    pub const fn unknown_parent_abstains(self) -> bool {
        self.profile1
    }

    /// The CommitVote waits for this node's own execution of the block.
    pub const fn commit_vote_needs_own_execution(self) -> bool {
        self.profile1
    }

    /// A14: whether `profile` may sign and verify consensus messages.
    pub const fn signing_profile_allowed(
        self,
        profile: crate::protocol::quorum::ConsensusSigningProfile,
    ) -> bool {
        !self.profile1
            || matches!(
                profile,
                crate::protocol::quorum::ConsensusSigningProfile::H2V4(_)
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::quorum::ConsensusSigningProfile;

    #[test]
    fn the_default_is_today_and_profile1_closes_every_road() {
        assert_eq!(VoteRules::default(), VoteRules::TODAY);
        let today = VoteRules::TODAY;
        assert!(!today.round1_needs_execution());
        assert!(!today.unknown_parent_abstains());
        assert!(!today.commit_vote_needs_own_execution());
        assert!(today.signing_profile_allowed(ConsensusSigningProfile::Native));
        assert!(today.signing_profile_allowed(ConsensusSigningProfile::Gov5Legacy));
        let p1 = VoteRules::PROFILE1;
        assert!(p1.round1_needs_execution());
        assert!(p1.unknown_parent_abstains());
        assert!(p1.commit_vote_needs_own_execution());
        assert!(!p1.signing_profile_allowed(ConsensusSigningProfile::Native));
        assert!(!p1.signing_profile_allowed(ConsensusSigningProfile::Gov5Legacy));
        assert!(p1.signing_profile_allowed(ConsensusSigningProfile::H2V4(
            n42_primitives::consensus::H2V4ChainIdentity {
                chain_id: 1144,
                genesis_hash: alloy_primitives::B256::ZERO,
            }
        )));
    }
}
