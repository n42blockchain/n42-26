//! Chain-bound signing domains for the cross-client H2-v4 protocol.

use alloy_primitives::B256;

const PREFIX: &[u8; 7] = b"N42H2V4";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct H2V4ChainIdentity {
    pub chain_id: u64,
    pub genesis_hash: B256,
}

#[derive(Clone, Copy)]
#[repr(u8)]
enum Phase {
    Proposal = 1,
    Vote = 2,
    Commit = 3,
    Timeout = 4,
    NewView = 5,
}

fn base(identity: H2V4ChainIdentity, phase: Phase, view: u64) -> [u8; 56] {
    let mut out = [0u8; 56];
    out[..7].copy_from_slice(PREFIX);
    out[7] = phase as u8;
    out[8..16].copy_from_slice(&identity.chain_id.to_le_bytes());
    out[16..48].copy_from_slice(identity.genesis_hash.as_slice());
    out[48..56].copy_from_slice(&view.to_le_bytes());
    out
}

pub fn proposal_signing_message(
    identity: H2V4ChainIdentity,
    view: u64,
    block_hash: B256,
    changes_hash: B256,
) -> [u8; 120] {
    let mut out = [0u8; 120];
    out[..56].copy_from_slice(&base(identity, Phase::Proposal, view));
    out[56..88].copy_from_slice(block_hash.as_slice());
    out[88..].copy_from_slice(changes_hash.as_slice());
    out
}

pub fn vote_signing_message(identity: H2V4ChainIdentity, view: u64, block_hash: B256) -> [u8; 88] {
    let mut out = [0u8; 88];
    out[..56].copy_from_slice(&base(identity, Phase::Vote, view));
    out[56..].copy_from_slice(block_hash.as_slice());
    out
}

pub fn commit_signing_message(
    identity: H2V4ChainIdentity,
    view: u64,
    block_hash: B256,
    changes_hash: B256,
) -> [u8; 120] {
    let mut out = [0u8; 120];
    out[..56].copy_from_slice(&base(identity, Phase::Commit, view));
    out[56..88].copy_from_slice(block_hash.as_slice());
    out[88..].copy_from_slice(changes_hash.as_slice());
    out
}

pub fn timeout_signing_message(identity: H2V4ChainIdentity, view: u64) -> [u8; 56] {
    base(identity, Phase::Timeout, view)
}

pub fn new_view_signing_message(identity: H2V4ChainIdentity, view: u64) -> [u8; 56] {
    base(identity, Phase::NewView, view)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct Fixture {
        schema: String,
        chain_id: u64,
        genesis_hash: String,
        view: u64,
        block_hash: String,
        changes_hash: String,
        proposal_hex: String,
        vote_hex: String,
        commit_hex: String,
        timeout_hex: String,
        new_view_hex: String,
    }

    #[test]
    fn matches_gov5_domains_and_binds_chain_identity() {
        let fixture: Fixture =
            serde_json::from_str(include_str!("../testdata/h2_v4_domains_v1.json")).unwrap();
        assert_eq!(fixture.schema, "n42-h2-v4-domains-v1");
        let identity = H2V4ChainIdentity {
            chain_id: fixture.chain_id,
            genesis_hash: B256::from_slice(&hex::decode(fixture.genesis_hash).unwrap()),
        };
        let block_hash = B256::from_slice(&hex::decode(fixture.block_hash).unwrap());
        let changes_hash = B256::from_slice(&hex::decode(fixture.changes_hash).unwrap());
        assert_eq!(
            hex::encode(proposal_signing_message(
                identity,
                fixture.view,
                block_hash,
                changes_hash
            )),
            fixture.proposal_hex
        );
        assert_eq!(
            hex::encode(vote_signing_message(identity, fixture.view, block_hash)),
            fixture.vote_hex
        );
        assert_eq!(
            hex::encode(commit_signing_message(
                identity,
                fixture.view,
                block_hash,
                changes_hash
            )),
            fixture.commit_hex
        );
        assert_eq!(
            hex::encode(timeout_signing_message(identity, fixture.view)),
            fixture.timeout_hex
        );
        assert_eq!(
            hex::encode(new_view_signing_message(identity, fixture.view)),
            fixture.new_view_hex
        );

        let mut other = identity;
        other.chain_id += 1;
        assert_ne!(
            commit_signing_message(identity, fixture.view, block_hash, changes_hash),
            commit_signing_message(other, fixture.view, block_hash, changes_hash)
        );
    }
}
