//! Strict decoder for Gov5's ETH-style RLP sealed-block gossip payload.

use alloy_consensus::{Header, TxEnvelope, proofs::calculate_transaction_root};
use alloy_eips::{Decodable2718, Encodable2718};
use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_rlp::{Decodable, Encodable, Header as RlpHeader};
use alloy_rpc_types_engine::ExecutionData;
use n42_consensus::{
    Gov5NativeHeader, Gov5Reward, remember_gov5_native_header, validate_gov5_interop_header,
};

#[derive(Clone, Debug)]
pub struct Gov5GossipBlock {
    /// keccak of the header's exact gov5 encoding, which alloy's re-encoding
    /// does not reproduce for headers with `0x80` placeholders or a
    /// mobile-registry root; see [`Gov5NativeHeader`].
    pub block_hash: B256,
    pub header: Header,
    pub transactions: Vec<TxEnvelope>,
    /// gov5's reward list, committed to in `header.withdrawals_root`.
    pub rewards: Vec<Gov5Reward>,
    /// gov5's 23rd header field, carried so the header can be re-encoded.
    pub mobile_registry_root: Option<B256>,
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
    #[error("gov5 block fetch hash mismatch: requested {expected}, got {actual}")]
    UnexpectedBlockHash { expected: B256, actual: B256 },
}

impl Gov5BlockError {
    /// Whether the failure is intrinsic to the header. This must still be
    /// paired with [`attributable_hash`] matching the requested hash.
    ///
    /// A matching header alone does not authenticate the body: a peer can
    /// attach malformed RLP, different transactions, or invalid auxiliary
    /// fields to an honest header. Those failures must remain retryable.
    pub const fn is_permanent(&self) -> bool {
        match self {
            Self::HeaderProfile(_) => true,
            Self::InvalidRlp
            | Self::TransactionRootMismatch
            | Self::PayloadReconstruction(_)
            | Self::PayloadHashMismatch
            | Self::UnexpectedBlockHash { .. } => false,
        }
    }
}

/// Recovers the block hash a response commits to, when it commits to one.
///
/// Only the outer list and the header item have to be well-formed: the hash is
/// `keccak256` of the header RLP, so this answers "whose block is this?" even
/// for a body that later fails validation. Returns `None` when the bytes do not
/// name a block at all — an empty response, a truncated frame, a garbage
/// payload — because there is then nothing to attribute a rejection to.
pub fn attributable_hash(encoded: &[u8]) -> Option<B256> {
    let mut payload = encoded;
    let outer = RlpHeader::decode(&mut payload).ok()?;
    if !outer.list || outer.payload_length != payload.len() {
        return None;
    }
    let header_rlp = take_rlp_item(&mut payload)?;
    // Confirm it parses as a header before trusting the hash: otherwise any
    // leading list item would masquerade as one.
    Gov5NativeHeader::decode(header_rlp).ok()?;
    Some(keccak256(header_rlp))
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
    // A native chain-94 parent selects the post-Cancun profile. Never erase
    // executed withdrawals or the committee link as the early devnet profile does.
    if let Some(parent) = n42_consensus::remembered_gov5_native_header(&execution.parent_hash())
        && parent.mobile_registry_root.is_some()
    {
        return normalize_native_payload(execution, view, state_root, receipts_root, &parent);
    }
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

fn normalize_native_payload(
    execution: &ExecutionData,
    view: u64,
    state_root: B256,
    receipts_root: B256,
    parent: &Gov5NativeHeader,
) -> Result<ExecutionData, Gov5BlockError> {
    if parent.mobile_registry_root != Some(B256::ZERO) {
        return Err(Gov5BlockError::PayloadReconstruction(
            "native mobile registry updates are not implemented".into(),
        ));
    }
    let mut block = execution
        .clone()
        .try_into_block::<TxEnvelope>()
        .map_err(|error| Gov5BlockError::PayloadReconstruction(error.to_string()))?;
    if block
        .header
        .requests_hash
        .is_some_and(|hash| hash != alloy_eips::eip7685::EMPTY_REQUESTS_HASH)
        || block.header.block_access_list_hash.is_some()
        || block.header.slot_number.is_some()
    {
        return Err(Gov5BlockError::PayloadReconstruction(
            "native producer cannot omit non-empty requests or later-fork commitments".into(),
        ));
    }
    let rewards = n42_consensus::gov5_withdrawals_to_rewards(
        block
            .body
            .withdrawals
            .as_ref()
            .map(|w| w.as_slice())
            .unwrap_or_default(),
    );
    block.header.ommers_hash = B256::ZERO;
    block.header.difficulty = U256::ZERO;
    block.header.state_root = state_root;
    block.header.receipts_root = receipts_root;
    block.header.withdrawals_root = Some(n42_consensus::gov5_native_rewards_root(&rewards));
    // Gov5 executes Prague system calls, but its native miner omits requestsHash.
    block.header.requests_hash = None;
    let mut extra = Vec::with_capacity(12 + GOV5_H2_SEAL_BYTES);
    extra.extend_from_slice(b"N42H");
    extra.extend_from_slice(&view.to_le_bytes());
    extra.resize(12 + GOV5_H2_SEAL_BYTES, 0);
    block.header.extra_data = extra.into();
    validate_gov5_interop_header(&block.header)
        .map_err(|error| Gov5BlockError::HeaderProfile(error.to_string()))?;
    let native = Gov5NativeHeader {
        header: block.header.clone(),
        mobile_registry_root: Some(B256::ZERO),
    };
    let hash = remember_gov5_native_header(&native.encode());
    Ok(ExecutionData::from_block_unchecked(hash, &block))
}

/// Encodes a locally built execution payload as gov5's canonical block gossip
/// form. Auxiliary verifier/reward lists are empty because execution validity
/// and H2-v4 consensus authentication are carried independently.
pub fn encode_gov5_block_rlp(execution: &ExecutionData) -> Result<Vec<u8>, Gov5BlockError> {
    let block = reconstruct_gov5_block(execution)?;
    validate_gov5_interop_header(&block.header)
        .map_err(|error| Gov5BlockError::HeaderProfile(error.to_string()))?;
    let native = n42_consensus::remembered_gov5_native_header(&execution.block_hash());
    if native
        .as_ref()
        .map_or_else(|| block.header.hash_slow(), Gov5NativeHeader::hash)
        != execution.block_hash()
    {
        return Err(Gov5BlockError::PayloadHashMismatch);
    }
    if calculate_transaction_root(&block.body.transactions) != block.header.transactions_root {
        return Err(Gov5BlockError::TransactionRootMismatch);
    }

    let mut header_rlp = Vec::new();
    if let Some(native) = native {
        header_rlp = native.encode();
    } else {
        block.header.encode(&mut header_rlp);
    }
    let transaction_bytes = block
        .body
        .transactions
        .iter()
        .map(|transaction| Bytes::from(transaction.encoded_2718()))
        .collect::<Vec<_>>();
    let verifiers = Vec::<Bytes>::new();
    let rewards = n42_consensus::gov5_withdrawals_to_rewards(
        block
            .body
            .withdrawals
            .as_ref()
            .map(|w| w.as_slice())
            .unwrap_or_default(),
    );
    let mut reward_items = Vec::new();
    for reward in rewards {
        RlpHeader {
            list: true,
            payload_length: reward.address.length() + reward.amount.length(),
        }
        .encode(&mut reward_items);
        reward.address.encode(&mut reward_items);
        reward.amount.encode(&mut reward_items);
    }
    let mut rewards_rlp = Vec::new();
    RlpHeader {
        list: true,
        payload_length: reward_items.len(),
    }
    .encode(&mut rewards_rlp);
    rewards_rlp.extend_from_slice(&reward_items);
    let payload_length =
        header_rlp.len() + transaction_bytes.length() + verifiers.length() + rewards_rlp.len();
    let mut encoded = Vec::new();
    RlpHeader {
        list: true,
        payload_length,
    }
    .encode(&mut encoded);
    encoded.extend_from_slice(&header_rlp);
    transaction_bytes.encode(&mut encoded);
    verifiers.encode(&mut encoded);
    encoded.extend_from_slice(&rewards_rlp);
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

    if let Some(native) = n42_consensus::remembered_gov5_native_header(&expected_hash) {
        let rewards = n42_consensus::gov5_withdrawals_to_rewards(
            block
                .body
                .withdrawals
                .as_ref()
                .map(|w| w.as_slice())
                .unwrap_or_default(),
        );
        block.header.difficulty = native.header.difficulty;
        block.header.withdrawals_root = block
            .body
            .withdrawals
            .is_some()
            .then(|| n42_consensus::gov5_native_rewards_root(&rewards));
        // Engine cannot spell a nil native requestsHash. Only an empty
        // execution-request set can stand in for it.
        if native.header.requests_hash.is_none()
            && block.header.requests_hash == Some(alloy_eips::eip7685::EMPTY_REQUESTS_HASH)
        {
            block.header.requests_hash = None;
        }
        if block.header == native.header && native.hash() == expected_hash {
            return Ok(block);
        }
        return Err(Gov5BlockError::PayloadHashMismatch);
    }

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
    let native = Gov5NativeHeader::decode(header_rlp).map_err(|_| Gov5BlockError::InvalidRlp)?;
    if native.encode() != header_rlp {
        // The decoder accepted bytes it cannot reproduce; the hash below
        // would then name a header this node can never re-encode.
        return Err(Gov5BlockError::InvalidRlp);
    }
    let header = native.header;
    validate_gov5_interop_header(&header)
        .map_err(|error| Gov5BlockError::HeaderProfile(error.to_string()))?;

    let transactions_rlp = take_rlp_item(&mut payload).ok_or(Gov5BlockError::InvalidRlp)?;
    let transactions = decode_transactions(transactions_rlp)?;

    // Gov5's blockRLP always has verifier and reward lists, followed by an
    // optional ZK proof. Consume their canonical RLP items so trailing bytes or
    // schema-shifted bodies cannot be mistaken for an execution block.
    take_rlp_list_item(&mut payload).ok_or(Gov5BlockError::InvalidRlp)?;
    let rewards_rlp = take_rlp_list_item(&mut payload).ok_or(Gov5BlockError::InvalidRlp)?;
    let rewards = decode_rewards(rewards_rlp)?;
    if !payload.is_empty() {
        take_rlp_bytes(&mut payload).ok_or(Gov5BlockError::InvalidRlp)?;
    }
    if !payload.is_empty() {
        return Err(Gov5BlockError::InvalidRlp);
    }

    if calculate_transaction_root(&transactions) != header.transactions_root {
        return Err(Gov5BlockError::TransactionRootMismatch);
    }

    // Remember the exact encoding: the engine seals the block with this hash
    // and must be able to prove the header behind it.
    let block_hash = remember_gov5_native_header(header_rlp);
    Ok(Gov5GossipBlock {
        block_hash,
        header,
        transactions,
        rewards,
        mobile_registry_root: native.mobile_registry_root,
    })
}

/// Decodes gov5's `[]*block.Reward` list: `[[address, amount], ...]`.
fn decode_rewards(encoded: &[u8]) -> Result<Vec<Gov5Reward>, Gov5BlockError> {
    let mut payload = encoded;
    let list = RlpHeader::decode(&mut payload).map_err(|_| Gov5BlockError::InvalidRlp)?;
    if !list.list || list.payload_length != payload.len() {
        return Err(Gov5BlockError::InvalidRlp);
    }
    let mut rewards = Vec::new();
    while !payload.is_empty() {
        let item = take_rlp_list_item(&mut payload).ok_or(Gov5BlockError::InvalidRlp)?;
        let mut cursor = item;
        let head = RlpHeader::decode(&mut cursor).map_err(|_| Gov5BlockError::InvalidRlp)?;
        if !head.list || head.payload_length != cursor.len() {
            return Err(Gov5BlockError::InvalidRlp);
        }
        let address = Address::decode(&mut cursor).map_err(|_| Gov5BlockError::InvalidRlp)?;
        let amount = U256::decode(&mut cursor).map_err(|_| Gov5BlockError::InvalidRlp)?;
        if !cursor.is_empty() {
            return Err(Gov5BlockError::InvalidRlp);
        }
        rewards.push(Gov5Reward { address, amount });
    }
    Ok(rewards)
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
        block_fixture_with_transactions(transactions_root, Vec::new())
    }

    fn block_fixture_with_transactions(
        transactions_root: B256,
        transactions: Vec<Bytes>,
    ) -> Vec<u8> {
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
    fn corrupted_body_cannot_permanently_reject_a_valid_header() {
        let good = block_fixture(calculate_transaction_root::<TxEnvelope>(&[]));
        let hash = decode_gov5_block_rlp(&good).unwrap().block_hash;
        let mut bad = good.clone();
        *bad.last_mut().unwrap() = 0x80;
        assert_eq!(attributable_hash(&bad), Some(hash));
        let error = decode_gov5_block_rlp(&bad).unwrap_err();
        assert!(
            !error.is_permanent(),
            "a peer can corrupt unsigned body fields"
        );
        assert_eq!(decode_gov5_block_rlp(&good).unwrap().block_hash, hash);
    }

    #[test]
    fn missing_transactions_are_retryable_even_when_header_hash_matches() {
        use alloy_consensus::{SignableTransaction, TxLegacy};
        use alloy_primitives::Signature;
        let transaction: TxEnvelope = TxLegacy::default()
            .into_signed(Signature::new(U256::from(1), U256::from(2), false))
            .into();
        let root = calculate_transaction_root(std::slice::from_ref(&transaction));
        let good = block_fixture_with_transactions(root, vec![transaction.encoded_2718().into()]);
        let hash = decode_gov5_block_rlp(&good).unwrap().block_hash;
        let missing = block_fixture(root);
        assert_eq!(attributable_hash(&missing), Some(hash));
        let error = decode_gov5_block_rlp(&missing).unwrap_err();
        assert_eq!(error, Gov5BlockError::TransactionRootMismatch);
        assert!(!error.is_permanent());
        assert_eq!(decode_gov5_block_rlp(&good).unwrap().block_hash, hash);
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
    fn native_producer_preserves_rewards_committee_and_mobile_header() {
        use alloy_eips::eip4895::{Withdrawal, Withdrawals};
        let parent = Gov5NativeHeader {
            header: Header {
                number: 13560375,
                ..Default::default()
            },
            mobile_registry_root: Some(B256::ZERO),
        };
        let parent_hash = remember_gov5_native_header(&parent.encode());
        let withdrawals = Withdrawals::new(vec![Withdrawal {
            index: 0,
            validator_index: 0,
            address: Address::repeat_byte(0x31),
            amount: 1_000_000_000,
        }]);
        let block: Block<TxEnvelope> = Block {
            header: Header {
                parent_hash,
                number: parent.header.number + 1,
                base_fee_per_gas: Some(7),
                transactions_root: calculate_transaction_root::<TxEnvelope>(&[]),
                withdrawals_root: Some(alloy_consensus::proofs::calculate_withdrawals_root(
                    &withdrawals,
                )),
                blob_gas_used: Some(0),
                excess_blob_gas: Some(0),
                parent_beacon_block_root: Some(B256::repeat_byte(0x44)),
                requests_hash: Some(alloy_eips::eip7685::EMPTY_REQUESTS_HASH),
                ..Default::default()
            },
            body: BlockBody {
                withdrawals: Some(withdrawals.clone()),
                ..Default::default()
            },
        };
        let execution = ExecutionData::from_block_unchecked(block.header.hash_slow(), &block);
        let normalized = normalize_execution_payload_for_gov5_h2(
            &execution,
            11031,
            B256::repeat_byte(0x71),
            keccak256([]),
        )
        .unwrap();
        let encoded = encode_gov5_block_rlp(&normalized).unwrap();
        let decoded = decode_gov5_block_rlp(&encoded).unwrap();
        assert_eq!(decoded.block_hash, normalized.block_hash());
        assert_eq!(decoded.mobile_registry_root, Some(B256::ZERO));
        assert_eq!(
            decoded.header.parent_beacon_block_root,
            block.header.parent_beacon_block_root
        );
        assert_eq!(decoded.header.blob_gas_used, Some(0));
        assert_eq!(decoded.header.excess_blob_gas, Some(0));
        assert_eq!(decoded.header.requests_hash, None);
        assert_eq!(
            decoded.rewards,
            n42_consensus::gov5_withdrawals_to_rewards(&withdrawals)
        );
        assert_eq!(gov5_header_view(&decoded.header).unwrap(), 11031);

        let mut tampered = normalized.clone();
        tampered.payload.as_v1_mut().state_root = B256::ZERO;
        assert!(encode_gov5_block_rlp(&tampered).is_err());

        let mut unsupported = block;
        unsupported.header.requests_hash = Some(B256::repeat_byte(0x55));
        let unsupported =
            ExecutionData::from_block_unchecked(unsupported.header.hash_slow(), &unsupported);
        assert!(
            normalize_execution_payload_for_gov5_h2(&unsupported, 11032, B256::ZERO, B256::ZERO)
                .is_err()
        );
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

    #[test]
    fn only_header_profile_verdicts_are_permanent() {
        assert!(Gov5BlockError::HeaderProfile("invalid difficulty".to_owned()).is_permanent());
        for error in [
            Gov5BlockError::InvalidRlp,
            Gov5BlockError::TransactionRootMismatch,
            Gov5BlockError::PayloadReconstruction("bad payload".to_owned()),
            Gov5BlockError::PayloadHashMismatch,
            Gov5BlockError::UnexpectedBlockHash {
                expected: B256::ZERO,
                actual: B256::repeat_byte(1),
            },
        ] {
            assert!(
                !error.is_permanent(),
                "{error} can be caused by a peer changing the response body"
            );
        }
    }

    /// A rejection is only a verdict on the requested block if the bytes
    /// belong to it. Serving peers answer "I do not have it" with an empty
    /// body, and the serve cache is a 1024-entry FIFO, so an honest and fully
    /// synced peer returns empty for anything older than that. Attributing
    /// such a response to the requested hash would blacklist a good block and
    /// stall ancestry walks permanently.
    #[test]
    fn unattributable_responses_name_no_block() {
        for (name, bytes) in [
            ("empty response (peer does not have the block)", Vec::new()),
            ("garbage", vec![0xFF, 0xFE, 0xFD]),
            ("empty rlp list", vec![0xC0]),
            (
                "truncated frame",
                block_fixture(calculate_transaction_root::<TxEnvelope>(&[]))[..20].to_vec(),
            ),
        ] {
            assert_eq!(
                attributable_hash(&bytes),
                None,
                "{name} must not be attributed to any hash"
            );
        }
    }

    /// Attribution identifies the header, but does not validate its body.
    #[test]
    fn a_body_that_fails_validation_still_names_its_block() {
        let good = block_fixture(calculate_transaction_root::<TxEnvelope>(&[]));
        let expected = decode_gov5_block_rlp(&good).unwrap().block_hash;
        assert_eq!(attributable_hash(&good), Some(expected));

        // Same header, wrong transaction root commitment: decoding fails, but
        // the bytes still identify which block they claim to be.
        let bad = block_fixture(B256::repeat_byte(0x99));
        let bad_hash = attributable_hash(&bad).expect("header is intact");
        assert!(decode_gov5_block_rlp(&bad).is_err());
        assert_ne!(
            bad_hash, expected,
            "a different transaction root yields a different block"
        );
    }
}
