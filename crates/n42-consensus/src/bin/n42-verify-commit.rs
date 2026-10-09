//! Offline commit-QC verifier. Caller must authenticate the expected profile,
//! chain identity, validator roster, fault tolerance and validator-changes hash.
use alloy_primitives::{Address, B256};
use bitvec::prelude::*;
use n42_consensus::{
    protocol::quorum::{ConsensusSigningProfile, verify_commit_qc_with_profile},
    validator::ValidatorSet,
};
use n42_primitives::{
    BlsPublicKey, BlsSignature,
    consensus::{H2V4ChainIdentity, QuorumCertificate},
};
use serde_json::{Value, json};
use std::{collections::HashSet, io::Read};

fn integer(value: &Value, field: &str) -> Result<u64, String> {
    value[field]
        .as_u64()
        .ok_or_else(|| format!("invalid {field}"))
}

fn fixed_bytes<const N: usize>(value: &Value) -> Result<[u8; N], String> {
    let text = value.as_str().ok_or("expected hexadecimal string")?;
    let bytes = hex::decode(text.strip_prefix("0x").unwrap_or(text)).map_err(|e| e.to_string())?;
    bytes.try_into().map_err(|_| format!("expected {N} bytes"))
}

fn verify(input: &Value) -> Result<Value, String> {
    let profile_name = input["profile"].as_str().ok_or("missing signing profile")?;
    let changes = B256::from(fixed_bytes::<32>(&input["validatorChangesHash"])?);
    let profile = match profile_name {
        "native" => ConsensusSigningProfile::Native,
        "h2v4" => ConsensusSigningProfile::H2V4(H2V4ChainIdentity {
            chain_id: integer(input, "chainId")?,
            genesis_hash: B256::from(fixed_bytes::<32>(&input["genesisHash"])?),
        }),
        "gov5legacy" => ConsensusSigningProfile::Gov5Legacy,
        _ => return Err("unsupported signing profile".into()),
    };
    if profile.is_gov5() && changes != B256::ZERO {
        return Err("Gov5 profiles require the protocol's zero changes hash".into());
    }
    let keys = input["validators"]
        .as_array()
        .ok_or("missing validator roster")?;
    if !(4..=1024).contains(&keys.len()) {
        return Err("validator roster must contain 4..=1024 keys".into());
    }
    let mut unique = HashSet::new();
    let mut roster_hash = blake3::Hasher::new();
    let mut validators = Vec::with_capacity(keys.len());
    for key in keys {
        let bytes = fixed_bytes::<48>(key)?;
        if !unique.insert(bytes) {
            return Err("duplicate validator public key".into());
        }
        roster_hash.update(&bytes);
        validators.push(n42_chainspec::ValidatorInfo {
            address: Address::ZERO,
            bls_public_key: BlsPublicKey::from_bytes(&bytes).map_err(|e| e.to_string())?,
            p2p_peer_id: None,
        });
    }
    let fault_tolerance = u32::try_from(integer(input, "faultTolerance")?)
        .map_err(|_| "fault tolerance exceeds u32")?;
    let validator_set =
        ValidatorSet::try_new(&validators, fault_tolerance).map_err(|e| e.to_string())?;
    let qc = &input["qc"];
    let view = integer(qc, "view")?;
    let hash = B256::from(fixed_bytes::<32>(&qc["blockHash"])?);
    if view == 0
        || view != integer(input, "expectedView")?
        || hash != B256::from(fixed_bytes::<32>(&input["expectedBlockHash"])?)
    {
        return Err("QC does not bind the expected non-genesis view/block".into());
    }
    let bitmap = qc["signers"].as_array().ok_or("missing signer bitmap")?;
    if bitmap.len() != validators.len() {
        return Err("signer bitmap length differs from validator roster".into());
    }
    let signers: BitVec<u8, Msb0> = bitmap
        .iter()
        .map(|bit| bit.as_bool().ok_or("non-boolean signer bit"))
        .collect::<Result<_, _>>()?;
    let certificate = QuorumCertificate {
        view,
        block_hash: hash,
        signers,
        aggregate_signature: BlsSignature::from_bytes(&fixed_bytes::<96>(&qc["signature"])?)
            .map_err(|e| e.to_string())?,
    };
    // Commit only. Never use the prepare-or-commit fallback verification API.
    verify_commit_qc_with_profile(&certificate, &validator_set, &changes, profile)
        .map_err(|e| e.to_string())?;
    Ok(json!({"schema":1, "verified":true, "profile":profile_name,
        "chainBoundSignature": matches!(profile, ConsensusSigningProfile::H2V4(_)),
        "view":view, "blockHash":hash, "validatorCount":validators.len(),
        "faultTolerance":fault_tolerance, "signerCount":certificate.signer_count(),
        "validatorSetHash":format!("0x{}",roster_hash.finalize().to_hex()),
        "validatorChangesHash":changes,
        "signingMessage":format!("0x{}", hex::encode(profile.commit_message(view,hash,changes)))}))
}

fn main() {
    let result = (|| {
        if std::env::args_os().len() != 1 {
            return Err("reads one JSON request from stdin; takes no arguments".into());
        }
        let mut bytes = Vec::new();
        std::io::stdin()
            .take(1_048_577)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        if bytes.len() > 1_048_576 {
            return Err("QC request exceeds 1 MiB".into());
        }
        verify(&serde_json::from_slice(&bytes).map_err(|e| e.to_string())?)
    })();
    match result {
        Ok(value) => println!("{value}"),
        Err(error) => {
            println!("{}", json!({"schema":1, "verified":false, "error":error}));
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use n42_primitives::{BlsSecretKey, bls::AggregateSignature};

    fn fixture(name: &str, prepare: bool) -> Value {
        let identity = H2V4ChainIdentity {
            chain_id: 94,
            genesis_hash: B256::repeat_byte(0x11),
        };
        let profile = match name {
            "native" => ConsensusSigningProfile::Native,
            "gov5legacy" => ConsensusSigningProfile::Gov5Legacy,
            _ => ConsensusSigningProfile::H2V4(identity),
        };
        let hash = B256::repeat_byte(0x22);
        let keys: Vec<_> = (0..4u8)
            .map(|n| BlsSecretKey::key_gen(&[n + 100; 32]).unwrap())
            .collect();
        let message = if prepare {
            profile.vote_message(7, hash)
        } else {
            profile.commit_message(7, hash, B256::ZERO)
        };
        let signatures: Vec<_> = keys[..3]
            .iter()
            .map(|sk| {
                if name == "native" {
                    sk.sign(&message)
                } else {
                    sk.sign_h2_v4(&message)
                }
            })
            .collect();
        let aggregate =
            AggregateSignature::aggregate(&signatures.iter().collect::<Vec<_>>()).unwrap();
        json!({"profile":name,"chainId":94,"genesisHash":identity.genesis_hash,
            "faultTolerance":1,"validatorChangesHash":B256::ZERO,"expectedView":7,"expectedBlockHash":hash,
            "validators":keys.iter().map(|sk|hex::encode(sk.public_key().to_bytes())).collect::<Vec<_>>(),
            "qc":{"view":7,"blockHash":hash,"signature":hex::encode(aggregate.to_bytes()),"signers":[true,true,true,false]}})
    }

    #[test]
    #[ignore = "explicit synthetic TPS certificate export; never used by the production CLI"]
    fn export_tps_fixtures() {
        let input = std::env::var("N42_TPS_FIXTURE_CONTEXTS").unwrap();
        let output = std::env::var("N42_TPS_FIXTURE_OUTPUT").unwrap();
        let contexts: Vec<Value> = serde_json::from_slice(&std::fs::read(input).unwrap()).unwrap();
        let mut results = Vec::new();
        for context in contexts {
            let mut request = fixture("h2v4", false);
            request["genesisHash"] = context["genesisHash"].clone();
            request["expectedView"] = context["expectedView"].clone();
            request["expectedBlockHash"] = context["expectedBlockHash"].clone();
            request["qc"]["view"] = request["expectedView"].clone();
            request["qc"]["blockHash"] = request["expectedBlockHash"].clone();
            let profile = ConsensusSigningProfile::H2V4(H2V4ChainIdentity {
                chain_id: 94,
                genesis_hash: B256::from(fixed_bytes::<32>(&request["genesisHash"]).unwrap()),
            });
            let message = profile.commit_message(
                integer(&request, "expectedView").unwrap(),
                B256::from(fixed_bytes::<32>(&request["expectedBlockHash"]).unwrap()),
                B256::ZERO,
            );
            let signatures: Vec<_> = (0..3u8)
                .map(|n| {
                    BlsSecretKey::key_gen(&[n + 100; 32])
                        .unwrap()
                        .sign_h2_v4(&message)
                })
                .collect();
            let aggregate =
                AggregateSignature::aggregate(&signatures.iter().collect::<Vec<_>>()).unwrap();
            request["qc"]["signature"] = json!(hex::encode(aggregate.to_bytes()));
            assert!(verify(&request).is_ok());
            results.push(request);
        }
        std::fs::write(output, serde_json::to_vec_pretty(&results).unwrap()).unwrap();
    }

    #[test]
    fn accepts_each_exact_commit_domain_and_rejects_prepare_signatures() {
        for name in ["native", "h2v4", "gov5legacy"] {
            assert_eq!(verify(&fixture(name, false)).unwrap()["signerCount"], 3);
            assert!(verify(&fixture(name, true)).is_err());
        }
    }

    #[test]
    fn seven_validator_h2v4_commit_requires_five_signers() {
        let identity = H2V4ChainIdentity {
            chain_id: 94,
            genesis_hash: B256::repeat_byte(0x11),
        };
        let profile = ConsensusSigningProfile::H2V4(identity);
        let hash = B256::repeat_byte(0x22);
        let keys: Vec<_> = (0..7u8)
            .map(|n| BlsSecretKey::key_gen(&[n + 100; 32]).unwrap())
            .collect();
        let message = profile.commit_message(7, hash, B256::ZERO);
        let signatures: Vec<_> = keys[..5]
            .iter()
            .map(|key| key.sign_h2_v4(&message))
            .collect();
        let aggregate =
            AggregateSignature::aggregate(&signatures.iter().collect::<Vec<_>>()).unwrap();
        let request = json!({"profile":"h2v4","chainId":94,"genesisHash":identity.genesis_hash,
            "faultTolerance":2,"validatorChangesHash":B256::ZERO,"expectedView":7,"expectedBlockHash":hash,
            "validators":keys.iter().map(|key|hex::encode(key.public_key().to_bytes())).collect::<Vec<_>>(),
            "qc":{"view":7,"blockHash":hash,"signature":hex::encode(aggregate.to_bytes()),
                  "signers":[true,true,true,true,true,false,false]}});
        assert_eq!(verify(&request).unwrap()["signerCount"], 5);
        let mut insufficient = request.clone();
        let four =
            AggregateSignature::aggregate(&signatures[..4].iter().collect::<Vec<_>>()).unwrap();
        insufficient["qc"]["signature"] = json!(hex::encode(four.to_bytes()));
        insufficient["qc"]["signers"] = json!([true, true, true, true, false, false, false]);
        assert!(verify(&insufficient).is_err());
        let mut wrong_fault_tolerance = request;
        wrong_fault_tolerance["faultTolerance"] = json!(1);
        assert!(verify(&wrong_fault_tolerance).is_err());
    }

    #[test]
    fn rejects_wrong_identity_phase_roster_and_quorum() {
        let base = fixture("h2v4", false);
        for (field, value) in [
            ("chainId", json!(95)),
            ("genesisHash", json!(B256::ZERO)),
            ("expectedView", json!(8)),
            ("expectedBlockHash", json!(B256::ZERO)),
            ("profile", json!("gov5legacy")),
            ("faultTolerance", json!(2)),
            ("validatorChangesHash", json!(B256::repeat_byte(1))),
        ] {
            let mut bad = base.clone();
            bad[field] = value;
            assert!(verify(&bad).is_err(), "{field}");
        }
        for bitmap in [
            json!([true, true, false, false]),
            json!([true, true, true]),
            json!([true, true, true, false, true]),
            json!([1, true, true, false]),
        ] {
            let mut bad = base.clone();
            bad["qc"]["signers"] = bitmap;
            assert!(verify(&bad).is_err());
        }
        let mut bad = base.clone();
        bad["validators"][3] = bad["validators"][0].clone();
        assert!(verify(&bad).is_err());
        let mut bad = base.clone();
        bad["validators"].as_array_mut().unwrap().swap(0, 3);
        assert!(verify(&bad).is_err());
        let mut bad = base.clone();
        bad["qc"]["signature"] = json!("00".repeat(96));
        assert!(verify(&bad).is_err());
        let mut bad = base;
        bad["expectedView"] = json!(0);
        bad["qc"]["view"] = json!(0);
        assert!(verify(&bad).is_err());
    }

    #[test]
    fn native_commit_binds_validator_changes() {
        let mut bad = fixture("native", false);
        bad["validatorChangesHash"] = json!(B256::repeat_byte(1));
        assert!(verify(&bad).is_err());
    }

    #[test]
    #[ignore = "write explicitly requested synthetic verifier fixtures"]
    fn export_synthetic_fixtures() {
        let directory = std::path::PathBuf::from(std::env::var("N42_COMMIT_FIXTURE_DIR").unwrap());
        std::fs::create_dir_all(&directory).unwrap();
        for name in ["native", "h2v4", "gov5legacy"] {
            for prepare in [false, true] {
                let phase = if prepare { "prepare" } else { "commit" };
                std::fs::write(
                    directory.join(format!("{name}-{phase}.json")),
                    serde_json::to_vec_pretty(&fixture(name, prepare)).unwrap(),
                )
                .unwrap();
            }
        }
    }
}
