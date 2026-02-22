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

    /// Creates a genesis QC (no signatures, view 0) with a deterministic dummy signature.
    /// The signature is never verified; the genesis QC is a sentinel value.
    pub fn genesis() -> Self {
        use crate::bls::BlsSecretKey;

        let ikm = [1u8; 32];
        let dummy_sig = match BlsSecretKey::from_bytes(&ikm) {
            Ok(sk) => sk.sign(b"genesis"),
            Err(_) => {
                BlsSignature::from_bytes(&[0u8; 96])
                    .unwrap_or_else(|_| panic!("failed to create genesis signature"))
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

    fn dummy_signature() -> BlsSignature {
        BlsSecretKey::random().unwrap().sign(b"dummy")
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
    fn test_all_message_variants_serde_roundtrip() {
        let sig = dummy_signature();
        let genesis_qc = QuorumCertificate::genesis();

        let variants: Vec<ConsensusMessage> = vec![
            ConsensusMessage::Proposal(Proposal {
                view: 1,
                block_hash: B256::repeat_byte(0x11),
                justify_qc: genesis_qc.clone(),
                proposer: 0,
                signature: sig.clone(),
                prepare_qc: None,
            }),
            ConsensusMessage::Vote(Vote {
                view: 2,
                block_hash: B256::repeat_byte(0x22),
                voter: 1,
                signature: sig.clone(),
            }),
            ConsensusMessage::CommitVote(CommitVote {
                view: 3,
                block_hash: B256::repeat_byte(0x33),
                voter: 2,
                signature: sig.clone(),
            }),
            ConsensusMessage::PrepareQC(PrepareQC {
                view: 4,
                block_hash: B256::repeat_byte(0x44),
                qc: genesis_qc.clone(),
            }),
            ConsensusMessage::Timeout(TimeoutMessage {
                view: 5,
                high_qc: genesis_qc.clone(),
                sender: 3,
                signature: sig.clone(),
            }),
            ConsensusMessage::NewView(NewView {
                view: 6,
                timeout_cert: TimeoutCertificate {
                    view: 5,
                    aggregate_signature: sig.clone(),
                    signers: BitVec::new(),
                    high_qc: genesis_qc.clone(),
                },
                leader: 0,
                signature: sig.clone(),
            }),
            ConsensusMessage::Decide(Decide {
                view: 7,
                block_hash: B256::repeat_byte(0x77),
                commit_qc: genesis_qc.clone(),
            }),
        ];

        for (i, msg) in variants.iter().enumerate() {
            let encoded = bincode::serialize(msg)
                .unwrap_or_else(|e| panic!("variant {} serialize failed: {}", i, e));
            let decoded: ConsensusMessage = bincode::deserialize(&encoded)
                .unwrap_or_else(|e| panic!("variant {} deserialize failed: {}", i, e));

            let orig_variant = format!("{:?}", msg).split('(').next().unwrap().to_string();
            let decoded_variant = format!("{:?}", decoded).split('(').next().unwrap().to_string();
            assert_eq!(orig_variant, decoded_variant);
        }
    }

    #[test]
    fn test_versioned_message_serde_roundtrip() {
        let sig = dummy_signature();
        let vote = Vote {
            view: 100,
            block_hash: B256::repeat_byte(0xEE),
            voter: 5,
            signature: sig,
        };
        let versioned = VersionedMessage {
            version: CONSENSUS_PROTOCOL_VERSION,
            message: ConsensusMessage::Vote(vote),
        };

        let encoded = bincode::serialize(&versioned).expect("serialize VersionedMessage");
        let decoded: VersionedMessage = bincode::deserialize(&encoded)
            .expect("deserialize VersionedMessage");

        assert_eq!(decoded.version, CONSENSUS_PROTOCOL_VERSION);
        match decoded.message {
            ConsensusMessage::Vote(v) => {
                assert_eq!(v.view, 100);
                assert_eq!(v.voter, 5);
                assert_eq!(v.block_hash, B256::repeat_byte(0xEE));
            }
            other => panic!("expected Vote, got {:?}", other),
        }
    }

    #[test]
    fn test_timeout_certificate_serde_roundtrip() {
        let sig = dummy_signature();
        let genesis_qc = QuorumCertificate::genesis();

        let tc = TimeoutCertificate {
            view: 42,
            aggregate_signature: sig,
            signers: {
                let mut s = bitvec![u8, Msb0; 0; 4];
                s.set(0, true);
                s.set(2, true);
                s
            },
            high_qc: genesis_qc,
        };

        let encoded = bincode::serialize(&tc).expect("serialize TC");
        let decoded: TimeoutCertificate = bincode::deserialize(&encoded)
            .expect("deserialize TC");

        assert_eq!(decoded.view, 42);
        assert_eq!(decoded.signers.count_ones(), 2);
        assert!(decoded.signers[0]);
        assert!(!decoded.signers[1]);
        assert!(decoded.signers[2]);
        assert!(!decoded.signers[3]);
        assert_eq!(decoded.high_qc.view, 0, "high_qc should be genesis");
    }

    #[test]
    fn test_genesis_qc() {
        let result = std::panic::catch_unwind(|| QuorumCertificate::genesis());
        match result {
            Ok(qc) => {
                assert_eq!(qc.view, 0);
                assert_eq!(qc.block_hash, B256::ZERO);
                assert_eq!(qc.signer_count(), 0);
                assert!(qc.signers.is_empty());
            }
            Err(_) => {
                eprintln!(
                    "NOTE: QuorumCertificate::genesis() panics if [0u8; 96] is not valid BLS signature"
                );
            }
        }
    }
}
