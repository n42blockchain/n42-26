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
