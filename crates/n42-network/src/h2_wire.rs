//! Isolated compatibility codec for gov5's current HotStuff-2 envelope.
//!
//! Production Rust gossip remains versioned bincode. This module pins the Go
//! byte format before the shared H2-v4 migration; it does not enable peering.

use alloy_primitives::B256;
use n42_primitives::BlsSignature;

// Matches gov5 encoder.MaxGossipSize. Individual message decoders impose
// tighter field limits where their schema permits it.
const MAX_ENVELOPE_PAYLOAD: usize = 1 << 20;
const MAX_HIGH_TC: usize = 4 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum H2MessageKind {
    Proposal = 1,
    Vote = 2,
    CommitVote = 3,
    PrepareQc = 4,
    Timeout = 5,
    NewView = 6,
    Decide = 7,
}

impl TryFrom<u8> for H2MessageKind {
    type Error = H2WireError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Proposal),
            2 => Ok(Self::Vote),
            3 => Ok(Self::CommitVote),
            4 => Ok(Self::PrepareQc),
            5 => Ok(Self::Timeout),
            6 => Ok(Self::NewView),
            7 => Ok(Self::Decide),
            _ => Err(H2WireError::UnknownMessageKind(value)),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct H2Envelope {
    pub kind: H2MessageKind,
    pub payload: Vec<u8>,
}

/// Round-1 vote as encoded by gov5. `high_tc` remains opaque until both
/// clients adopt the common H2-v4 timeout-certificate schema.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct H2Vote {
    pub view: u64,
    pub block_hash: B256,
    pub voter: u32,
    pub signature: BlsSignature,
    pub high_tc: Option<Vec<u8>>,
}

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum H2WireError {
    #[error("H2 message is truncated")]
    Truncated,
    #[error("H2 message has trailing bytes")]
    TrailingBytes,
    #[error("H2 envelope payload is {actual} bytes; maximum is {max}")]
    EnvelopeTooLarge { actual: usize, max: usize },
    #[error("H2 byte field is {actual} bytes; maximum is {max}")]
    FieldTooLarge { actual: usize, max: usize },
    #[error("unknown H2 message kind {0}")]
    UnknownMessageKind(u8),
    #[error("expected H2 Vote envelope, got {0:?}")]
    WrongMessageKind(H2MessageKind),
    #[error("H2 block hash must be 32 bytes, got {0}")]
    InvalidBlockHashLength(usize),
    #[error("H2 BLS signature must be 96 bytes, got {0}")]
    InvalidSignatureLength(usize),
    #[error("invalid H2 BLS signature")]
    InvalidSignature,
}

/// Decode gov5's `type || payload_len_le || payload` envelope.
pub fn decode_envelope(data: &[u8]) -> Result<H2Envelope, H2WireError> {
    let (&kind, rest) = data.split_first().ok_or(H2WireError::Truncated)?;
    let (payload, consumed) = read_bytes(rest, MAX_ENVELOPE_PAYLOAD)?;
    if consumed != rest.len() {
        return Err(H2WireError::TrailingBytes);
    }
    Ok(H2Envelope {
        kind: kind.try_into()?,
        payload: payload.to_vec(),
    })
}

/// Decode a current gov5 Round-1 vote. Callers must separately negotiate the
/// legacy codec and validate the H2 signature domain.
pub fn decode_vote(data: &[u8]) -> Result<H2Vote, H2WireError> {
    let envelope = decode_envelope(data)?;
    if envelope.kind != H2MessageKind::Vote {
        return Err(H2WireError::WrongMessageKind(envelope.kind));
    }
    let mut offset = 0;
    let view = read_u64(&envelope.payload, &mut offset)?;
    let block_hash_bytes = read_bytes_at(&envelope.payload, &mut offset, 32)?;
    if block_hash_bytes.len() != 32 {
        return Err(H2WireError::InvalidBlockHashLength(block_hash_bytes.len()));
    }
    let block_hash = B256::from_slice(block_hash_bytes);
    let voter = read_u32(&envelope.payload, &mut offset)?;
    let signature_bytes = read_bytes_at(&envelope.payload, &mut offset, 96)?;
    if signature_bytes.len() != 96 {
        return Err(H2WireError::InvalidSignatureLength(signature_bytes.len()));
    }
    let mut signature_array = [0u8; 96];
    signature_array.copy_from_slice(signature_bytes);
    let signature =
        BlsSignature::from_bytes(&signature_array).map_err(|_| H2WireError::InvalidSignature)?;
    // Older gov5 messages can omit this trailing field; new encoders write a
    // zero-length one. Preserve either representation.
    let high_tc = if offset == envelope.payload.len() {
        None
    } else {
        let bytes = read_bytes_at(&envelope.payload, &mut offset, MAX_HIGH_TC)?;
        (!bytes.is_empty()).then(|| bytes.to_vec())
    };
    if offset != envelope.payload.len() {
        return Err(H2WireError::TrailingBytes);
    }
    Ok(H2Vote {
        view,
        block_hash,
        voter,
        signature,
        high_tc,
    })
}

/// Encode the exact current gov5 H2 Vote envelope for Phase-0 vector parity.
pub fn encode_vote(vote: &H2Vote) -> Result<Vec<u8>, H2WireError> {
    let mut payload = Vec::with_capacity(8 + 36 + 4 + 100 + 4);
    payload.extend_from_slice(&vote.view.to_le_bytes());
    put_bytes(&mut payload, vote.block_hash.as_slice())?;
    payload.extend_from_slice(&vote.voter.to_le_bytes());
    put_bytes(&mut payload, &vote.signature.to_bytes())?;
    put_bytes(&mut payload, vote.high_tc.as_deref().unwrap_or_default())?;
    if payload.len() > MAX_ENVELOPE_PAYLOAD {
        return Err(H2WireError::EnvelopeTooLarge {
            actual: payload.len(),
            max: MAX_ENVELOPE_PAYLOAD,
        });
    }
    let mut out = Vec::with_capacity(1 + 4 + payload.len());
    out.push(H2MessageKind::Vote as u8);
    put_bytes(&mut out, &payload)?;
    Ok(out)
}

fn read_u64(data: &[u8], offset: &mut usize) -> Result<u64, H2WireError> {
    let bytes = take(data, offset, 8)?;
    Ok(u64::from_le_bytes(
        bytes.try_into().expect("length checked"),
    ))
}

fn read_u32(data: &[u8], offset: &mut usize) -> Result<u32, H2WireError> {
    let bytes = take(data, offset, 4)?;
    Ok(u32::from_le_bytes(
        bytes.try_into().expect("length checked"),
    ))
}

fn read_bytes<'a>(data: &'a [u8], max: usize) -> Result<(&'a [u8], usize), H2WireError> {
    if data.len() < 4 {
        return Err(H2WireError::Truncated);
    }
    let len = u32::from_le_bytes(data[..4].try_into().expect("length checked")) as usize;
    if len > max {
        return Err(H2WireError::FieldTooLarge { actual: len, max });
    }
    let end = 4usize.checked_add(len).ok_or(H2WireError::Truncated)?;
    let bytes = data.get(4..end).ok_or(H2WireError::Truncated)?;
    Ok((bytes, end))
}

fn read_bytes_at<'a>(
    data: &'a [u8],
    offset: &mut usize,
    max: usize,
) -> Result<&'a [u8], H2WireError> {
    let (bytes, consumed) = read_bytes(data.get(*offset..).ok_or(H2WireError::Truncated)?, max)?;
    *offset = (*offset)
        .checked_add(consumed)
        .ok_or(H2WireError::Truncated)?;
    Ok(bytes)
}

fn take<'a>(data: &'a [u8], offset: &mut usize, len: usize) -> Result<&'a [u8], H2WireError> {
    let end = (*offset).checked_add(len).ok_or(H2WireError::Truncated)?;
    let bytes = data.get(*offset..end).ok_or(H2WireError::Truncated)?;
    *offset = end;
    Ok(bytes)
}

fn put_bytes(out: &mut Vec<u8>, data: &[u8]) -> Result<(), H2WireError> {
    let len = u32::try_from(data.len()).map_err(|_| H2WireError::FieldTooLarge {
        actual: data.len(),
        max: u32::MAX as usize,
    })?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(data);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Generated and pinned by gov5's
    // internal/consensus/hotstuff/cross_client_wire_test.go.
    const GOV5_VOTE_FIXTURE: &str = concat!(
        "0298000000080706050403020120000000",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "0d0c0b0a60000000",
        "96ac1723aa056aa092d9891b0b2bfd7cdca6139feb5fdbed2c33670f08743402",
        "63676e73576d7686ed386866d9f58ba10c7a6f2835ccb63b94eb16f5e1540c2",
        "8285757a85f0298614f189b2c625cb165cb4eb8dd1d9770b711972cd5f13a9bf5",
        "00000000"
    );

    #[test]
    fn decodes_and_reencodes_gov5_vote_fixture() {
        let fixture = hex::decode(GOV5_VOTE_FIXTURE).expect("fixture is hex");
        let vote = decode_vote(&fixture).expect("gov5 vote fixture must decode");
        assert_eq!(vote.view, 0x0102_0304_0506_0708);
        assert_eq!(vote.block_hash, B256::repeat_byte(0xAA));
        assert_eq!(vote.voter, 0x0A0B_0C0D);
        assert!(vote.high_tc.is_none());
        assert_eq!(encode_vote(&vote).unwrap(), fixture);
    }

    #[test]
    fn rejects_malformed_or_oversized_envelopes() {
        assert_eq!(decode_envelope(&[]), Err(H2WireError::Truncated));
        assert_eq!(
            decode_envelope(&[2, 1, 0, 0, 0]),
            Err(H2WireError::Truncated)
        );
        let mut oversized = vec![2];
        oversized.extend_from_slice(&((MAX_ENVELOPE_PAYLOAD + 1) as u32).to_le_bytes());
        assert_eq!(
            decode_envelope(&oversized),
            Err(H2WireError::FieldTooLarge {
                actual: MAX_ENVELOPE_PAYLOAD + 1,
                max: MAX_ENVELOPE_PAYLOAD,
            })
        );
    }
}
