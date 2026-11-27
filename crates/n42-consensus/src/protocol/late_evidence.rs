//! Equivocation evidence across the view boundary (E15, AM28).
//!
//! A leader advances the view the moment its CommitQC forms, and every view
//! advance clears the per-view equivocation trackers. A vote, commit vote or
//! proposal that arrives after that is stale, so the second message of a
//! conflicting pair whose first was counted (or a pair both of whose messages
//! are late) used to be dropped unseen. The trackers of the last
//! [`LATE_EVIDENCE_VIEWS`] closed views are kept here, and late messages of
//! those views are checked against them. Late messages never reach a
//! collector and never change consensus state.
//!
//! Votes and commit votes are addressed to the view's leader, so only views
//! this node led hold them (the trackers are empty otherwise); proposals are
//! tracked by every node. A late message is kept as it came, unverified: its
//! signature is checked only when a second message of the same signer
//! conflicts with it, so an honest late message costs one map insert.
//! Evidence is emitted only after both signatures verify.

use alloy_primitives::B256;
use n42_primitives::{
    BlsSignature,
    consensus::{CommitVote, ConsensusMessage, Proposal, Vote},
};
use std::collections::{HashMap, VecDeque};

use super::state_machine::{
    AuthenticatedVoteProof, ConsensusEngine, EquivocationKind, SignedMessage,
};
use crate::error::ConsensusResult;

/// How many closed views back a late message is still checked.
pub(super) const LATE_EVIDENCE_VIEWS: u64 = 3;

/// The first verified message of each signer in one closed view: its
/// commitment (the block hash for votes; the signed-message hash for
/// proposals, whose validator changes are signed too), block hash and
/// signed message.
type ClosedTracker = HashMap<u32, (B256, B256, SignedMessage)>;

#[derive(Debug)]
struct ClosedView {
    view: u64,
    votes: ClosedTracker,
    commit_votes: ClosedTracker,
    proposals: ClosedTracker,
}

impl ClosedView {
    fn tracker(&self, kind: EquivocationKind) -> &ClosedTracker {
        match kind {
            EquivocationKind::Vote => &self.votes,
            EquivocationKind::CommitVote => &self.commit_votes,
            EquivocationKind::Proposal => &self.proposals,
        }
    }
}

/// A late message kept as it arrived.
#[derive(Debug, Clone)]
struct LateMessage {
    commitment: B256,
    block_hash: B256,
    signed: Vec<u8>,
    signature: BlsSignature,
    /// Authenticated already (by the engine's batch verifier).
    verified: bool,
}

/// Closed-view trackers and late messages, both bounded by
/// [`LATE_EVIDENCE_VIEWS`] views × signers × three kinds.
#[derive(Debug, Default)]
pub(super) struct LateEvidence {
    closed: VecDeque<ClosedView>,
    late: HashMap<(EquivocationKind, u64, u32), LateMessage>,
}

impl LateEvidence {
    /// Keeps the trackers of `closing`, which the engine is leaving for
    /// `new_view`, and drops whatever is now outside the window.
    pub(super) fn close_view(
        &mut self,
        closing: u64,
        new_view: u64,
        votes: HashMap<u32, (B256, SignedMessage)>,
        commit_votes: HashMap<u32, (B256, SignedMessage)>,
        proposals: HashMap<u32, (B256, B256, SignedMessage)>,
    ) {
        let by_hash = |tracker: HashMap<u32, (B256, SignedMessage)>| -> ClosedTracker {
            tracker
                .into_iter()
                .map(|(signer, (hash, signed))| (signer, (hash, hash, signed)))
                .collect()
        };
        if !(votes.is_empty() && commit_votes.is_empty() && proposals.is_empty()) {
            self.closed.push_back(ClosedView {
                view: closing,
                votes: by_hash(votes),
                commit_votes: by_hash(commit_votes),
                proposals,
            });
        }
        let in_window = |view: u64| view < new_view && new_view - view <= LATE_EVIDENCE_VIEWS;
        self.closed.retain(|closed| in_window(closed.view));
        self.late.retain(|(_, view, _), _| in_window(*view));
    }

    fn closed_first(
        &self,
        kind: EquivocationKind,
        view: u64,
        signer: u32,
    ) -> Option<&(B256, B256, SignedMessage)> {
        self.closed
            .iter()
            .find(|closed| closed.view == view)
            .and_then(|closed| closed.tracker(kind).get(&signer))
    }

    #[cfg(test)]
    pub(super) fn retained(&self) -> (usize, usize) {
        (
            self.closed
                .iter()
                .map(|closed| {
                    closed.votes.len() + closed.commit_votes.len() + closed.proposals.len()
                })
                .sum(),
            self.late.len(),
        )
    }
}

/// One late message, normalized across kinds.
struct LateCandidate {
    kind: EquivocationKind,
    view: u64,
    signer: u32,
    commitment: B256,
    block_hash: B256,
    signed: Vec<u8>,
    signature: BlsSignature,
    verified: bool,
}

impl ConsensusEngine {
    /// Checks a message of a recently closed view for a conflict with the
    /// signer's other message of that view, and emits the AM28 evidence if
    /// both verify. Nothing else happens to it.
    pub(super) fn note_late_message(
        &mut self,
        msg: &ConsensusMessage,
        proof: Option<AuthenticatedVoteProof>,
    ) -> ConsensusResult<()> {
        let current = self.round_state.current_view();
        let candidate = match msg {
            ConsensusMessage::Vote(vote) => self.late_vote(vote, proof),
            ConsensusMessage::CommitVote(cv) => self.late_commit_vote(cv, proof),
            ConsensusMessage::Proposal(proposal) => self.late_proposal(proposal),
            _ => None,
        };
        let Some(candidate) = candidate else {
            return Ok(());
        };
        if !(candidate.view < current && current - candidate.view <= LATE_EVIDENCE_VIEWS) {
            return Ok(());
        }
        self.check_late(candidate)
    }

    fn late_vote(
        &self,
        vote: &Vote,
        proof: Option<AuthenticatedVoteProof>,
    ) -> Option<LateCandidate> {
        // Votes go to the view's leader; the current view tracks them only there.
        if !self.is_leader_for_view(vote.view) {
            return None;
        }
        Some(LateCandidate {
            kind: EquivocationKind::Vote,
            view: vote.view,
            signer: vote.voter,
            commitment: vote.block_hash,
            block_hash: vote.block_hash,
            signed: self
                .signing_profile
                .vote_message(vote.view, vote.block_hash),
            signature: vote.signature.clone(),
            verified: proof == Some(AuthenticatedVoteProof::Vote { signer: vote.voter }),
        })
    }

    fn late_commit_vote(
        &self,
        cv: &CommitVote,
        proof: Option<AuthenticatedVoteProof>,
    ) -> Option<LateCandidate> {
        if !self.is_leader_for_view(cv.view) {
            return None;
        }
        let changes_hash = self.cached_changes_hash(&cv.block_hash);
        Some(LateCandidate {
            kind: EquivocationKind::CommitVote,
            view: cv.view,
            signer: cv.voter,
            commitment: cv.block_hash,
            block_hash: cv.block_hash,
            signed: self
                .signing_profile
                .commit_message(cv.view, cv.block_hash, changes_hash),
            signature: cv.signature.clone(),
            verified: proof
                == Some(AuthenticatedVoteProof::CommitVote {
                    signer: cv.voter,
                    validator_changes_hash: changes_hash,
                }),
        })
    }

    fn late_proposal(&self, proposal: &Proposal) -> Option<LateCandidate> {
        let view_set = self.validator_set_for_view(proposal.view);
        if proposal.proposer != self.leader_in_set(proposal.view, view_set) {
            return None;
        }
        if self.signing_profile.is_gov5()
            && proposal
                .validator_changes
                .as_ref()
                .is_some_and(|changes| !changes.is_empty())
        {
            return None;
        }
        let signed = self.signing_profile.proposal_message(
            proposal.view,
            proposal.block_hash,
            &proposal.validator_changes,
        );
        Some(LateCandidate {
            kind: EquivocationKind::Proposal,
            view: proposal.view,
            signer: proposal.proposer,
            commitment: B256::from(*blake3::hash(&signed).as_bytes()),
            block_hash: proposal.block_hash,
            signed,
            signature: proposal.signature.clone(),
            verified: false,
        })
    }

    fn check_late(&mut self, candidate: LateCandidate) -> ConsensusResult<()> {
        let LateCandidate {
            kind,
            view,
            signer,
            commitment,
            block_hash,
            signed,
            signature,
            verified,
        } = candidate;
        let Ok(public_key) = self
            .validator_set_for_view(view)
            .get_public_key(signer)
            .cloned()
        else {
            return Ok(());
        };
        let profile = self.signing_profile;
        let key = (kind, view, signer);
        let first = match self.late_evidence.closed_first(kind, view, signer) {
            Some((first_commitment, ..)) if *first_commitment == commitment => return Ok(()),
            Some(first) => first.clone(),
            None => match self.late_evidence.late.get(&key).cloned() {
                None => {
                    self.late_evidence.late.insert(
                        key,
                        LateMessage {
                            commitment,
                            block_hash,
                            signed,
                            signature,
                            verified,
                        },
                    );
                    return Ok(());
                }
                Some(kept) if kept.commitment == commitment => return Ok(()),
                Some(kept) => {
                    if !kept.verified
                        && !profile.verify_single(&public_key, &kept.signed, &kept.signature)
                    {
                        // A forged first: this message takes its place.
                        self.late_evidence.late.insert(
                            key,
                            LateMessage {
                                commitment,
                                block_hash,
                                signed,
                                signature,
                                verified,
                            },
                        );
                        return Ok(());
                    }
                    (
                        kept.commitment,
                        kept.block_hash,
                        SignedMessage {
                            signed: kept.signed,
                            signature: kept.signature.to_bytes(),
                        },
                    )
                }
            },
        };
        if !verified && !profile.verify_single(&public_key, &signed, &signature) {
            tracing::debug!(target: "n42::cl::voting", view, signer, kind = kind.as_str(),
                "ignoring late message with an invalid signature");
            return Ok(());
        }
        let (first_commitment, first_hash, first_signed) = first;
        // The block hashes when they differ, else the signed-message
        // commitments (a proposal re-signed with other validator changes).
        let (hash1, hash2) = if first_hash != block_hash {
            (first_hash, block_hash)
        } else {
            (first_commitment, commitment)
        };
        tracing::warn!(target: "n42::cl::voting", view, validator = signer,
            kind = kind.as_str(), %hash1, %hash2,
            "equivocation detected after the view closed");
        let second = SignedMessage {
            signed,
            signature: signature.to_bytes(),
        };
        self.emit_equivocation(kind, view, signer, (hash1, hash2, first_signed, second))
    }
}
