use n42_primitives::consensus::{QuorumCertificate, ViewNumber};

/// The phase within a consensus round (view).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Waiting for the leader to propose a block.
    WaitingForProposal,
    /// Leader has proposed; collecting Round 1 (Prepare) votes.
    Voting,
    /// QC formed from Round 1; collecting Round 2 (Commit) votes.
    PreCommit,
    /// Block is committed (terminal state for this view).
    Committed,
    /// View has timed out; collecting timeout messages for ViewChange.
    TimedOut,
}

/// Tracks the state of the current consensus round.
///
/// Each view progresses through phases:
/// `WaitingForProposal → Voting → PreCommit → Committed`
///
/// If a timeout occurs at any point, the phase transitions to `TimedOut`,
/// triggering the view change (Round 3) protocol.
#[derive(Debug, Clone)]
pub struct RoundState {
    /// Current view number (monotonically increasing).
    current_view: ViewNumber,
    /// Current phase within this view.
    phase: Phase,
    /// The highest QC this node has seen (used for locking).
    /// A node is "locked" on this QC and will only vote for proposals
    /// that extend it (safety rule).
    locked_qc: QuorumCertificate,
    /// The QC of the last committed block.
    last_committed_qc: QuorumCertificate,
    /// Number of consecutive timeouts (for exponential backoff).
    consecutive_timeouts: u32,
}

impl RoundState {
    /// Creates a new RoundState starting from genesis.
    pub fn new() -> Self {
        let genesis_qc = QuorumCertificate::genesis();
        Self {
            current_view: 1,
            phase: Phase::WaitingForProposal,
            locked_qc: genesis_qc.clone(),
            last_committed_qc: genesis_qc,
            consecutive_timeouts: 0,
        }
    }

    /// Returns the current view number.
    pub fn current_view(&self) -> ViewNumber {
        self.current_view
    }

    /// Returns the current phase.
    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// Returns a reference to the locked QC.
    pub fn locked_qc(&self) -> &QuorumCertificate {
        &self.locked_qc
    }

    /// Returns a reference to the last committed QC.
    pub fn last_committed_qc(&self) -> &QuorumCertificate {
        &self.last_committed_qc
    }

    /// Returns the number of consecutive timeouts.
    pub fn consecutive_timeouts(&self) -> u32 {
        self.consecutive_timeouts
    }

    /// Advances to the voting phase after receiving a valid proposal.
    pub fn enter_voting(&mut self) {
        self.phase = Phase::Voting;
    }

    /// Advances to the pre-commit phase after a QC is formed.
    pub fn enter_pre_commit(&mut self) {
        self.phase = Phase::PreCommit;
    }

    /// Marks the current view as committed.
    /// Updates the locked QC and last committed QC.
    pub fn commit(&mut self, commit_qc: QuorumCertificate) {
        self.phase = Phase::Committed;
        // Update locked QC if the commit QC is higher
        if commit_qc.view > self.locked_qc.view {
            self.locked_qc = commit_qc.clone();
        }
        self.last_committed_qc = commit_qc;
        self.consecutive_timeouts = 0;
    }

    /// Advances to the next view (after commit or view change).
    pub fn advance_view(&mut self, new_view: ViewNumber) {
        self.current_view = new_view;
        self.phase = Phase::WaitingForProposal;
    }

    /// Transitions to the timed-out phase.
    pub fn timeout(&mut self) {
        self.phase = Phase::TimedOut;
        self.consecutive_timeouts += 1;
    }

    /// Updates the locked QC if the new one is higher.
    /// Called when seeing a QC in a proposal's justify_qc.
    pub fn update_locked_qc(&mut self, qc: &QuorumCertificate) {
        if qc.view > self.locked_qc.view {
            self.locked_qc = qc.clone();
        }
    }

    /// Checks the HotStuff-2 safety rule:
    /// A proposal is safe to vote on if its justify_qc extends the locked QC
    /// (i.e., justify_qc.view >= locked_qc.view).
    pub fn is_safe_to_vote(&self, justify_qc: &QuorumCertificate) -> bool {
        justify_qc.view >= self.locked_qc.view
    }
}

impl Default for RoundState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::B256;
    use bitvec::prelude::*;
    use n42_primitives::BlsSecretKey;

    /// Helper: create a QuorumCertificate with the given view number.
    fn make_qc(view: ViewNumber) -> QuorumCertificate {
        let sk = BlsSecretKey::random().unwrap();
        let sig = sk.sign(b"test-qc");
        QuorumCertificate {
            view,
            block_hash: B256::repeat_byte(view as u8),
            aggregate_signature: sig,
            signers: bitvec![u8, Msb0; 1, 1, 0, 1],
        }
    }

    #[test]
    fn test_initial_state() {
        let state = RoundState::new();

        assert_eq!(state.current_view(), 1, "initial view should be 1");
        assert_eq!(state.phase(), Phase::WaitingForProposal, "initial phase should be WaitingForProposal");
        assert_eq!(state.consecutive_timeouts(), 0, "initial timeouts should be 0");
        assert_eq!(state.locked_qc().view, 0, "locked QC should be genesis (view 0)");
        assert_eq!(state.last_committed_qc().view, 0, "last committed QC should be genesis (view 0)");
    }

    #[test]
    fn test_phase_transitions() {
        let mut state = RoundState::new();

        // WaitingForProposal -> Voting
        assert_eq!(state.phase(), Phase::WaitingForProposal);
        state.enter_voting();
        assert_eq!(state.phase(), Phase::Voting, "should be in Voting phase");

        // Voting -> PreCommit
        state.enter_pre_commit();
        assert_eq!(state.phase(), Phase::PreCommit, "should be in PreCommit phase");

        // PreCommit -> Committed
        let commit_qc = make_qc(1);
        state.commit(commit_qc);
        assert_eq!(state.phase(), Phase::Committed, "should be in Committed phase");
    }

    #[test]
    fn test_timeout() {
        let mut state = RoundState::new();

        assert_eq!(state.consecutive_timeouts(), 0);

        state.timeout();
        assert_eq!(state.phase(), Phase::TimedOut, "phase should be TimedOut");
        assert_eq!(state.consecutive_timeouts(), 1, "consecutive_timeouts should be 1");

        state.timeout();
        assert_eq!(state.consecutive_timeouts(), 2, "consecutive_timeouts should be 2");

        state.timeout();
        assert_eq!(state.consecutive_timeouts(), 3, "consecutive_timeouts should be 3");
    }

    #[test]
    fn test_advance_view() {
        let mut state = RoundState::new();

        // Enter voting, then timeout
        state.enter_voting();
        state.timeout();
        assert_eq!(state.phase(), Phase::TimedOut);

        // Advance to view 2
        state.advance_view(2);
        assert_eq!(state.current_view(), 2, "view should be 2");
        assert_eq!(
            state.phase(),
            Phase::WaitingForProposal,
            "phase should reset to WaitingForProposal"
        );

        // Advance to view 10
        state.advance_view(10);
        assert_eq!(state.current_view(), 10, "view should jump to 10");
        assert_eq!(state.phase(), Phase::WaitingForProposal);
    }

    #[test]
    fn test_safety_rule() {
        let mut state = RoundState::new();

        // Initially locked on genesis QC (view 0)
        let qc_view_0 = QuorumCertificate::genesis();
        assert!(
            state.is_safe_to_vote(&qc_view_0),
            "genesis QC (view 0) should be safe when locked on view 0"
        );

        // Update locked QC to view 5
        let qc_view_5 = make_qc(5);
        state.update_locked_qc(&qc_view_5);
        assert_eq!(state.locked_qc().view, 5, "locked QC should now be at view 5");

        // A justify_qc at view 5 (equal) should be safe
        let justify_equal = make_qc(5);
        assert!(
            state.is_safe_to_vote(&justify_equal),
            "justify_qc at same view as locked should be safe"
        );

        // A justify_qc at view 7 (higher) should be safe
        let justify_higher = make_qc(7);
        assert!(
            state.is_safe_to_vote(&justify_higher),
            "justify_qc with higher view should be safe"
        );

        // A justify_qc at view 3 (lower) should NOT be safe
        let justify_lower = make_qc(3);
        assert!(
            !state.is_safe_to_vote(&justify_lower),
            "justify_qc with lower view should NOT be safe"
        );
    }

    #[test]
    fn test_commit_resets_consecutive_timeouts() {
        let mut state = RoundState::new();

        // Accumulate some timeouts
        state.timeout();
        state.timeout();
        assert_eq!(state.consecutive_timeouts(), 2);

        // Commit resets the counter
        state.advance_view(2);
        state.enter_voting();
        state.enter_pre_commit();
        let qc = make_qc(2);
        state.commit(qc);
        assert_eq!(
            state.consecutive_timeouts(),
            0,
            "consecutive_timeouts should reset to 0 after commit"
        );
    }

    #[test]
    fn test_commit_updates_locked_qc() {
        let mut state = RoundState::new();

        // Commit with view 5 QC
        let qc5 = make_qc(5);
        state.commit(qc5.clone());
        assert_eq!(state.locked_qc().view, 5, "locked QC should update to view 5");
        assert_eq!(
            state.last_committed_qc().view,
            5,
            "last committed QC should be view 5"
        );

        // Commit with view 3 QC (lower) - locked QC should NOT downgrade
        let qc3 = make_qc(3);
        state.commit(qc3);
        assert_eq!(
            state.locked_qc().view,
            5,
            "locked QC should stay at view 5 (not downgrade to 3)"
        );
        assert_eq!(
            state.last_committed_qc().view,
            3,
            "last committed QC updates regardless"
        );
    }

    #[test]
    fn test_default() {
        let state = RoundState::default();
        assert_eq!(state.current_view(), 1);
        assert_eq!(state.phase(), Phase::WaitingForProposal);
    }
}
