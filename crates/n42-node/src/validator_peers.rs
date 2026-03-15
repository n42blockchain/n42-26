use n42_chainspec::ValidatorInfo;
use n42_network::{PeerId, deterministic_validator_peer_id};
use std::collections::HashMap;

pub fn configured_validator_peer_ids(
    validators: &[ValidatorInfo],
) -> eyre::Result<HashMap<u32, PeerId>> {
    let mut peer_ids = HashMap::new();
    for (index, validator) in validators.iter().enumerate() {
        if let Some(peer_id) = validator
            .parsed_p2p_peer_id()
            .map_err(|error| eyre::eyre!(error))?
        {
            peer_ids.insert(index as u32, peer_id);
        }
    }
    Ok(peer_ids)
}

pub fn expected_validator_peer_ids(
    validators: &[ValidatorInfo],
) -> eyre::Result<HashMap<u32, PeerId>> {
    expected_validator_peer_ids_with_policy(validators, true)
}

pub fn expected_validator_peer_ids_with_policy(
    validators: &[ValidatorInfo],
    allow_deterministic_fallback: bool,
) -> eyre::Result<HashMap<u32, PeerId>> {
    let configured = configured_validator_peer_ids(validators)?;
    if !configured.is_empty() {
        return Ok(configured);
    }

    if !allow_deterministic_fallback && validators.len() > 1 {
        return Err(eyre::eyre!(
            "multi-validator peer authentication requires explicit p2p_peer_id for every validator; set N42_ALLOW_DETERMINISTIC_P2P=1 only for non-production compatibility"
        ));
    }

    let mut peer_ids = HashMap::with_capacity(validators.len());
    for index in 0..validators.len() {
        peer_ids.insert(index as u32, deterministic_validator_peer_id(index as u32)?);
    }
    Ok(peer_ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Address;
    use libp2p::identity::Keypair;
    use n42_primitives::BlsSecretKey;

    fn make_validator(peer_id: Option<String>) -> ValidatorInfo {
        let sk = BlsSecretKey::random().unwrap();
        ValidatorInfo {
            address: Address::random(),
            bls_public_key: sk.public_key(),
            p2p_peer_id: peer_id,
        }
    }

    #[test]
    fn test_expected_validator_peer_ids_uses_explicit_bindings() {
        let peer0 = Keypair::generate_ed25519().public().to_peer_id();
        let peer1 = Keypair::generate_ed25519().public().to_peer_id();
        let validators = vec![
            make_validator(Some(peer0.to_string())),
            make_validator(Some(peer1.to_string())),
        ];

        let expected = expected_validator_peer_ids(&validators).unwrap();

        assert_eq!(expected.get(&0), Some(&peer0));
        assert_eq!(expected.get(&1), Some(&peer1));
    }

    #[test]
    fn test_expected_validator_peer_ids_falls_back_to_deterministic_bindings() {
        let validators = vec![make_validator(None), make_validator(None)];

        let expected = expected_validator_peer_ids(&validators).unwrap();

        assert_eq!(expected.len(), 2);
        assert_eq!(
            expected.get(&0),
            Some(&deterministic_validator_peer_id(0).unwrap())
        );
        assert_eq!(
            expected.get(&1),
            Some(&deterministic_validator_peer_id(1).unwrap())
        );
    }

    #[test]
    fn test_expected_validator_peer_ids_with_policy_rejects_strict_multi_validator_fallback() {
        let validators = vec![make_validator(None), make_validator(None)];

        let error = expected_validator_peer_ids_with_policy(&validators, false).unwrap_err();

        assert!(error.to_string().contains("requires explicit p2p_peer_id"));
    }
}
