//! Read-only verification of chain-bound gov5 H2-v4 finality proofs.

use n42_consensus::ValidatorSet;
use n42_network::h2_v4::{H2V4Envelope, commit_signing_message};
use n42_network::h2_wire::{H2Decide, H2Message, H2QuorumCertificate};
use n42_primitives::BlsSignature;
use n42_primitives::bls::AggregateSignature;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct H2V4FinalityProof {
    pub view: u64,
    pub block_hash: alloy_primitives::B256,
    pub changes_hash: alloy_primitives::B256,
}

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum H2V4FinalityError {
    #[error("H2-v4 message is not a Decide")]
    NotDecide,
    #[error("H2-v4 Decide does not match its CommitQC")]
    DecideQcMismatch,
    #[error("H2-v4 signer bitmap is not canonical for validator set size {expected}")]
    InvalidBitmap { expected: usize },
    #[error("H2-v4 CommitQC has {actual} signers; quorum requires {required}")]
    InsufficientQuorum { actual: usize, required: usize },
    #[error("H2-v4 CommitQC signature is invalid")]
    InvalidSignature,
}

/// Verifies an H2-v4 Decide as a finality proof. This function is deliberately
/// side-effect free: observers may use the result as a sync target, but it
/// never enters the voting state machine.
pub fn verify_h2_v4_decide(
    envelope: &H2V4Envelope,
    validator_set: &ValidatorSet,
) -> Result<H2V4FinalityProof, H2V4FinalityError> {
    let H2Message::Decide(decide) = &envelope.message else {
        return Err(H2V4FinalityError::NotDecide);
    };
    verify_decide_fields(decide)?;

    let signer_indices = signer_indices(&decide.commit_qc, validator_set.len() as usize)?;
    let required = validator_set.quorum_size();
    if signer_indices.len() < required {
        return Err(H2V4FinalityError::InsufficientQuorum {
            actual: signer_indices.len(),
            required,
        });
    }
    let public_keys = validator_set
        .public_keys_for_signers(&signer_indices)
        .map_err(|_| H2V4FinalityError::InvalidBitmap {
            expected: validator_set.len() as usize,
        })?;
    let signature_bytes: [u8; 96] = decide
        .commit_qc
        .aggregate_signature
        .as_slice()
        .try_into()
        .map_err(|_| H2V4FinalityError::InvalidSignature)?;
    let signature = BlsSignature::from_bytes(&signature_bytes)
        .map_err(|_| H2V4FinalityError::InvalidSignature)?;
    let message = commit_signing_message(
        envelope.identity,
        decide.view,
        decide.block_hash,
        envelope.changes_hash,
    );
    AggregateSignature::verify_h2_v4_aggregate(&message, &signature, &public_keys)
        .map_err(|_| H2V4FinalityError::InvalidSignature)?;

    Ok(H2V4FinalityProof {
        view: decide.view,
        block_hash: decide.block_hash,
        changes_hash: envelope.changes_hash,
    })
}

fn verify_decide_fields(decide: &H2Decide) -> Result<(), H2V4FinalityError> {
    if decide.view != decide.commit_qc.view || decide.block_hash != decide.commit_qc.block_hash {
        return Err(H2V4FinalityError::DecideQcMismatch);
    }
    Ok(())
}

fn signer_indices(
    qc: &H2QuorumCertificate,
    expected_count: usize,
) -> Result<Vec<u32>, H2V4FinalityError> {
    let bitmap = &qc.signers_bitmap;
    let encoded_count = bitmap
        .get(..2)
        .and_then(|bytes| <[u8; 2]>::try_from(bytes).ok())
        .map(u16::from_le_bytes)
        .map(usize::from);
    let byte_count = expected_count.div_ceil(8);
    if encoded_count != Some(expected_count) || bitmap.len() != 2 + byte_count {
        return Err(H2V4FinalityError::InvalidBitmap {
            expected: expected_count,
        });
    }
    if !expected_count.is_multiple_of(8) {
        let used_mask = (1u16 << (expected_count % 8)) as u8 - 1;
        if bitmap.last().is_some_and(|last| last & !used_mask != 0) {
            return Err(H2V4FinalityError::InvalidBitmap {
                expected: expected_count,
            });
        }
    }
    Ok((0..expected_count)
        .filter(|index| bitmap[2 + index / 8] & (1 << (index % 8)) != 0)
        .map(|index| index as u32)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, B256};
    use n42_chainspec::ValidatorInfo;
    use n42_network::h2_v4::{H2V4ChainIdentity, decode_envelope};
    use n42_primitives::BlsPublicKey;
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct Fixture {
        schema: String,
        chain_id: u64,
        genesis_hash: String,
        changes_hash: String,
        validator_public_keys: Vec<String>,
        envelope_hex: String,
    }

    fn fixture() -> (H2V4Envelope, ValidatorSet) {
        let fixture: Fixture =
            serde_json::from_str(include_str!("../testdata/h2_v4_finality_v1.json")).unwrap();
        assert_eq!(fixture.schema, "n42-h2-v4-finality-v1");
        let identity = H2V4ChainIdentity {
            chain_id: fixture.chain_id,
            genesis_hash: B256::from_slice(&hex::decode(fixture.genesis_hash).unwrap()),
        };
        let envelope =
            decode_envelope(&hex::decode(fixture.envelope_hex).unwrap(), identity).unwrap();
        assert_eq!(
            envelope.changes_hash,
            B256::from_slice(&hex::decode(fixture.changes_hash).unwrap())
        );
        let validators = fixture
            .validator_public_keys
            .iter()
            .enumerate()
            .map(|(index, encoded)| {
                let bytes: [u8; 48] = hex::decode(encoded).unwrap().try_into().unwrap();
                ValidatorInfo {
                    address: Address::with_last_byte(index as u8 + 1),
                    bls_public_key: BlsPublicKey::from_bytes(&bytes).unwrap(),
                    p2p_peer_id: None,
                }
            })
            .collect::<Vec<_>>();
        (envelope, ValidatorSet::new(&validators, 1))
    }

    #[test]
    fn verifies_gov5_finality_fixture_and_binds_domain() {
        let (envelope, validator_set) = fixture();
        let proof = verify_h2_v4_decide(&envelope, &validator_set).unwrap();
        assert_eq!(proof.view, 9001);
        assert_eq!(proof.block_hash, B256::repeat_byte(0x44));

        let mut wrong_changes = envelope.clone();
        wrong_changes.changes_hash[0] ^= 1;
        assert_eq!(
            verify_h2_v4_decide(&wrong_changes, &validator_set),
            Err(H2V4FinalityError::InvalidSignature)
        );

        let mut insufficient = envelope;
        let H2Message::Decide(decide) = &mut insufficient.message else {
            unreachable!()
        };
        decide.commit_qc.signers_bitmap[2] = 0b0000_0011;
        assert_eq!(
            verify_h2_v4_decide(&insufficient, &validator_set),
            Err(H2V4FinalityError::InsufficientQuorum {
                actual: 2,
                required: 3,
            })
        );
    }
}
