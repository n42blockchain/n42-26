//! Chain-bound signing domains for the cross-client H2-v4 protocol.

use crate::h2_wire::{H2Message, H2WireError, decode_message, encode_message};
use alloy_primitives::B256;

const PREFIX: &[u8; 7] = b"N42H2V4";
const MAX_WIRE_SIZE: usize = 8192;
const HEADER_SIZE: usize = 7 + 8 + 32 + 32 + 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct H2V4ChainIdentity {
    pub chain_id: u64,
    pub genesis_hash: B256,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct H2V4Envelope {
    pub identity: H2V4ChainIdentity,
    pub changes_hash: B256,
    pub message: H2Message,
}

#[derive(Debug, thiserror::Error)]
pub enum H2V4Error {
    #[error("truncated H2-v4 envelope")]
    Truncated,
    #[error("invalid H2-v4 magic")]
    InvalidMagic,
    #[error("H2-v4 chain identity mismatch")]
    ChainIdentityMismatch,
    #[error("invalid H2-v4 message length")]
    InvalidLength,
    #[error("invalid H2-v4 snappy payload: {0}")]
    InvalidSnappy(String),
    #[error(transparent)]
    Wire(#[from] H2WireError),
}

pub fn encode_envelope(envelope: &H2V4Envelope) -> Result<Vec<u8>, H2V4Error> {
    let wire = encode_message(&envelope.message)?;
    if wire.len() > MAX_WIRE_SIZE {
        return Err(H2V4Error::InvalidLength);
    }
    let mut out = Vec::with_capacity(HEADER_SIZE + wire.len());
    out.extend_from_slice(PREFIX);
    out.extend_from_slice(&envelope.identity.chain_id.to_le_bytes());
    out.extend_from_slice(envelope.identity.genesis_hash.as_slice());
    out.extend_from_slice(envelope.changes_hash.as_slice());
    out.extend_from_slice(&(wire.len() as u32).to_le_bytes());
    out.extend_from_slice(&wire);
    Ok(out)
}

pub fn decode_envelope(
    data: &[u8],
    expected: H2V4ChainIdentity,
) -> Result<H2V4Envelope, H2V4Error> {
    if data.len() < HEADER_SIZE {
        return Err(H2V4Error::Truncated);
    }
    if &data[..7] != PREFIX {
        return Err(H2V4Error::InvalidMagic);
    }
    let chain_id = u64::from_le_bytes(data[7..15].try_into().expect("fixed range"));
    let genesis_hash = B256::from_slice(&data[15..47]);
    let identity = H2V4ChainIdentity {
        chain_id,
        genesis_hash,
    };
    if identity != expected {
        return Err(H2V4Error::ChainIdentityMismatch);
    }
    let changes_hash = B256::from_slice(&data[47..79]);
    let wire_len = u32::from_le_bytes(data[79..83].try_into().expect("fixed range")) as usize;
    if wire_len > MAX_WIRE_SIZE || wire_len != data.len() - HEADER_SIZE {
        return Err(H2V4Error::InvalidLength);
    }
    let message = decode_message(&data[HEADER_SIZE..])?;
    Ok(H2V4Envelope {
        identity,
        changes_hash,
        message,
    })
}

pub fn encode_gossip(envelope: &H2V4Envelope) -> Result<Vec<u8>, H2V4Error> {
    let wire = encode_envelope(envelope)?;
    snap::raw::Encoder::new()
        .compress_vec(&wire)
        .map_err(|error| H2V4Error::InvalidSnappy(error.to_string()))
}

pub fn decode_gossip(data: &[u8], expected: H2V4ChainIdentity) -> Result<H2V4Envelope, H2V4Error> {
    let len = snap::raw::decompress_len(data)
        .map_err(|error| H2V4Error::InvalidSnappy(error.to_string()))?;
    if len > HEADER_SIZE + MAX_WIRE_SIZE {
        return Err(H2V4Error::InvalidLength);
    }
    let wire = snap::raw::Decoder::new()
        .decompress_vec(data)
        .map_err(|error| H2V4Error::InvalidSnappy(error.to_string()))?;
    decode_envelope(&wire, expected)
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

    #[derive(Deserialize)]
    struct EnvelopeFixture {
        schema: String,
        envelope_hex: String,
        gossip_hex: String,
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

    #[test]
    fn decodes_and_reencodes_gov5_v4_envelope_and_snappy() {
        let fixture: EnvelopeFixture =
            serde_json::from_str(include_str!("../testdata/h2_v4_envelope_v1.json")).unwrap();
        assert_eq!(fixture.schema, "n42-h2-v4-envelope-v1");
        let identity = H2V4ChainIdentity {
            chain_id: 94,
            genesis_hash: B256::repeat_byte(0x11),
        };
        let wire = hex::decode(fixture.envelope_hex).unwrap();
        let envelope = decode_envelope(&wire, identity).unwrap();
        assert_eq!(envelope.changes_hash, B256::repeat_byte(0x33));
        assert_eq!(envelope.message.kind() as u8, 3);
        assert_eq!(encode_envelope(&envelope).unwrap(), wire);

        let gossip = hex::decode(fixture.gossip_hex).unwrap();
        assert_eq!(decode_gossip(&gossip, identity).unwrap(), envelope);
        let mut wrong = identity;
        wrong.chain_id += 1;
        assert!(matches!(
            decode_envelope(&wire, wrong),
            Err(H2V4Error::ChainIdentityMismatch)
        ));
        let mut trailing = wire;
        trailing.push(0);
        assert!(matches!(
            decode_envelope(&trailing, identity),
            Err(H2V4Error::InvalidLength)
        ));
    }
}
