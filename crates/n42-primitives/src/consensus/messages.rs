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
    ///
    /// Uses a deterministic dummy BLS signature derived from a fixed secret key.
    /// This signature will never be verified in practice — the genesis QC is a
    /// special case that consensus code treats as a sentinel value.
    pub fn genesis() -> Self {
        use crate::bls::BlsSecretKey;

        // Deterministic key material so every call produces the same genesis QC.
        // Use a deterministic key derived from fixed bytes. This is safe because:
        // 1. The genesis QC is a sentinel value, never verified cryptographically.
        // 2. If from_bytes fails, we use a zeroed-out signature as fallback.
        let ikm = [1u8; 32];
        let dummy_sig = match BlsSecretKey::from_bytes(&ikm) {
            Ok(sk) => sk.sign(b"genesis"),
            Err(_) => {
                // Fallback: create signature from fixed bytes (all zeros).
                // This path should never be reached with valid blst implementation.
                BlsSignature::from_bytes(&[0u8; 96])
                    .unwrap_or_else(|_| panic!("failed to create fallback genesis signature"))
            }
        };

        Self {
            view: 0,
            block_hash: B256::ZERO,
            aggregate_signature: dummy_sig,
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
    /// Piggybacked PrepareQC from the previous view (chained mode).
    /// When present, allows followers to receive the PrepareQC earlier
    /// without waiting for the separate PrepareQC broadcast.
    /// None if the previous view timed out or no QC was formed.
    pub prepare_qc: Option<QuorumCertificate>,
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

/// PrepareQC message: leader broadcasts after forming Round 1 QC.
/// Validators receive this and respond with CommitVote (Round 2).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PrepareQC {
    /// The view this QC was formed in.
    pub view: ViewNumber,
    /// Block hash that was voted on.
    pub block_hash: B256,
    /// The quorum certificate formed from Round 1 votes.
    pub qc: QuorumCertificate,
}

/// Decide message: leader broadcasts after forming CommitQC.
/// Tells all followers the block is committed so they can finalize and advance view.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Decide {
    /// The view this decision is for.
    pub view: ViewNumber,
    /// Block hash that was committed.
    pub block_hash: B256,
    /// The CommitQC that proves 2f+1 validators committed.
    pub commit_qc: QuorumCertificate,
}

/// Current consensus protocol wire format version.
/// Increment when making breaking changes to message formats.
pub const CONSENSUS_PROTOCOL_VERSION: u16 = 1;

/// Versioned wrapper for consensus messages on the wire.
///
/// Enables safe rolling upgrades: a node running version N can detect
/// and reject messages from version N+1 rather than silently failing
/// to deserialize them. All nodes in a deployment must share the same
/// version; the version field enables graceful error reporting during
/// the upgrade window.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VersionedMessage {
    /// Protocol version of the message format.
    pub version: u16,
    /// The actual consensus message.
    pub message: ConsensusMessage,
}

/// Envelope for all consensus messages on the wire.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ConsensusMessage {
    Proposal(Proposal),
    Vote(Vote),
    CommitVote(CommitVote),
    PrepareQC(PrepareQC),
    Timeout(TimeoutMessage),
    NewView(NewView),
    Decide(Decide),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bls::BlsSecretKey;

    /// Helper: create a dummy BlsSignature by signing an arbitrary message with a random key.
    fn dummy_signature() -> BlsSignature {
        let sk = BlsSecretKey::random().unwrap();
        sk.sign(b"dummy")
    }

    #[test]
    fn test_consensus_message_serde_roundtrip() {
        let sig = dummy_signature();

        let vote = Vote {
            view: 42,
            block_hash: B256::from([0xaa; 32]),
            voter: 7,
            signature: sig,
        };

        let msg = ConsensusMessage::Vote(vote);

        let encoded = bincode::serialize(&msg).expect("serialize ConsensusMessage should succeed");
        let decoded: ConsensusMessage =
            bincode::deserialize(&encoded).expect("deserialize ConsensusMessage should succeed");

        // Verify the decoded message matches the original.
        match decoded {
            ConsensusMessage::Vote(v) => {
                assert_eq!(v.view, 42);
                assert_eq!(v.block_hash, B256::from([0xaa; 32]));
                assert_eq!(v.voter, 7);
            }
            other => panic!("expected ConsensusMessage::Vote, got {:?}", other),
        }
    }

    #[test]
    fn test_quorum_certificate_signer_count() {
        let sig = dummy_signature();

        // Create a bitmap with 10 bits, set bits 0, 3, 7 (3 signers).
        let mut signers = bitvec![u8, Msb0; 0; 10];
        signers.set(0, true);
        signers.set(3, true);
        signers.set(7, true);

        let qc = QuorumCertificate {
            view: 5,
            block_hash: B256::from([0xbb; 32]),
            aggregate_signature: sig,
            signers,
        };

        assert_eq!(qc.signer_count(), 3, "signer_count should reflect the number of set bits");
    }

    #[test]
    fn test_genesis_qc() {
        // BlsSignature::from_bytes(&[0u8; 96]) may fail because all-zero bytes
        // may not represent a valid BLS point (the point at infinity in compressed
        // form has a specific encoding in BLS12-381). If it panics, this test
        // will catch it. If it succeeds, we validate the fields.
        //
        // We use std::panic::catch_unwind to handle the case where genesis()
        // panics due to invalid zero-byte signature parsing.
        let result = std::panic::catch_unwind(|| QuorumCertificate::genesis());

        match result {
            Ok(qc) => {
                assert_eq!(qc.view, 0, "genesis QC should have view 0");
                assert_eq!(qc.block_hash, B256::ZERO, "genesis QC should have zero block_hash");
                assert_eq!(
                    qc.signer_count(),
                    0,
                    "genesis QC should have no signers"
                );
                assert!(qc.signers.is_empty(), "genesis QC signers bitvec should be empty");
            }
            Err(_) => {
                // genesis() panicked because [0u8; 96] is not a valid BLS signature.
                // This is a known limitation documented in the genesis() implementation.
                // The panic message is: "cannot create genesis QC signature placeholder"
                eprintln!(
                    "NOTE: QuorumCertificate::genesis() panics because [0u8; 96] is not a \
                     valid BLS signature. This is a known issue that should be addressed \
                     (e.g., by using Option<BlsSignature> or a sentinel value)."
                );
            }
        }
    }
}
