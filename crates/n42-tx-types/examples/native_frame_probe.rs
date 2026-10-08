//! Native 0x50 frame admission costs. No execution, QMDB or canonical TPS.
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, Bytes, U256};
use ed25519_dalek::SigningKey;
use n42_tx_types::{
    ALG_ED25519, FrameAttestation, FrameGateways, TxAltSig,
    admission::{AdmittedFrame, FrameAuthentication, FrameLimits},
};
use std::{hint::black_box, time::Instant};

fn main() {
    let chain_id = 941007;
    let count = 500;
    let keys: Vec<_> = (1..=16)
        .map(|seed| SigningKey::from_bytes(&[seed; 32]))
        .collect();
    // Input creation and gateway attestation are outside all timed intervals.
    let encoded: Vec<Bytes> = (0..count)
        .map(|index| {
            let key = &keys[index % keys.len()];
            TxAltSig {
                chain_id,
                nonce: (index / keys.len()) as u64,
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
            .sign_ed25519(key)
            .encoded_2718()
            .into()
        })
        .collect();
    let limits = FrameLimits {
        max_transactions: count,
        max_encoded_bytes: 1_048_576,
    };
    let reference = AdmittedFrame::authenticate(chain_id, &encoded, &[], None, limits).unwrap();
    let gateway = SigningKey::from_bytes(&[99; 32]);
    let gateways = FrameGateways::new(vec![gateway.verifying_key()], 1);
    let attestations = [FrameAttestation::sign(&gateway, chain_id, reference.root())];
    let trusted =
        AdmittedFrame::authenticate(chain_id, &encoded, &attestations, Some(&gateways), limits)
            .unwrap();
    assert_eq!(
        trusted.authentication(),
        FrameAuthentication::TrustedGateways
    );
    assert_eq!(reference.root(), trusted.root());
    for (left, right) in reference.transactions().iter().zip(trusted.transactions()) {
        assert_eq!(left.transaction(), right.transaction());
        assert_eq!(left.sender(), right.sender());
    }
    for tag in ["warmup", "a1", "b", "a2"] {
        for iteration in 0..10 {
            let started = Instant::now();
            let frame = AdmittedFrame::authenticate(
                chain_id,
                &encoded,
                if tag == "b" { &attestations } else { &[] },
                if tag == "b" { Some(&gateways) } else { None },
                limits,
            )
            .unwrap();
            let duration = started.elapsed().as_nanos();
            assert_eq!(frame.root(), reference.root());
            assert_eq!(frame.transactions().len(), count);
            assert_eq!(
                frame.authentication(),
                if tag == "b" {
                    FrameAuthentication::TrustedGateways
                } else {
                    FrameAuthentication::TransactionSignatures
                }
            );
            black_box(&frame);
            println!(
                "NATIVE_FRAME_PROBE {{\"tag\":\"{tag}\",\"iteration\":{iteration},\"transactions\":{count},\"duration_ns\":{duration}}}"
            );
        }
    }
}
