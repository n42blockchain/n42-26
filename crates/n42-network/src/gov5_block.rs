//! Strict decoder for Gov5's ETH-style RLP sealed-block gossip payload.

use alloy_consensus::{Header, TxEnvelope, proofs::calculate_transaction_root};
use alloy_eips::{Decodable2718, Encodable2718};
use alloy_primitives::{B256, Bytes, U256, keccak256};
use alloy_rlp::{Decodable, Encodable, Header as RlpHeader};
use alloy_rpc_types_engine::ExecutionData;
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
    #[error("execution payload cannot reconstruct a gov5 block: {0}")]
    PayloadReconstruction(String),
    #[error("execution payload block hash does not match its reconstructed header")]
    PayloadHashMismatch,
}

const GOV5_H2_SEAL_BYTES: usize = 96;

/// Returns the consensus view committed into a validated Gov5 H2 header.
///
/// Live Gov5 views are not block numbers: timeout views create gaps. Callers
/// walking an authenticated parent chain must therefore read this value from
/// each header instead of deriving `parent_view = child_view - 1`.
pub fn gov5_header_view(header: &Header) -> Result<u64, Gov5BlockError> {
    let encoded = header.extra_data.as_ref();
    if encoded.len() < 12 || &encoded[..4] != b"N42H" {
        return Err(Gov5BlockError::HeaderProfile(
            "missing canonical N42H view prefix".to_owned(),
        ));
    }
    Ok(u64::from_le_bytes(
        encoded[4..12]
            .try_into()
            .expect("length checked before fixed-width conversion"),
    ))
}

/// Converts a standard locally built Engine payload into the live gov5 H2
/// header shape. Transaction content and execution roots are unchanged; fields
/// Engine payloads cannot express (`ommers_hash`, `difficulty`), the H2
/// header-extra reservation, and pre-Shanghai withdrawals markers are
/// normalized before the new block hash is formed.
///
/// The first interoperable producer profile intentionally omits the optional
/// header QC and reserves a zero seal. Gov5 authenticates the same block hash
/// through the chain-bound H2 Proposal/CommitQC, and its header decoder accepts
/// this current `magic || view || seal` layout.
pub fn normalize_execution_payload_for_gov5_h2(
    execution: &ExecutionData,
    view: u64,
    state_root: B256,
    receipts_root: B256,
) -> Result<ExecutionData, Gov5BlockError> {
    if reconstruct_gov5_block(execution).is_ok_and(|block| {
        block.header.state_root == state_root
            && block.header.receipts_root == receipts_root
            && block.header.withdrawals_root.is_none()
            && block.header.blob_gas_used.is_none()
            && block.header.excess_blob_gas.is_none()
            && block.header.parent_beacon_block_root.is_none()
            && block.header.requests_hash.is_none()
            && block.header.block_access_list_hash.is_none()
            && block.header.slot_number.is_none()
            && block.body.withdrawals.is_none()
    }) {
        return Ok(execution.clone());
    }

    let mut block = execution
        .clone()
        .try_into_block::<TxEnvelope>()
        .map_err(|error| Gov5BlockError::PayloadReconstruction(error.to_string()))?;
    if calculate_transaction_root(&block.body.transactions) != block.header.transactions_root {
        return Err(Gov5BlockError::TransactionRootMismatch);
    }

    block.header.ommers_hash = B256::ZERO;
    block.header.difficulty = U256::ZERO;
    // The deployed Gov5 chain is pre-Shanghai. Reth's builder may represent
    // "no withdrawals" as an empty V2/V3 list and attach later-fork sidecar
    // markers; retaining any of them makes Engine API reject the otherwise
    // valid payload as post-Shanghai.
    block.header.withdrawals_root = None;
    block.header.blob_gas_used = None;
    block.header.excess_blob_gas = None;
    block.header.parent_beacon_block_root = None;
    block.header.requests_hash = None;
    block.header.block_access_list_hash = None;
    block.header.slot_number = None;
    block.body.withdrawals = None;
    // Bind the H2 header to the execution output's Gov5-native
    // `hash.DeriveSha` receipt commitment. The standard Ethereum builder uses
    // a Merkle-Patricia receipt trie (whose empty root is 56e81f...), while
    // Gov5 commits to keccak of concatenated receipt encodings (c5d246... for
    // an empty set). The caller derives this value from the builder's cached
    // execution output, so non-empty blocks remain fully supported.
    block.header.receipts_root = receipts_root;
    // Reth's stock payload builder commits the Ethereum Merkle-Patricia state
    // trie. Gov5 replay-v2 commits the same executed state transition through
    // QMDB. The caller derives this root from the authenticated parent QMDB
    // branch and the builder's exact execution output.
    block.header.state_root = state_root;
    let mut extra = Vec::with_capacity(12 + GOV5_H2_SEAL_BYTES);
    extra.extend_from_slice(b"N42H");
    extra.extend_from_slice(&view.to_le_bytes());
    extra.resize(12 + GOV5_H2_SEAL_BYTES, 0);
    block.header.extra_data = extra.into();
    validate_gov5_interop_header(&block.header)
        .map_err(|error| Gov5BlockError::HeaderProfile(error.to_string()))?;

    let block_hash = block.header.hash_slow();
    Ok(ExecutionData::from_block_unchecked(block_hash, &block))
}

/// Encodes a locally built execution payload as gov5's canonical block gossip
/// form. Auxiliary verifier/reward lists are empty because execution validity
/// and H2-v4 consensus authentication are carried independently.
pub fn encode_gov5_block_rlp(execution: &ExecutionData) -> Result<Vec<u8>, Gov5BlockError> {
    let block = reconstruct_gov5_block(execution)?;
    validate_gov5_interop_header(&block.header)
        .map_err(|error| Gov5BlockError::HeaderProfile(error.to_string()))?;
    if block.header.hash_slow() != execution.block_hash() {
        return Err(Gov5BlockError::PayloadHashMismatch);
    }
    if calculate_transaction_root(&block.body.transactions) != block.header.transactions_root {
        return Err(Gov5BlockError::TransactionRootMismatch);
    }

    let mut header_rlp = Vec::new();
    block.header.encode(&mut header_rlp);
    let transaction_bytes = block
        .body
        .transactions
        .iter()
        .map(|transaction| Bytes::from(transaction.encoded_2718()))
        .collect::<Vec<_>>();
    let verifiers = Vec::<Bytes>::new();
    let rewards = Vec::<Bytes>::new();
    let payload_length =
        header_rlp.len() + transaction_bytes.length() + verifiers.length() + rewards.length();
    let mut encoded = Vec::new();
    RlpHeader {
        list: true,
        payload_length,
    }
    .encode(&mut encoded);
    encoded.extend_from_slice(&header_rlp);
    transaction_bytes.encode(&mut encoded);
    verifiers.encode(&mut encoded);
    rewards.encode(&mut encoded);
    Ok(encoded)
}

/// Engine payloads do not carry `ommers_hash` or `difficulty`. Reth therefore
/// reconstructs Ethereum's empty-list ommers commitment, while live gov5 H2
/// headers deliberately commit to a zero ommers hash. Rebuild both permitted
/// gov5 H2 difficulty variants and let the authenticated payload hash select
/// the exact header; no operator-controlled guessing is involved.
fn reconstruct_gov5_block(
    execution: &ExecutionData,
) -> Result<alloy_consensus::Block<TxEnvelope>, Gov5BlockError> {
    let expected_hash = execution.block_hash();
    if let Ok(direct) = execution.clone().try_into_block::<TxEnvelope>()
        && validate_gov5_interop_header(&direct.header).is_ok()
        && direct.header.hash_slow() == expected_hash
    {
        return Ok(direct);
    }

    let extra_data = execution.payload.as_v1().extra_data.clone();
    let mut standard_execution = execution.clone();
    standard_execution.payload.set_extra_data(Bytes::new());
    let mut block = standard_execution
        .try_into_block::<TxEnvelope>()
        .map_err(|error| Gov5BlockError::PayloadReconstruction(error.to_string()))?;
    block.header.ommers_hash = B256::ZERO;
    block.header.extra_data = extra_data;

    block.header.difficulty = U256::ZERO;
    if validate_gov5_interop_header(&block.header).is_ok()
        && block.header.hash_slow() == expected_hash
    {
        return Ok(block);
    }
    block.header.difficulty = U256::from(1);
    if validate_gov5_interop_header(&block.header).is_ok()
        && block.header.hash_slow() == expected_hash
    {
        return Ok(block);
    }
    Err(Gov5BlockError::PayloadHashMismatch)
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
    use alloy_consensus::{Block, BlockBody, proofs::calculate_transaction_root};
    use alloy_primitives::{Bytes, U256};
    use alloy_rlp::Encodable;
    use n42_consensus::gov5_native_receipts_root;

    fn block_fixture(transactions_root: B256) -> Vec<u8> {
        let header = Header {
            ommers_hash: B256::ZERO,
            transactions_root,
            difficulty: U256::ZERO,
            base_fee_per_gas: Some(0),
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

    #[test]
    fn locally_built_execution_payload_round_trips_through_gov5_block_wire() {
        let original = decode_gov5_block_rlp(&block_fixture(calculate_transaction_root::<
            TxEnvelope,
        >(&[])))
        .unwrap();
        let block: Block<TxEnvelope> = Block {
            header: original.header,
            body: BlockBody {
                transactions: original.transactions,
                ommers: Vec::new(),
                withdrawals: None,
            },
        };
        let execution = ExecutionData::from_block_unchecked(original.block_hash, &block);

        let encoded = encode_gov5_block_rlp(&execution).unwrap();
        let recovered = decode_gov5_block_rlp(&encoded).unwrap();
        assert_eq!(recovered.block_hash, original.block_hash);
        assert_eq!(recovered.header, block.header);
        assert!(recovered.transactions.is_empty());
    }

    #[test]
    fn normalizes_standard_engine_payload_for_gov5_h2_leader() {
        let block: Block<TxEnvelope> = Block {
            header: Header {
                transactions_root: calculate_transaction_root::<TxEnvelope>(&[]),
                base_fee_per_gas: Some(0),
                ..Default::default()
            },
            body: BlockBody::default(),
        };
        let standard_hash = block.header.hash_slow();
        let standard = ExecutionData::from_block_unchecked(standard_hash, &block);

        let native_state_root = B256::repeat_byte(0x19);
        let normalized = normalize_execution_payload_for_gov5_h2(
            &standard,
            19,
            native_state_root,
            gov5_native_receipts_root(&[]),
        )
        .unwrap();
        assert_ne!(normalized.block_hash(), standard_hash);
        let encoded = encode_gov5_block_rlp(&normalized).unwrap();
        let recovered = decode_gov5_block_rlp(&encoded).unwrap();
        assert_eq!(recovered.block_hash, normalized.block_hash());
        assert_eq!(recovered.header.ommers_hash, B256::ZERO);
        assert_eq!(recovered.header.difficulty, U256::ZERO);
        assert_eq!(recovered.header.state_root, native_state_root);
        assert_eq!(&recovered.header.extra_data[..4], b"N42H");
        assert_eq!(gov5_header_view(&recovered.header).unwrap(), 19);
        assert_eq!(recovered.header.extra_data.len(), 108);
    }

    #[test]
    fn normalizes_local_zero_ommers_payload_hidden_by_engine_shape() {
        let block: Block<TxEnvelope> = Block {
            header: Header {
                ommers_hash: B256::ZERO,
                transactions_root: calculate_transaction_root::<TxEnvelope>(&[]),
                base_fee_per_gas: Some(0),
                ..Default::default()
            },
            body: BlockBody::default(),
        };
        let local_hash = block.header.hash_slow();
        let local = ExecutionData::from_block_unchecked(local_hash, &block);

        let normalized = normalize_execution_payload_for_gov5_h2(
            &local,
            23,
            B256::repeat_byte(0x23),
            gov5_native_receipts_root(&[]),
        )
        .unwrap();
        let recovered =
            decode_gov5_block_rlp(&encode_gov5_block_rlp(&normalized).unwrap()).unwrap();
        assert_eq!(recovered.header.ommers_hash, B256::ZERO);
        assert_eq!(gov5_header_view(&recovered.header).unwrap(), 23);
    }

    #[test]
    fn normalization_strips_empty_withdrawals_for_pre_shanghai_gov5() {
        let block: Block<TxEnvelope> = Block {
            header: Header {
                transactions_root: calculate_transaction_root::<TxEnvelope>(&[]),
                withdrawals_root: Some(B256::repeat_byte(0x42)),
                blob_gas_used: Some(0),
                excess_blob_gas: Some(0),
                parent_beacon_block_root: Some(B256::ZERO),
                base_fee_per_gas: Some(0),
                ..Default::default()
            },
            body: BlockBody {
                withdrawals: Some(Default::default()),
                ..Default::default()
            },
        };
        let standard_hash = block.header.hash_slow();
        let standard = ExecutionData::from_block_unchecked(standard_hash, &block);

        let native_receipts_root = gov5_native_receipts_root(&[]);
        let native_state_root = B256::repeat_byte(0x29);
        let normalized = normalize_execution_payload_for_gov5_h2(
            &standard,
            29,
            native_state_root,
            native_receipts_root,
        )
        .unwrap();
        let recovered =
            decode_gov5_block_rlp(&encode_gov5_block_rlp(&normalized).unwrap()).unwrap();
        assert!(recovered.header.withdrawals_root.is_none());
        assert!(recovered.header.parent_beacon_block_root.is_none());
        assert_eq!(recovered.header.state_root, native_state_root);
        assert_eq!(recovered.header.receipts_root, native_receipts_root);
        assert!(normalized.withdrawals().is_none());
        assert!(normalized.parent_beacon_block_root().is_none());
    }

    #[test]
    fn rejects_header_without_canonical_h2_view_prefix() {
        let header = Header {
            extra_data: Bytes::from_static(b"not-an-h2-header"),
            ..Default::default()
        };
        assert!(matches!(
            gov5_header_view(&header),
            Err(Gov5BlockError::HeaderProfile(_))
        ));
    }
}
