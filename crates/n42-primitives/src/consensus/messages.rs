use crate::bls::BlsSignature;
use alloy_primitives::B256;
use bitvec::prelude::*;
use serde::{Deserialize, Serialize};

/// View number (monotonically increasing round identifier).
pub type ViewNumber = u64;

/// Validator index within the validator set.
pub type ValidatorIndex = u32;

/// A quorum certificate: aggregated BLS signature + signer bitmap.
/// Proves that 2f+1 validators signed a particular message.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QuorumCertificate {
    /// The view this QC was formed in.
    pub view: ViewNumber,
    /// Block hash that was voted on.
    pub block_hash: B256,
    /// Aggregated BLS signature from 2f+1 validators.
    pub aggregate_signature: BlsSignature,
    /// Bitmap indicating which validators signed (1 bit per validator).
    pub signers: BitVec<u8, Msb0>,
}

impl QuorumCertificate {
    /// Returns the number of signers.
    pub fn signer_count(&self) -> usize {
        self.signers.count_ones()
    }

    /// Creates a genesis QC (no signatures, view 0).
    pub fn genesis() -> Self {
        Self {
            view: 0,
            block_hash: B256::ZERO,
            // A "zero" signature - will not be verified in practice.
            aggregate_signature: BlsSignature::from_bytes(&[0u8; 96])
                .unwrap_or_else(|_| {
                    // If zero bytes don't parse, use a dummy approach.
                    // In production, the genesis QC is a special case that skips verification.
                    panic!("cannot create genesis QC signature placeholder")
                }),
            signers: BitVec::new(),
        }
    }
}

/// A timeout certificate: proves that 2f+1 validators timed out in a given view.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TimeoutCertificate {
    /// The view that timed out.
    pub view: ViewNumber,
    /// Aggregated timeout signature.
    pub aggregate_signature: BlsSignature,
    /// Bitmap of validators that timed out.
    pub signers: BitVec<u8, Msb0>,
    /// The highest QC known by any of the timeout signers.
    pub high_qc: QuorumCertificate,
}

/// Leader proposal message.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Proposal {
    /// Current view number.
    pub view: ViewNumber,
    /// Hash of the proposed block.
    pub block_hash: B256,
    /// The QC that justifies this proposal (from the previous round).
    pub justify_qc: QuorumCertificate,
    /// Proposer's validator index.
    pub proposer: ValidatorIndex,
    /// Proposer's BLS signature over (view, block_hash).
    pub signature: BlsSignature,
}

/// Vote message (Round 1: Prepare).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Vote {
    /// View being voted on.
    pub view: ViewNumber,
    /// Block hash being voted for.
    pub block_hash: B256,
    /// Voter's validator index.
    pub voter: ValidatorIndex,
    /// BLS signature over (view, block_hash).
    pub signature: BlsSignature,
}

/// Commit vote (Round 2).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommitVote {
    /// View being committed.
    pub view: ViewNumber,
    /// Block hash being committed.
    pub block_hash: B256,
    /// Voter's validator index.
    pub voter: ValidatorIndex,
    /// BLS signature over ("commit", view, block_hash).
    pub signature: BlsSignature,
}

/// Timeout message: sent when a validator's timer expires.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TimeoutMessage {
    /// View that timed out.
    pub view: ViewNumber,
    /// The highest QC this validator knows of.
    pub high_qc: QuorumCertificate,
    /// Sender's validator index.
    pub sender: ValidatorIndex,
    /// BLS signature over (view, high_qc.view).
    pub signature: BlsSignature,
}

/// NewView message: sent by the new leader after collecting a TC.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NewView {
    /// The new view number.
    pub view: ViewNumber,
    /// The timeout certificate that triggered the view change.
    pub timeout_cert: TimeoutCertificate,
    /// New leader's validator index.
    pub leader: ValidatorIndex,
    /// BLS signature over (view, timeout_cert hash).
    pub signature: BlsSignature,
}

/// Envelope for all consensus messages on the wire.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ConsensusMessage {
    Proposal(Proposal),
    Vote(Vote),
    CommitVote(CommitVote),
    Timeout(TimeoutMessage),
    NewView(NewView),
}
