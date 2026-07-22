//! Strict decoder for Gov5's ETH-style RLP sealed-block gossip payload.

use alloy_consensus::{Header, TxEnvelope, proofs::calculate_transaction_root};
use alloy_eips::Decodable2718;
use alloy_primitives::{B256, keccak256};
use alloy_rlp::{Decodable, Header as RlpHeader};
use n42_consensus::validate_gov5_interop_header;

#[derive(Clone, Debug)]
pub struct Gov5GossipBlock {
    pub block_hash: B256,
    pub header: Header,
    pub transactions: Vec<TxEnvelope>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Gov5BlockError {
    #[error("invalid gov5 block RLP")]
    InvalidRlp,
    #[error("gov5 block header violates the interop profile: {0}")]
    HeaderProfile(String),
    #[error("gov5 block transaction root mismatch")]
    TransactionRootMismatch,
}

/// Decodes the uncompressed Gov5 block wire form:
/// `[header, tx_bytes, verifiers, rewards, zkproof?]`.
///
/// Auxiliary consensus fields are structurally consumed but never trusted by
/// the execution observer. The authenticated header commits to the transaction
/// root, while H2-v4 finality separately authenticates the resulting block hash.
pub fn decode_gov5_block_rlp(encoded: &[u8]) -> Result<Gov5GossipBlock, Gov5BlockError> {
    let mut payload = encoded;
    let outer = RlpHeader::decode(&mut payload).map_err(|_| Gov5BlockError::InvalidRlp)?;
    if !outer.list || outer.payload_length != payload.len() {
        return Err(Gov5BlockError::InvalidRlp);
    }

    let header_rlp = take_rlp_item(&mut payload).ok_or(Gov5BlockError::InvalidRlp)?;
    let mut header_cursor = header_rlp;
    let header = Header::decode(&mut header_cursor).map_err(|_| Gov5BlockError::InvalidRlp)?;
    if !header_cursor.is_empty() {
        return Err(Gov5BlockError::InvalidRlp);
    }
    validate_gov5_interop_header(&header)
        .map_err(|error| Gov5BlockError::HeaderProfile(error.to_string()))?;

    let transactions_rlp = take_rlp_item(&mut payload).ok_or(Gov5BlockError::InvalidRlp)?;
    let transactions = decode_transactions(transactions_rlp)?;

    // Gov5's blockRLP always has verifier and reward lists, followed by an
    // optional ZK proof. Consume their canonical RLP items so trailing bytes or
    // schema-shifted bodies cannot be mistaken for an execution block.
    take_rlp_list_item(&mut payload).ok_or(Gov5BlockError::InvalidRlp)?;
    take_rlp_list_item(&mut payload).ok_or(Gov5BlockError::InvalidRlp)?;
    if !payload.is_empty() {
        take_rlp_bytes(&mut payload).ok_or(Gov5BlockError::InvalidRlp)?;
    }
    if !payload.is_empty() {
        return Err(Gov5BlockError::InvalidRlp);
    }

    if calculate_transaction_root(&transactions) != header.transactions_root {
        return Err(Gov5BlockError::TransactionRootMismatch);
    }

    Ok(Gov5GossipBlock {
        block_hash: keccak256(header_rlp),
        header,
        transactions,
    })
}

fn decode_transactions(encoded: &[u8]) -> Result<Vec<TxEnvelope>, Gov5BlockError> {
    let mut payload = encoded;
    let list = RlpHeader::decode(&mut payload).map_err(|_| Gov5BlockError::InvalidRlp)?;
    if !list.list || list.payload_length != payload.len() {
        return Err(Gov5BlockError::InvalidRlp);
    }

    let mut transactions = Vec::new();
    while !payload.is_empty() {
        let encoded_tx = take_rlp_bytes(&mut payload).ok_or(Gov5BlockError::InvalidRlp)?;
        let mut tx_cursor = encoded_tx;
        let transaction =
            TxEnvelope::decode_2718(&mut tx_cursor).map_err(|_| Gov5BlockError::InvalidRlp)?;
        if !tx_cursor.is_empty() {
            return Err(Gov5BlockError::InvalidRlp);
        }
        transactions.push(transaction);
    }
    Ok(transactions)
}

fn take_rlp_bytes<'a>(cursor: &mut &'a [u8]) -> Option<&'a [u8]> {
    let mut payload = *cursor;
    let header = RlpHeader::decode(&mut payload).ok()?;
    if header.list || header.payload_length > payload.len() {
        return None;
    }
    let value = payload.get(..header.payload_length)?;
    *cursor = payload.get(header.payload_length..)?;
    Some(value)
}

fn take_rlp_item<'a>(cursor: &mut &'a [u8]) -> Option<&'a [u8]> {
    let original = *cursor;
    let mut payload = original;
    let header = RlpHeader::decode(&mut payload).ok()?;
    let prefix_len = original.len().checked_sub(payload.len())?;
    let total_len = prefix_len.checked_add(header.payload_length)?;
    let item = original.get(..total_len)?;
    *cursor = original.get(total_len..)?;
    Some(item)
}

fn take_rlp_list_item<'a>(cursor: &mut &'a [u8]) -> Option<&'a [u8]> {
    let item = take_rlp_item(cursor)?;
    let mut payload = item;
    let header = RlpHeader::decode(&mut payload).ok()?;
    (header.list && header.payload_length == payload.len()).then_some(item)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::proofs::calculate_transaction_root;
    use alloy_primitives::{Bytes, U256};
    use alloy_rlp::Encodable;

    fn block_fixture(transactions_root: B256) -> Vec<u8> {
        let header = Header {
            ommers_hash: B256::ZERO,
            transactions_root,
            difficulty: U256::ZERO,
            extra_data: Bytes::from_static(b"N42H\x07\0\0\0\0\0\0\0"),
            ..Default::default()
        };
        let mut header_rlp = Vec::new();
        header.encode(&mut header_rlp);
        let transactions = Vec::<Bytes>::new();
        let verifiers = Vec::<Bytes>::new();
        let rewards = Vec::<Bytes>::new();
        let payload_length =
            header_rlp.len() + transactions.length() + verifiers.length() + rewards.length();
        let mut encoded = Vec::new();
        RlpHeader {
            list: true,
            payload_length,
        }
        .encode(&mut encoded);
        encoded.extend_from_slice(&header_rlp);
        transactions.encode(&mut encoded);
        verifiers.encode(&mut encoded);
        rewards.encode(&mut encoded);
        encoded
    }

    #[test]
    fn decodes_gov5_eth_style_block_and_binds_transaction_root() {
        let encoded = block_fixture(calculate_transaction_root::<TxEnvelope>(&[]));
        let decoded = decode_gov5_block_rlp(&encoded).unwrap();
        assert_eq!(decoded.header.number, 0);
        assert!(decoded.transactions.is_empty());
        assert_eq!(decoded.block_hash, decoded.header.hash_slow());
    }

    #[test]
    fn rejects_body_that_does_not_match_authenticated_transaction_root() {
        let encoded = block_fixture(B256::repeat_byte(0x42));
        assert_eq!(
            decode_gov5_block_rlp(&encoded).unwrap_err(),
            Gov5BlockError::TransactionRootMismatch
        );
    }

    #[test]
    fn rejects_schema_shifted_auxiliary_fields() {
        let mut encoded = block_fixture(calculate_transaction_root::<TxEnvelope>(&[]));
        *encoded.last_mut().unwrap() = 0x80;
        assert_eq!(
            decode_gov5_block_rlp(&encoded).unwrap_err(),
            Gov5BlockError::InvalidRlp
        );
    }
}
