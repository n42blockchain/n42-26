//! Lossless conversion between native consensus values and the gov5 H2-v4 schema.
//!
//! The current production profile is intentionally static-validator only:
//! gov5 commits `changes_hash == 0`, and its wire proposal does not carry the
//! Rust validator-change list needed to reconstruct a non-zero commitment.

use alloy_primitives::B256;
use bitvec::prelude::{BitVec, Msb0};
use n42_primitives::{
    BlsSignature,
    consensus::{
        CommitVote, ConsensusMessage, Decide, NewView, PrepareQC, Proposal, QuorumCertificate,
        TimeoutCertificate, TimeoutMessage, Vote,
    },
};

use crate::h2_v4::{H2V4ChainIdentity, H2V4Envelope};
use crate::h2_wire::{
    H2Decide, H2Message, H2NewView, H2PrepareQc, H2Proposal, H2QuorumCertificate, H2Timeout,
    H2TimeoutCertificate, H2Vote,
};

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum H2BridgeError {
    #[error("H2-v4 participant profile requires changes_hash == 0")]
    NonZeroChangesHash,
    #[error("H2-v4 signer bitmap is malformed")]
    InvalidBitmap,
    #[error("H2-v4 aggregate signature is malformed")]
    InvalidAggregateSignature,
    #[error("H2-v4 participant profile does not support validator changes")]
    ValidatorChangesUnsupported,
}

fn native_bitmap_to_h2(signers: &BitVec<u8, Msb0>) -> Result<Vec<u8>, H2BridgeError> {
    let count = u16::try_from(signers.len()).map_err(|_| H2BridgeError::InvalidBitmap)?;
    if count == 0 {
        return Ok(vec![0, 0]);
    }
    let mut encoded = Vec::with_capacity(2 + signers.len().div_ceil(8));
    encoded.extend_from_slice(&count.to_le_bytes());
    encoded.resize(2 + signers.len().div_ceil(8), 0);
    for (index, bit) in signers.iter().enumerate() {
        if *bit {
            encoded[2 + index / 8] |= 1 << (index % 8);
        }
    }
    Ok(encoded)
}

fn h2_bitmap_to_native(encoded: &[u8]) -> Result<BitVec<u8, Msb0>, H2BridgeError> {
    if encoded == [0, 0] {
        return Ok(BitVec::new());
    }
    let [lo, hi, rest @ ..] = encoded else {
        return Err(H2BridgeError::InvalidBitmap);
    };
    let count = u16::from_le_bytes([*lo, *hi]) as usize;
    if count == 0 || rest.len() != count.div_ceil(8) {
        return Err(H2BridgeError::InvalidBitmap);
    }
    if !count.is_multiple_of(8) {
        let used_mask = (1u8 << (count % 8)) - 1;
        if rest.last().is_some_and(|last| last & !used_mask != 0) {
            return Err(H2BridgeError::InvalidBitmap);
        }
    }
    Ok((0..count)
        .map(|index| rest[index / 8] & (1 << (index % 8)) != 0)
        .collect())
}

fn qc_to_h2(qc: &QuorumCertificate) -> Result<H2QuorumCertificate, H2BridgeError> {
    if qc.view == 0 && qc.block_hash == B256::ZERO && qc.signers.is_empty() {
        return Ok(H2QuorumCertificate {
            view: 0,
            block_hash: B256::ZERO,
            aggregate_signature: Vec::new(),
            signers_bitmap: vec![0, 0],
        });
    }
    Ok(H2QuorumCertificate {
        view: qc.view,
        block_hash: qc.block_hash,
        aggregate_signature: qc.aggregate_signature.to_bytes().to_vec(),
        signers_bitmap: native_bitmap_to_h2(&qc.signers)?,
    })
}

fn qc_from_h2(qc: H2QuorumCertificate) -> Result<QuorumCertificate, H2BridgeError> {
    if qc.view == 0
        && qc.block_hash == B256::ZERO
        && qc.aggregate_signature.is_empty()
        && qc.signers_bitmap == [0, 0]
    {
        return Ok(QuorumCertificate::genesis());
    }
    let signature: [u8; 96] = qc
        .aggregate_signature
        .try_into()
        .map_err(|_| H2BridgeError::InvalidAggregateSignature)?;
    Ok(QuorumCertificate {
        view: qc.view,
        block_hash: qc.block_hash,
        aggregate_signature: BlsSignature::from_bytes(&signature)
            .map_err(|_| H2BridgeError::InvalidAggregateSignature)?,
        signers: h2_bitmap_to_native(&qc.signers_bitmap)?,
    })
}

fn tc_to_h2(tc: &TimeoutCertificate) -> Result<H2TimeoutCertificate, H2BridgeError> {
    Ok(H2TimeoutCertificate {
        view: tc.view,
        aggregate_signature: tc.aggregate_signature.to_bytes().to_vec(),
        signers_bitmap: native_bitmap_to_h2(&tc.signers)?,
        high_qc: qc_to_h2(&tc.high_qc)?,
    })
}

fn tc_from_h2(tc: H2TimeoutCertificate) -> Result<TimeoutCertificate, H2BridgeError> {
    let signature: [u8; 96] = tc
        .aggregate_signature
        .try_into()
        .map_err(|_| H2BridgeError::InvalidAggregateSignature)?;
    Ok(TimeoutCertificate {
        view: tc.view,
        aggregate_signature: BlsSignature::from_bytes(&signature)
            .map_err(|_| H2BridgeError::InvalidAggregateSignature)?,
        signers: h2_bitmap_to_native(&tc.signers_bitmap)?,
        high_qc: qc_from_h2(tc.high_qc)?,
    })
}

pub fn consensus_to_h2_v4(
    identity: H2V4ChainIdentity,
    message: &ConsensusMessage,
) -> Result<H2V4Envelope, H2BridgeError> {
    let message = match message {
        ConsensusMessage::Proposal(value) => {
            if value
                .validator_changes
                .as_ref()
                .is_some_and(|changes| !changes.is_empty())
            {
                return Err(H2BridgeError::ValidatorChangesUnsupported);
            }
            H2Message::Proposal(H2Proposal {
                view: value.view,
                block_hash: value.block_hash,
                justify_qc: qc_to_h2(&value.justify_qc)?,
                proposer: value.proposer,
                signature: value.signature.clone(),
                prepare_qc: value.prepare_qc.as_ref().map(qc_to_h2).transpose()?,
                tx_root_hash: value.tx_root_hash.unwrap_or_default(),
            })
        }
        ConsensusMessage::Vote(value) => H2Message::Vote(H2Vote {
            view: value.view,
            block_hash: value.block_hash,
            voter: value.voter,
            signature: value.signature.clone(),
            high_tc: None,
        }),
        ConsensusMessage::CommitVote(value) => H2Message::CommitVote(H2Vote {
            view: value.view,
            block_hash: value.block_hash,
            voter: value.voter,
            signature: value.signature.clone(),
            high_tc: None,
        }),
        ConsensusMessage::PrepareQC(value) => H2Message::PrepareQc(H2PrepareQc {
            view: value.view,
            block_hash: value.block_hash,
            qc: qc_to_h2(&value.qc)?,
        }),
        ConsensusMessage::Timeout(value) => H2Message::Timeout(H2Timeout {
            view: value.view,
            high_qc: qc_to_h2(&value.high_qc)?,
            sender: value.sender,
            signature: value.signature.clone(),
            high_tc: None,
        }),
        ConsensusMessage::NewView(value) => H2Message::NewView(H2NewView {
            view: value.view,
            timeout_certificate: tc_to_h2(&value.timeout_cert)?,
            leader: value.leader,
            signature: value.signature.clone(),
        }),
        ConsensusMessage::Decide(value) => {
            if value.validator_changes_hash != B256::ZERO {
                return Err(H2BridgeError::NonZeroChangesHash);
            }
            H2Message::Decide(H2Decide {
                view: value.view,
                block_hash: value.block_hash,
                commit_qc: qc_to_h2(&value.commit_qc)?,
            })
        }
    };
    Ok(H2V4Envelope {
        identity,
        changes_hash: B256::ZERO,
        message,
    })
}

pub fn consensus_from_h2_v4(envelope: H2V4Envelope) -> Result<ConsensusMessage, H2BridgeError> {
    if envelope.changes_hash != B256::ZERO {
        return Err(H2BridgeError::NonZeroChangesHash);
    }
    Ok(match envelope.message {
        H2Message::Proposal(value) => ConsensusMessage::Proposal(Proposal {
            view: value.view,
            block_hash: value.block_hash,
            justify_qc: qc_from_h2(value.justify_qc)?,
            proposer: value.proposer,
            signature: value.signature,
            prepare_qc: value.prepare_qc.map(qc_from_h2).transpose()?,
            tx_root_hash: Some(value.tx_root_hash),
            validator_changes: None,
        }),
        H2Message::Vote(value) => ConsensusMessage::Vote(Vote {
            view: value.view,
            block_hash: value.block_hash,
            voter: value.voter,
            signature: value.signature,
        }),
        H2Message::CommitVote(value) => ConsensusMessage::CommitVote(CommitVote {
            view: value.view,
            block_hash: value.block_hash,
            voter: value.voter,
            signature: value.signature,
        }),
        H2Message::PrepareQc(value) => ConsensusMessage::PrepareQC(PrepareQC {
            view: value.view,
            block_hash: value.block_hash,
            qc: qc_from_h2(value.qc)?,
        }),
        H2Message::Timeout(value) => ConsensusMessage::Timeout(TimeoutMessage {
            view: value.view,
            high_qc: qc_from_h2(value.high_qc)?,
            sender: value.sender,
            signature: value.signature,
        }),
        H2Message::NewView(value) => ConsensusMessage::NewView(NewView {
            view: value.view,
            timeout_cert: tc_from_h2(value.timeout_certificate)?,
            leader: value.leader,
            signature: value.signature,
        }),
        H2Message::Decide(value) => ConsensusMessage::Decide(Decide {
            view: value.view,
            block_hash: value.block_hash,
            commit_qc: qc_from_h2(value.commit_qc)?,
            validator_changes_hash: B256::ZERO,
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use n42_primitives::{BlsSecretKey, consensus::ValidatorChange};

    fn key() -> BlsSecretKey {
        BlsSecretKey::key_gen(&[0x42; 32]).unwrap()
    }

    fn qc(view: u64, hash: B256) -> QuorumCertificate {
        let signature = key().sign_h2_v4(b"bridge-fixture");
        QuorumCertificate {
            view,
            block_hash: hash,
            aggregate_signature: signature,
            signers: [true, false, true, false, false, false, true]
                .into_iter()
                .collect(),
        }
    }

    fn tc(view: u64, hash: B256) -> TimeoutCertificate {
        TimeoutCertificate {
            view,
            aggregate_signature: key().sign_h2_v4(b"tc-fixture"),
            signers: [true, true, true, false, false, false, false]
                .into_iter()
                .collect(),
            high_qc: qc(view.saturating_sub(1), hash),
        }
    }

    #[test]
    fn all_consensus_kinds_round_trip_and_preserve_lsb_bitmap_order() {
        let identity = H2V4ChainIdentity {
            chain_id: 42,
            genesis_hash: B256::repeat_byte(0x11),
        };
        let hash = B256::repeat_byte(0x22);
        let signature = key().sign_h2_v4(b"message-fixture");
        let messages = vec![
            ConsensusMessage::Proposal(Proposal {
                view: 8,
                block_hash: hash,
                justify_qc: qc(7, hash),
                proposer: 1,
                signature: signature.clone(),
                prepare_qc: Some(qc(6, hash)),
                tx_root_hash: Some(B256::repeat_byte(0x33)),
                validator_changes: None,
            }),
            ConsensusMessage::Vote(Vote {
                view: 8,
                block_hash: hash,
                voter: 2,
                signature: signature.clone(),
            }),
            ConsensusMessage::CommitVote(CommitVote {
                view: 8,
                block_hash: hash,
                voter: 2,
                signature: signature.clone(),
            }),
            ConsensusMessage::PrepareQC(PrepareQC {
                view: 8,
                block_hash: hash,
                qc: qc(8, hash),
            }),
            ConsensusMessage::Timeout(TimeoutMessage {
                view: 8,
                high_qc: qc(7, hash),
                sender: 3,
                signature: signature.clone(),
            }),
            ConsensusMessage::NewView(NewView {
                view: 9,
                timeout_cert: tc(8, hash),
                leader: 4,
                signature: signature.clone(),
            }),
            ConsensusMessage::Decide(Decide {
                view: 8,
                block_hash: hash,
                commit_qc: qc(8, hash),
                validator_changes_hash: B256::ZERO,
            }),
        ];

        for message in messages {
            let envelope = consensus_to_h2_v4(identity, &message).unwrap();
            let bitmap = match &envelope.message {
                H2Message::Proposal(value) => &value.justify_qc.signers_bitmap,
                H2Message::PrepareQc(value) => &value.qc.signers_bitmap,
                H2Message::Timeout(value) => &value.high_qc.signers_bitmap,
                H2Message::NewView(value) => &value.timeout_certificate.high_qc.signers_bitmap,
                H2Message::Decide(value) => &value.commit_qc.signers_bitmap,
                H2Message::Vote(_) | H2Message::CommitVote(_) => {
                    let recovered = consensus_from_h2_v4(envelope).unwrap();
                    assert_eq!(
                        bincode::serialize(&recovered).unwrap(),
                        bincode::serialize(&message).unwrap()
                    );
                    continue;
                }
            };
            assert_eq!(bitmap, &[7, 0, 0b0100_0101]);
            let recovered = consensus_from_h2_v4(envelope).unwrap();
            assert_eq!(
                bincode::serialize(&recovered).unwrap(),
                bincode::serialize(&message).unwrap()
            );
        }
    }

    #[test]
    fn static_profile_rejects_nonzero_changes_in_both_directions() {
        let identity = H2V4ChainIdentity {
            chain_id: 1,
            genesis_hash: B256::ZERO,
        };
        let mut envelope = consensus_to_h2_v4(
            identity,
            &ConsensusMessage::Vote(Vote {
                view: 1,
                block_hash: B256::ZERO,
                voter: 0,
                signature: key().sign_h2_v4(b"vote"),
            }),
        )
        .unwrap();
        envelope.changes_hash = B256::repeat_byte(1);
        assert!(matches!(
            consensus_from_h2_v4(envelope),
            Err(H2BridgeError::NonZeroChangesHash)
        ));

        let proposal = ConsensusMessage::Proposal(Proposal {
            view: 1,
            block_hash: B256::ZERO,
            justify_qc: QuorumCertificate::genesis(),
            proposer: 0,
            signature: key().sign_h2_v4(b"proposal"),
            prepare_qc: None,
            tx_root_hash: Some(B256::ZERO),
            validator_changes: Some(vec![ValidatorChange::Remove {
                address: alloy_primitives::Address::ZERO,
            }]),
        });
        assert_eq!(
            consensus_to_h2_v4(identity, &proposal),
            Err(H2BridgeError::ValidatorChangesUnsupported)
        );
    }
}
