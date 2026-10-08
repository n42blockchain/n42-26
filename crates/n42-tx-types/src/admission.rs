//! Bounded authentication boundary for native frames, before queue admission.
//! A frame owns immutable decoded transactions and authenticated sender identities.
//! Account nonce/balance and queue credits belong to the downstream state/queue.
use crate::{
    AltSigError, AltSigTx, FrameAttestation, FrameGateways, MAX_FRAME_ATTESTATIONS, frame_root,
    verify_batch,
};
use alloy_eips::eip2718::Decodable2718;
use alloy_primitives::{Address, B256, Bytes};
use std::{collections::HashSet, sync::Arc};

/// Local admission limits, independent of the frame-index retention bound.
#[derive(Debug, Clone, Copy)]
pub struct FrameLimits {
    pub max_transactions: usize,
    pub max_encoded_bytes: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum FrameAdmissionError {
    #[error("frame transaction count exceeds its nonzero admission limit")]
    TransactionCount,
    #[error("frame encoded bytes exceed its admission limit")]
    EncodedBytes,
    #[error("too many frame attestations")]
    AttestationCount,
    #[error("gateway quorum cannot be satisfied by the configured keys and attestation limit")]
    GatewayConfiguration,
    #[error("invalid native transaction envelope at index {0}")]
    Envelope(usize),
    #[error("transaction at index {0} belongs to another chain")]
    Chain(usize),
    #[error("duplicate transaction at index {0}")]
    Duplicate(usize),
    #[error("native transaction signature at index {index}: {source}")]
    Signature { index: usize, source: AltSigError },
}

/// Shared by queue, frame plan and executor; callers cannot mutate authenticated bytes.
#[derive(Debug)]
pub struct AdmittedTransaction {
    transaction: AltSigTx,
    sender: Address,
}

impl AdmittedTransaction {
    pub fn transaction(&self) -> &AltSigTx {
        &self.transaction
    }
    pub fn sender(&self) -> Address {
        self.sender
    }
}

/// Authentication outcome, not execution or canonical validity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameAuthentication {
    TransactionSignatures,
    TrustedGateways,
}

#[derive(Debug, Clone)]
pub struct AdmittedFrame {
    root: B256,
    chain_id: u64,
    encoded_bytes: usize,
    transactions: Arc<[AdmittedTransaction]>,
    authentication: FrameAuthentication,
}

impl AdmittedFrame {
    /// Decode exactly once, compute the root exactly once, and authenticate.
    /// The network decoder must also bound wire allocation before calling this API.
    /// Trusted gateways attest the complete root; missing/invalid attestations
    /// fall back to transaction verification. No caller-declared sender/hash is used.
    pub fn authenticate(
        chain_id: u64,
        encoded: &[Bytes],
        attestations: &[FrameAttestation],
        gateways: Option<&FrameGateways>,
        limits: FrameLimits,
    ) -> Result<Self, FrameAdmissionError> {
        if encoded.is_empty() || encoded.len() > limits.max_transactions {
            return Err(FrameAdmissionError::TransactionCount);
        }
        if attestations.len() > MAX_FRAME_ATTESTATIONS {
            return Err(FrameAdmissionError::AttestationCount);
        }
        if gateways.is_some_and(|gateways| {
            gateways.min() > gateways.keys().len() || gateways.min() > MAX_FRAME_ATTESTATIONS
        }) {
            return Err(FrameAdmissionError::GatewayConfiguration);
        }
        let encoded_bytes = encoded
            .iter()
            .try_fold(0usize, |total, bytes| total.checked_add(bytes.len()))
            .filter(|total| *total <= limits.max_encoded_bytes)
            .ok_or(FrameAdmissionError::EncodedBytes)?;
        let mut decoded = Vec::with_capacity(encoded.len());
        let mut hashes = Vec::with_capacity(encoded.len());
        let mut unique = HashSet::with_capacity(encoded.len());
        for (index, bytes) in encoded.iter().enumerate() {
            let tx = AltSigTx::decode_2718_exact(bytes)
                .map_err(|_| FrameAdmissionError::Envelope(index))?;
            if tx.tx().chain_id != chain_id {
                return Err(FrameAdmissionError::Chain(index));
            }
            // Shape/canonical checks remain mandatory even on an attested frame.
            tx.tx()
                .ed25519_key()
                .map_err(|source| FrameAdmissionError::Signature { index, source })?;
            tx.ed25519_signature()
                .map_err(|source| FrameAdmissionError::Signature { index, source })?;
            if !unique.insert(*tx.hash()) {
                return Err(FrameAdmissionError::Duplicate(index));
            }
            hashes.push(*tx.hash());
            decoded.push(tx);
        }
        let root = frame_root(&hashes);
        let attested =
            gateways.is_some_and(|gateways| gateways.check(chain_id, root, attestations).attested);
        let senders = if attested {
            decoded.iter().map(AltSigTx::sender).collect::<Vec<_>>()
        } else {
            verify_batch(&decoded.iter().collect::<Vec<_>>())
                .into_iter()
                .enumerate()
                .map(|(index, sender)| {
                    sender.map_err(|source| FrameAdmissionError::Signature { index, source })
                })
                .collect::<Result<Vec<_>, _>>()?
        };
        let transactions = decoded
            .into_iter()
            .zip(senders)
            .map(|(transaction, sender)| AdmittedTransaction {
                transaction,
                sender,
            })
            .collect::<Vec<_>>()
            .into();
        Ok(Self {
            root,
            chain_id,
            encoded_bytes,
            transactions,
            authentication: if attested {
                FrameAuthentication::TrustedGateways
            } else {
                FrameAuthentication::TransactionSignatures
            },
        })
    }

    pub fn root(&self) -> B256 {
        self.root
    }
    pub fn chain_id(&self) -> u64 {
        self.chain_id
    }
    pub fn encoded_bytes(&self) -> usize {
        self.encoded_bytes
    }
    pub fn transactions(&self) -> &[AdmittedTransaction] {
        &self.transactions
    }
    pub fn authentication(&self) -> FrameAuthentication {
        self.authentication
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ALG_ED25519, TxAltSig};
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::U256;
    use ed25519_dalek::SigningKey;

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }
    fn signed(nonce: u64) -> AltSigTx {
        let key = key(1);
        TxAltSig {
            chain_id: 94,
            nonce,
            max_priority_fee_per_gas: 1,
            max_fee_per_gas: 2,
            gas_limit: 21_000,
            to: Address::repeat_byte(0x22),
            value: U256::from(1),
            input: Bytes::new(),
            access_list: Default::default(),
            alg_type: ALG_ED25519,
            pubkey: Bytes::copy_from_slice(key.verifying_key().as_bytes()),
        }
        .sign_ed25519(&key)
    }
    fn limits() -> FrameLimits {
        FrameLimits {
            max_transactions: 500,
            max_encoded_bytes: 1_048_576,
        }
    }
    fn encoded() -> Vec<Bytes> {
        [signed(0), signed(1)]
            .iter()
            .map(|tx| tx.encoded_2718().into())
            .collect()
    }
    fn admit(
        bytes: &[Bytes],
        attest: &[FrameAttestation],
        gates: Option<&FrameGateways>,
    ) -> Result<AdmittedFrame, FrameAdmissionError> {
        AdmittedFrame::authenticate(94, bytes, attest, gates, limits())
    }

    #[test]
    fn verified_frame_binds_root_and_sender_and_shares_storage() {
        let input = encoded();
        let frame = admit(&input, &[], None).unwrap();
        assert_eq!(
            frame.authentication(),
            FrameAuthentication::TransactionSignatures
        );
        assert_eq!(
            frame.root(),
            frame_root(&[*signed(0).hash(), *signed(1).hash()])
        );
        assert_eq!(frame.chain_id(), 94);
        assert_eq!(frame.encoded_bytes(), input.iter().map(|tx| tx.len()).sum());
        for tx in frame.transactions() {
            assert_eq!(tx.sender(), tx.transaction().verify().unwrap());
        }
        let shared = frame.clone();
        assert!(Arc::ptr_eq(&frame.transactions, &shared.transactions));
    }

    #[test]
    fn trusted_gateway_attests_the_actual_root() {
        let input = encoded();
        let root = admit(&input, &[], None).unwrap().root();
        let gateway = key(2);
        let gates = FrameGateways::new(vec![gateway.verifying_key()], 1);
        let attestation = FrameAttestation::sign(&gateway, 94, root);
        let frame = admit(&input, &[attestation], Some(&gates)).unwrap();
        assert_eq!(frame.authentication(), FrameAuthentication::TrustedGateways);
        assert_eq!(frame.root(), root);
        for tx in frame.transactions() {
            assert_eq!(tx.sender(), tx.transaction().verify().unwrap());
        }
    }

    #[test]
    fn absent_bad_wrong_chain_wrong_root_and_untrusted_attestations_fall_back() {
        let input = encoded();
        let gateway = key(2);
        let gates = FrameGateways::new(vec![gateway.verifying_key()], 1);
        let root = admit(&input, &[], None).unwrap().root();
        let mut bad = FrameAttestation::sign(&gateway, 94, root);
        bad.signature[0] ^= 1;
        for attest in [
            vec![],
            vec![bad],
            vec![FrameAttestation::sign(&gateway, 95, root)],
            vec![FrameAttestation::sign(&gateway, 94, B256::ZERO)],
            vec![FrameAttestation::sign(&key(3), 94, root)],
        ] {
            assert_eq!(
                admit(&input, &attest, Some(&gates))
                    .unwrap()
                    .authentication(),
                FrameAuthentication::TransactionSignatures
            );
        }
    }

    #[test]
    fn insufficient_gateway_quorum_verifies_transactions() {
        let input = encoded();
        let gateway = key(2);
        let gates = FrameGateways::new(vec![gateway.verifying_key(), key(3).verifying_key()], 2);
        let root = admit(&input, &[], None).unwrap().root();
        let one = FrameAttestation::sign(&gateway, 94, root);
        assert_eq!(
            admit(&input, &[one, one], Some(&gates))
                .unwrap()
                .authentication(),
            FrameAuthentication::TransactionSignatures
        );
    }

    #[test]
    fn bad_transaction_cannot_hide_behind_a_stale_frame_attestation() {
        let gateway = key(2);
        let gates = FrameGateways::new(vec![gateway.verifying_key()], 1);
        let mut input = encoded();
        let root = admit(&input, &[], None).unwrap().root();
        let attest = FrameAttestation::sign(&gateway, 94, root);
        let original = signed(1);
        let mut tx = original.tx().clone();
        tx.value += U256::from(1);
        input[1] = AltSigTx::new(tx, original.signature().clone())
            .encoded_2718()
            .into();
        assert!(matches!(
            admit(&input, &[attest], Some(&gates)),
            Err(FrameAdmissionError::Signature { index: 1, .. })
        ));
    }

    #[test]
    fn impossible_gateway_quorum_is_rejected_before_allocation() {
        let gates = FrameGateways::new(vec![key(2).verifying_key()], usize::MAX);
        assert!(matches!(
            admit(&encoded(), &[], Some(&gates)),
            Err(FrameAdmissionError::GatewayConfiguration)
        ));
    }

    #[test]
    fn empty_count_byte_and_attestation_limits_are_enforced() {
        let input = encoded();
        assert!(matches!(
            admit(&[], &[], None),
            Err(FrameAdmissionError::TransactionCount)
        ));
        let run = |limit| AdmittedFrame::authenticate(94, &input, &[], None, limit);
        assert!(matches!(
            run(FrameLimits {
                max_transactions: 1,
                ..limits()
            }),
            Err(FrameAdmissionError::TransactionCount)
        ));
        assert!(matches!(
            run(FrameLimits {
                max_encoded_bytes: input.iter().map(|tx| tx.len()).sum::<usize>() - 1,
                ..limits()
            }),
            Err(FrameAdmissionError::EncodedBytes)
        ));
        let attest = FrameAttestation::sign(&key(2), 94, B256::ZERO);
        assert!(matches!(
            admit(&input, &[attest; MAX_FRAME_ATTESTATIONS + 1], None),
            Err(FrameAdmissionError::AttestationCount)
        ));
        assert!(
            run(FrameLimits {
                max_transactions: input.len(),
                max_encoded_bytes: input.iter().map(|tx| tx.len()).sum()
            })
            .is_ok()
        );
    }

    #[test]
    fn malformed_nonexact_foreign_and_duplicate_transactions_are_rejected() {
        let mut input = encoded();
        let mut trailing = input[0].to_vec();
        trailing.push(0);
        for bytes in [
            Bytes::from(trailing),
            Bytes::from_static(&[0x80]),
            Bytes::from_static(&[0x50, 0xc0]),
        ] {
            input[0] = bytes;
            assert!(matches!(
                admit(&input, &[], None),
                Err(FrameAdmissionError::Envelope(0))
            ));
        }
        assert!(matches!(
            AdmittedFrame::authenticate(95, &encoded(), &[], None, limits()),
            Err(FrameAdmissionError::Chain(0))
        ));
        let tx = encoded()[0].clone();
        assert!(matches!(
            admit(&[tx.clone(), tx], &[], None),
            Err(FrameAdmissionError::Duplicate(1))
        ));
    }
}
