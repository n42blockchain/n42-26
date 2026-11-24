//! Strict decoder for Gov5's ETH-style RLP sealed-block gossip payload.

use alloy_consensus::{Header, proofs::calculate_transaction_root};
use n42_tx_types::N42TxEnvelope;
use alloy_eips::{Decodable2718, Encodable2718};
use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_rlp::{Decodable, Encodable, Header as RlpHeader};
use alloy_rpc_types_engine::ExecutionData;
use n42_chainspec::profile1::Profile1Header;
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
    pub transactions: Vec<N42TxEnvelope>,
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
    #[error("gov5 block rewards do not match the header commitment")]
    RewardsRootMismatch,
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
            | Self::RewardsRootMismatch
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
    // leading list item would masquerade as one. A Profile-1 block carries
    // the 24-element header.
    if Gov5NativeHeader::decode(header_rlp).is_err()
        && n42_chainspec::profile1::Profile1Header::decode(header_rlp).is_err()
    {
        return None;
    }
    Some(keccak256(header_rlp))
}

/// Decodes a served or gossiped block in either wire layout: the gov5 one or
/// a Profile-1 chain's 24-element one.
pub fn decode_wire_block_rlp(encoded: &[u8]) -> Result<Gov5GossipBlock, Gov5BlockError> {
    decode_gov5_block_rlp(encoded).or_else(|gov5| decode_profile1_block_rlp(encoded).map_err(|_| gov5))
}

const GOV5_H2_SEAL_BYTES: usize = 96;
/// Magic plus the little-endian view.
const GOV5_H2_EXTRA_PREFIX: usize = 12;
/// How many remembered headers a producer walks back from the parent to find
/// the block its CommitQC names. A committed block normally sits one to
/// three blocks below the parent.
const HEADER_QC_ANCESTOR_DEPTH: usize = 64;

/// The CommitQC a gov5 H2 producer embeds in the header `Extra` (sync
/// standard section 3.2): the last CommitQC the leader holds at build time,
/// in gov5's QC wire form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Gov5HeaderQc {
    /// The view the QC certifies.
    pub view: u64,
    /// The block the QC certifies.
    pub block_hash: B256,
    /// `encodeQC` of the certificate.
    pub wire: Vec<u8>,
}

impl Gov5HeaderQc {
    /// The header form of a commit-domain QC. `None` for the view-0 genesis
    /// sentinel, which a producer never embeds, or a certificate gov5's
    /// encoding cannot carry.
    pub fn from_commit_qc(qc: &n42_primitives::consensus::QuorumCertificate) -> Option<Self> {
        if qc.view == 0 {
            return None;
        }
        let h2 = crate::h2_bridge::quorum_certificate_to_h2(qc).ok()?;
        let wire = crate::h2_wire::encode_h2_qc(&h2).ok()?;
        Some(Self {
            view: qc.view,
            block_hash: qc.block_hash,
            wire,
        })
    }
}

/// What a gov5 H2 header `Extra` carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Gov5HeaderExtra {
    /// The view the block was proposed in.
    pub view: u64,
    /// The embedded QC, when the layout carries one.
    pub qc: Option<crate::h2_wire::H2QuorumCertificate>,
    /// Whether the layout ends in the 96-byte seal.
    pub sealed: bool,
}

/// `"N42H" || view u64 LE || qc || seal(96)`, the seal zero: what gov5's
/// `buildHeaderExtra(view, committedQC)` writes. An empty `qc` gives the
/// QC-less layout `"N42H" || view || seal(96)`.
pub fn gov5_header_extra(view: u64, qc: &[u8]) -> Vec<u8> {
    let mut extra = Vec::with_capacity(GOV5_H2_EXTRA_PREFIX + qc.len() + GOV5_H2_SEAL_BYTES);
    extra.extend_from_slice(b"N42H");
    extra.extend_from_slice(&view.to_le_bytes());
    extra.extend_from_slice(qc);
    extra.resize(extra.len() + GOV5_H2_SEAL_BYTES, 0);
    extra
}

/// Reads every layout gov5's `decodeHeaderExtra` accepts: `magic||view`,
/// `magic||view||seal`, `magic||view||QC||seal` and the legacy
/// `magic||view||QC`. A payload longer than the seal is read as QC followed
/// by seal first, then as a QC alone, as gov5 does.
pub fn decode_gov5_header_extra(extra: &[u8]) -> Result<Gov5HeaderExtra, Gov5BlockError> {
    let invalid = |reason: &str| Gov5BlockError::HeaderProfile(reason.to_owned());
    if extra.len() > n42_consensus::MAX_GOV5_HEADER_EXTRA_BYTES {
        return Err(invalid("header extra exceeds 4096 bytes"));
    }
    let header_view = gov5_extra_view(extra)?;
    let payload = &extra[GOV5_H2_EXTRA_PREFIX..];
    let (qc, sealed) = match payload.len() {
        0 => (None, false),
        GOV5_H2_SEAL_BYTES => (None, true),
        len if len > GOV5_H2_SEAL_BYTES => {
            match crate::h2_wire::decode_h2_qc(&payload[..len - GOV5_H2_SEAL_BYTES]) {
                Ok(qc) => (Some(qc), true),
                Err(_) => (
                    Some(
                        crate::h2_wire::decode_h2_qc(payload)
                            .map_err(|_| invalid("header extra QC does not decode"))?,
                    ),
                    false,
                ),
            }
        }
        _ => (
            Some(
                crate::h2_wire::decode_h2_qc(payload)
                    .map_err(|_| invalid("header extra QC does not decode"))?,
            ),
            false,
        ),
    };
    Ok(Gov5HeaderExtra {
        view: header_view,
        qc,
        sealed,
    })
}

fn gov5_extra_view(encoded: &[u8]) -> Result<u64, Gov5BlockError> {
    if encoded.len() < GOV5_H2_EXTRA_PREFIX || &encoded[..4] != b"N42H" {
        return Err(Gov5BlockError::HeaderProfile(
            "missing canonical N42H view prefix".to_owned(),
        ));
    }
    Ok(u64::from_le_bytes(
        encoded[4..GOV5_H2_EXTRA_PREFIX]
            .try_into()
            .expect("length checked before fixed-width conversion"),
    ))
}

/// The QC wire to embed in a header of `view` built on `parent`, or empty
/// for the QC-less layout.
///
/// The standard requires the QC to name an ancestor of the block, its parent
/// or lower. A committed block is an ancestor of every block on the locked
/// branch, so this holds for an honest leader by construction; it is still
/// checked against the remembered headers, because a header naming a
/// non-ancestor would be non-conformant. When the ancestor cannot be found
/// the QC-less layout is used and the miss is counted.
fn header_qc_wire(header_qc: Option<&Gov5HeaderQc>, view: u64, parent: B256) -> &[u8] {
    let Some(qc) = header_qc else {
        return &[];
    };
    if qc.view == 0 || qc.view >= view {
        metrics::counter!("n42_header_qc_omitted_total", "reason" => "view").increment(1);
        return &[];
    }
    let mut cursor = parent;
    for _ in 0..HEADER_QC_ANCESTOR_DEPTH {
        if cursor == qc.block_hash {
            return &qc.wire;
        }
        let Some(header) = n42_consensus::remembered_gov5_native_header(&cursor)
            .map(|native| native.header)
            .or_else(|| n42_consensus::remembered_profile1_header(&cursor).map(|p1| p1.header))
        else {
            break;
        };
        if header.number == 0 {
            break;
        }
        cursor = header.parent_hash;
    }
    metrics::counter!("n42_header_qc_omitted_total", "reason" => "ancestor").increment(1);
    tracing::warn!(target: "n42::interop::h2v4", %parent, qc_view = qc.view,
        qc_block = %qc.block_hash,
        "the held CommitQC names no remembered ancestor; header carries no QC");
    &[]
}

/// Returns the consensus view committed into a validated Gov5 H2 header.
///
/// Live Gov5 views are not block numbers: timeout views create gaps. Callers
/// walking an authenticated parent chain must therefore read this value from
/// each header instead of deriving `parent_view = child_view - 1`.
pub fn gov5_header_view(header: &Header) -> Result<u64, Gov5BlockError> {
    gov5_extra_view(header.extra_data.as_ref())
}

/// Converts a standard locally built Engine payload into the live gov5 H2
/// header shape. Transaction content and execution roots are unchanged; fields
/// Engine payloads cannot express (`ommers_hash`, `difficulty`), the H2
/// header-extra reservation, and pre-Shanghai withdrawals markers are
/// normalized before the new block hash is formed.
///
/// This form embeds no header QC: `magic || view || seal`. A producer passes
/// its last CommitQC through [`normalize_built_payload_for_gov5_h2`].
pub fn normalize_execution_payload_for_gov5_h2(
    execution: &ExecutionData,
    view: u64,
    state_root: B256,
    receipts_root: B256,
) -> Result<ExecutionData, Gov5BlockError> {
    normalize_execution_payload_for_gov5_h2_with_transactions_root(
        execution,
        view,
        state_root,
        receipts_root,
        None,
    )
}

/// [`normalize_execution_payload_for_gov5_h2`] for a payload whose transaction
/// root the caller already holds, as the builder of the payload does. The root
/// must be the one of `execution`'s transactions; it is not recomputed. Only
/// the native profile uses it; the early devnet profile ignores it.
pub fn normalize_execution_payload_for_gov5_h2_with_transactions_root(
    execution: &ExecutionData,
    view: u64,
    state_root: B256,
    receipts_root: B256,
    transactions_root: Option<B256>,
) -> Result<ExecutionData, Gov5BlockError> {
    normalize_payload(
        execution,
        view,
        state_root,
        receipts_root,
        transactions_root,
        None,
    )
}

fn normalize_payload(
    execution: &ExecutionData,
    view: u64,
    state_root: B256,
    receipts_root: B256,
    transactions_root: Option<B256>,
    header_qc: Option<&Gov5HeaderQc>,
) -> Result<ExecutionData, Gov5BlockError> {
    // A native chain-94 parent selects the post-Cancun profile. Never erase
    // executed withdrawals or the committee link as the early devnet profile does.
    if let Some(parent) = n42_consensus::remembered_gov5_native_header(&execution.parent_hash())
        && (parent.mobile_registry_root.is_some() || native_cancun_genesis(&parent))
    {
        return normalize_native_payload(
            execution,
            view,
            state_root,
            receipts_root,
            transactions_root,
            header_qc,
            &parent,
        );
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
        // The leader's direct broadcast looks the proposal up in the native
        // header registry; the pre-Shanghai profile must register it too.
        if let Ok(block) = reconstruct_gov5_block(execution) {
            remember_pre_shanghai_native_header(&block.header);
        }
        return Ok(execution.clone());
    }

    let mut block = execution
        .clone()
        .try_into_block::<N42TxEnvelope>()
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
    block.header.extra_data =
        gov5_header_extra(view, header_qc_wire(header_qc, view, block.header.parent_hash)).into();
    validate_gov5_interop_header(&block.header)
        .map_err(|error| Gov5BlockError::HeaderProfile(error.to_string()))?;

    let block_hash = block.header.hash_slow();
    remember_pre_shanghai_native_header(&block.header);
    Ok(ExecutionData::from_block_unchecked(block_hash, &block))
}

// Pre-Shanghai Gov5 headers carry no gov5-only field, so the native encoding
// equals alloy's and hashes to the same block hash.
fn remember_pre_shanghai_native_header(header: &alloy_consensus::Header) {
    let native = Gov5NativeHeader {
        header: header.clone(),
        mobile_registry_root: None,
    };
    remember_gov5_native_header(&native.encode());
}

// Gov5 seeds a Cancun block-zero header without mobileRegistryRoot. Its
// first live native block must still retain withdrawals and Cancun markers.
fn native_cancun_genesis(parent: &Gov5NativeHeader) -> bool {
    parent.header.number == 0
        && parent.mobile_registry_root.is_none()
        && parent.header.withdrawals_root.is_some()
        && parent.header.blob_gas_used.is_some()
        && parent.header.excess_blob_gas.is_some()
        && parent.header.parent_beacon_block_root.is_some()
}

/// A locally built payload normalized by
/// [`normalize_built_payload_for_gov5_h2`].
#[derive(Debug)]
pub struct NormalizedBuiltPayload {
    /// The normalized payload.
    pub payload: ExecutionData,
    /// [`gov5_normalization_preserves_execution_input`] of the payload handed
    /// in and this one.
    pub preserves_execution_input: bool,
}

/// [`normalize_execution_payload_for_gov5_h2_with_transactions_root`] for a
/// payload the caller hands over, with whether the normalization preserved
/// the execution input.
///
/// On the native profile with the transactions root given, only header
/// fields change and nothing reads the transactions. They are set aside
/// while the header is derived and the two payloads compared, and the same
/// list is the result's: a full block's 95,000 transactions are then neither
/// copied twice nor compared byte by byte on the leader's way to its own
/// validation.
///
/// `header_qc` is the last CommitQC the leader holds; the header embeds it
/// when it names the parent or a lower ancestor (sync standard section 3.2).
/// `None` gives the QC-less layout.
pub fn normalize_built_payload_for_gov5_h2(
    mut execution: ExecutionData,
    view: u64,
    state_root: B256,
    receipts_root: B256,
    transactions_root: Option<B256>,
    header_qc: Option<&Gov5HeaderQc>,
) -> Result<NormalizedBuiltPayload, Gov5BlockError> {
    let native = n42_consensus::remembered_gov5_native_header(&execution.parent_hash())
        .is_some_and(|parent| {
            parent.mobile_registry_root.is_some() || native_cancun_genesis(&parent)
        });
    let set_aside = (native && transactions_root.is_some())
        .then(|| std::mem::take(&mut execution.payload.as_v1_mut().transactions));
    let mut payload = normalize_payload(
        &execution,
        view,
        state_root,
        receipts_root,
        transactions_root,
        header_qc,
    )?;
    let preserves_execution_input =
        gov5_normalization_preserves_execution_input(&execution, &payload);
    if let Some(transactions) = set_aside {
        payload.payload.as_v1_mut().transactions = transactions;
    }
    Ok(NormalizedBuiltPayload {
        payload,
        preserves_execution_input,
    })
}

/// A conservative guard for reusing the builder's execution output after Gov5
/// changes the header hash. Only the native Cancun profile is eligible: the
/// pre-Shanghai normalization can also change the withdrawal body.
pub fn gov5_normalization_preserves_execution_input(
    original: &ExecutionData,
    normalized: &ExecutionData,
) -> bool {
    let before = original.payload.as_v1();
    let after = normalized.payload.as_v1();
    let empty_requests = |data: &ExecutionData| {
        data.sidecar
            .requests_hash()
            .is_none_or(|hash| hash == alloy_eips::eip7685::EMPTY_REQUESTS_HASH)
    };
    // Compare raw transaction bytes and EVM environment fields directly. A
    // full block reconstruction would decode 100k transactions twice here.
    original.payload.as_v4().is_none()
        && normalized.payload.as_v4().is_none()
        && original.payload.blob_gas_used().is_some()
        && original.payload.excess_blob_gas().is_some()
        && original.parent_beacon_block_root().is_some()
        && empty_requests(original)
        && empty_requests(normalized)
        && before.parent_hash == after.parent_hash
        && before.fee_recipient == after.fee_recipient
        && before.logs_bloom == after.logs_bloom
        && before.prev_randao == after.prev_randao
        && before.block_number == after.block_number
        && before.gas_limit == after.gas_limit
        && before.gas_used == after.gas_used
        && before.timestamp == after.timestamp
        && before.base_fee_per_gas == after.base_fee_per_gas
        && before.transactions == after.transactions
        && original.withdrawals() == normalized.withdrawals()
        && original.payload.blob_gas_used() == normalized.payload.blob_gas_used()
        && original.payload.excess_blob_gas() == normalized.payload.excess_blob_gas()
        && original.parent_beacon_block_root() == normalized.parent_beacon_block_root()
        && original.sidecar.versioned_hashes() == normalized.sidecar.versioned_hashes()
        && original.payload.block_access_list() == normalized.payload.block_access_list()
}

fn normalize_native_payload(
    execution: &ExecutionData,
    view: u64,
    state_root: B256,
    receipts_root: B256,
    transactions_root: Option<B256>,
    header_qc: Option<&Gov5HeaderQc>,
    parent: &Gov5NativeHeader,
) -> Result<ExecutionData, Gov5BlockError> {
    if parent.mobile_registry_root != Some(B256::ZERO) && !native_cancun_genesis(parent) {
        return Err(Gov5BlockError::PayloadReconstruction(
            "native mobile registry updates are not implemented".into(),
        ));
    }
    // Only header fields change, so the transactions stay raw bytes: decoding
    // and re-encoding a full block costs more than the rest of normalization.
    // The leader's engine validation of the result still decodes every one.
    let payload = execution.payload.clone();
    let mut block = match transactions_root {
        Some(root) => payload
            .into_block_with_sidecar_raw_with_transactions_root(&execution.sidecar, root),
        None => payload.into_block_with_sidecar_raw(&execution.sidecar),
    }
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
    block.header.extra_data =
        gov5_header_extra(view, header_qc_wire(header_qc, view, block.header.parent_hash)).into();
    validate_gov5_interop_header(&block.header)
        .map_err(|error| Gov5BlockError::HeaderProfile(error.to_string()))?;
    let native = Gov5NativeHeader {
        header: block.header.clone(),
        mobile_registry_root: Some(B256::ZERO),
    };
    let hash = remember_gov5_native_header(&native.encode());
    // The payload carries no ommers hash, difficulty or withdrawals root, so
    // the changed header fields it holds are the two roots, the extra data and
    // the hash; the requests hash lives in the sidecar. This is the payload
    // `ExecutionData::from_block_unchecked` derives from the edited block.
    let mut normalized = execution.clone();
    let fields = normalized.payload.as_v1_mut();
    fields.state_root = block.header.state_root;
    fields.receipts_root = block.header.receipts_root;
    fields.extra_data = block.header.extra_data;
    fields.block_hash = hash;
    if normalized.sidecar.requests_hash().is_some() {
        normalized.sidecar = match normalized.sidecar.cancun() {
            Some(cancun) => alloy_rpc_types_engine::ExecutionPayloadSidecar::v3(cancun.clone()),
            None => alloy_rpc_types_engine::ExecutionPayloadSidecar::none(),
        };
    }
    Ok(normalized)
}

/// Encodes a locally built execution payload as gov5's canonical block gossip
/// form. Auxiliary verifier/reward lists are empty because execution validity
/// and H2-v4 consensus authentication are carried independently.
pub fn encode_gov5_block_rlp(execution: &ExecutionData) -> Result<Vec<u8>, Gov5BlockError> {
    if let Some(header) = n42_consensus::remembered_profile1_header(&execution.block_hash()) {
        return encode_profile1_payload(execution, &header);
    }
    let block = reconstruct_gov5_block_raw(execution)?;
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
    // Reconstruction computes the trie root from these exact payload bytes and
    // authenticates it through the header hash. Check that every transaction is
    // canonical before reusing its bytes, so the same root also commits to the
    // decoded transactions. A reusable buffer avoids one allocation per tx and
    // a second full trie construction over re-encoded transactions.
    let mut canonical = Vec::new();
    for bytes in &block.body.transactions {
        let transaction = N42TxEnvelope::decode_2718_exact(bytes.as_ref())
            .map_err(|error| Gov5BlockError::PayloadReconstruction(error.to_string()))?;
        canonical.clear();
        transaction.encode_2718(&mut canonical);
        if canonical.as_slice() != bytes.as_ref() {
            return Err(Gov5BlockError::TransactionRootMismatch);
        }
    }

    let mut header_rlp = Vec::new();
    if let Some(native) = native {
        header_rlp = native.encode();
    } else {
        block.header.encode(&mut header_rlp);
    }
    let rewards = n42_consensus::gov5_withdrawals_to_rewards(
        block
            .body
            .withdrawals
            .as_ref()
            .map(|w| w.as_slice())
            .unwrap_or_default(),
    );
    Ok(encode_block_wire(&header_rlp, &block.body.transactions, &rewards))
}

/// A locally built payload turned into a sealed Profile-1 block.
#[derive(Debug)]
pub struct Profile1BuiltBlock {
    /// The payload under the Profile-1 hash, as the engine imports it.
    pub payload: ExecutionData,
    /// The sealed 24-element header (remembered in the native header
    /// registry under its hash).
    pub header: Profile1Header,
}

/// Turns a payload this node's builder assembled and executed into the
/// Profile-1 block of `view` (mixed production standard B1-B12), sealed by
/// `key` (B2, the leader of `view`).
///
/// The header carries the block's own execution results: `state_root` (the
/// QMDB root), `receipts_root` (the native receipts root, B7), and the
/// payload's own gas used and logs bloom. Deferred execution (A10/B13)
/// replaces exactly these four with the parent's results at this step and
/// nowhere else.
///
/// Fields that are execution inputs are checked, never changed: `prevRandao`
/// must already be zero (B3) and the requests hash `sha256("")` (AM24, B8).
/// Only header fields that do not enter execution are written: ommers hash
/// and difficulty zero (B3), the rewards root of the withdrawals (B6),
/// `slotNumber` = number (B10), the gap and `mobileRegistryRoot` (B11, B12),
/// and `extraData` = `N42H || view || qc || seal` (B1). `header_qc` is the
/// last CommitQC the leader holds; it is embedded when it names the parent
/// or a remembered ancestor and is below `view`, else the QC is absent.
pub fn seal_built_payload_for_profile1(
    execution: &ExecutionData,
    view: u64,
    header_qc: Option<&Gov5HeaderQc>,
    state_root: B256,
    receipts_root: B256,
    mobile_registry_root: B256,
    key: &n42_primitives::BlsSecretKey,
) -> Result<Profile1BuiltBlock, Gov5BlockError> {
    let invalid = |reason: String| Gov5BlockError::HeaderProfile(format!("profile 1: {reason}"));
    let block = execution
        .payload
        .clone()
        .into_block_with_sidecar_raw(&execution.sidecar)
        .map_err(|error| Gov5BlockError::PayloadReconstruction(error.to_string()))?;
    let mut header = block.header;
    if header.mix_hash != B256::ZERO {
        return Err(invalid(format!("prevRandao {} is not zero (B3)", header.mix_hash)));
    }
    if header.requests_hash != Some(n42_chainspec::profile1::PROFILE1_REQUESTS_HASH) {
        return Err(invalid(format!(
            "requestsHash {:?} is not sha256(\"\") (AM24)",
            header.requests_hash
        )));
    }
    if header.block_access_list_hash.is_some() {
        return Err(invalid("the payload carries a block access list".to_owned()));
    }
    let rewards = n42_consensus::gov5_withdrawals_to_rewards(
        block
            .body
            .withdrawals
            .as_ref()
            .map(|withdrawals| withdrawals.as_slice())
            .unwrap_or_default(),
    );
    header.ommers_hash = B256::ZERO;
    header.difficulty = U256::ZERO;
    header.state_root = state_root;
    header.receipts_root = receipts_root;
    header.withdrawals_root = Some(n42_consensus::gov5_native_rewards_root(&rewards));
    header.slot_number = Some(header.number);
    header.extra_data = n42_chainspec::profile1::profile1_extra(
        view,
        header_qc_wire(header_qc, view, header.parent_hash),
    );
    let mut sealed = Profile1Header {
        header,
        mobile_registry_root,
    };
    sealed
        .seal(key)
        .map_err(|error| invalid(error.to_string()))?;
    let hash = remember_gov5_native_header(&sealed.encode());
    // The payload carries no ommers hash, difficulty, withdrawals root,
    // slot or registry root; what changed in it is the two roots, the
    // extra data and the hash.
    let mut payload = execution.clone();
    let fields = payload.payload.as_v1_mut();
    fields.state_root = state_root;
    fields.receipts_root = receipts_root;
    fields.extra_data = sealed.header.extra_data.clone();
    fields.block_hash = hash;
    Ok(Profile1BuiltBlock {
        payload,
        header: sealed,
    })
}

/// The gov5 wire form of a Profile-1 payload under its remembered header:
/// the payload's transactions and rewards must be the ones the header
/// commits to.
fn encode_profile1_payload(
    execution: &ExecutionData,
    header: &Profile1Header,
) -> Result<Vec<u8>, Gov5BlockError> {
    // The payload's extra data is longer than Ethereum allows; only the body
    // is read here, the header is the remembered one.
    let mut payload = execution.payload.clone();
    payload.set_extra_data(Bytes::new());
    let block = payload
        .into_block_with_sidecar_raw(&execution.sidecar)
        .map_err(|error| Gov5BlockError::PayloadReconstruction(error.to_string()))?
        .try_map_transactions(|bytes| {
            N42TxEnvelope::decode_2718_exact(bytes.as_ref())
                .map_err(|error| Gov5BlockError::PayloadReconstruction(error.to_string()))
        })?;
    if calculate_transaction_root(&block.body.transactions) != header.header.transactions_root {
        return Err(Gov5BlockError::TransactionRootMismatch);
    }
    let rewards = n42_consensus::gov5_withdrawals_to_rewards(
        block
            .body
            .withdrawals
            .as_ref()
            .map(|withdrawals| withdrawals.as_slice())
            .unwrap_or_default(),
    );
    if Some(n42_consensus::gov5_native_rewards_root(&rewards)) != header.header.withdrawals_root {
        return Err(Gov5BlockError::RewardsRootMismatch);
    }
    Ok(encode_profile1_block_rlp(header, &block.body.transactions, &rewards))
}

/// Encodes a Profile-1 block in gov5's sealed-block wire form: the exact
/// 24-element header, the transactions (EIP-2718), an empty verifier list and
/// the rewards (B6). [`decode_profile1_block_rlp`] reads it back.
pub fn encode_profile1_block_rlp(
    header: &Profile1Header,
    transactions: &[N42TxEnvelope],
    rewards: &[Gov5Reward],
) -> Vec<u8> {
    let transactions: Vec<Bytes> = transactions
        .iter()
        .map(|transaction| transaction.encoded_2718().into())
        .collect();
    encode_block_wire(&header.encode(), &transactions, rewards)
}

/// `[header, transactions, verifiers (empty), rewards]` with the header
/// already encoded.
fn encode_block_wire(header_rlp: &[u8], transaction_bytes: &[Bytes], rewards: &[Gov5Reward]) -> Vec<u8> {
    let verifiers = Vec::<Bytes>::new();
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
        header_rlp.len() + alloy_rlp::list_length::<Bytes, Bytes>(transaction_bytes) + verifiers.length() + rewards_rlp.len();
    let mut encoded =
        Vec::with_capacity(alloy_rlp::length_of_length(payload_length) + payload_length);
    RlpHeader {
        list: true,
        payload_length,
    }
    .encode(&mut encoded);
    encoded.extend_from_slice(header_rlp);
    alloy_rlp::encode_list::<_, Bytes>(transaction_bytes, &mut encoded);
    verifiers.encode(&mut encoded);
    encoded.extend_from_slice(&rewards_rlp);
    encoded
}

/// Engine payloads do not carry `ommers_hash` or `difficulty`. Reth therefore
/// reconstructs Ethereum's empty-list ommers commitment, while live gov5 H2
/// headers deliberately commit to a zero ommers hash. Rebuild both permitted
/// gov5 H2 difficulty variants and let the authenticated payload hash select
/// the exact header; no operator-controlled guessing is involved.
fn reconstruct_gov5_block(
    execution: &ExecutionData,
) -> Result<alloy_consensus::Block<N42TxEnvelope>, Gov5BlockError> {
    reconstruct_gov5_block_raw(execution)?.try_map_transactions(|bytes| {
        N42TxEnvelope::decode_2718_exact(bytes.as_ref())
            .map_err(|error| Gov5BlockError::PayloadReconstruction(error.to_string()))
    })
}

/// Reconstruct the authenticated header and raw transaction body with one trie
/// calculation, including when the standard Ethereum header needs Gov5 fields.
fn reconstruct_gov5_block_raw(
    execution: &ExecutionData,
) -> Result<alloy_consensus::Block<Bytes>, Gov5BlockError> {
    let expected_hash = execution.block_hash();
    let extra_data = execution.payload.as_v1().extra_data.clone();
    let mut standard_execution = execution.clone();
    standard_execution.payload.set_extra_data(Bytes::new());
    let mut block = standard_execution
        .into_block_raw()
        .map_err(|error| Gov5BlockError::PayloadReconstruction(error.to_string()))?;
    block.header.extra_data = extra_data;
    // Match the old direct conversion before trying native header variants.
    // Ethereum's converter rejects extraData longer than 32 bytes; only Gov5's
    // reconstruction path permits its longer consensus metadata.
    if block.header.extra_data.len() <= 32
        && validate_gov5_interop_header(&block.header).is_ok()
        && block.header.hash_slow() == expected_hash
    {
        return Ok(block);
    }
    block.header.ommers_hash = B256::ZERO;

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
    decode_gov5_block_body(payload, header_rlp, header, native.mobile_registry_root)
}

/// Decodes a block of a Profile-1 chain: the gov5 wire form of
/// [`decode_gov5_block_rlp`] with the strict 24-element Profile-1 header
/// (mixed production standard B10-B12). Any other header layout is a
/// permanent [`Gov5BlockError::HeaderProfile`] refusal. The header rules that
/// need the parent (B1-B4, B8-B14, A23) are checked by the importer.
pub fn decode_profile1_block_rlp(encoded: &[u8]) -> Result<Gov5GossipBlock, Gov5BlockError> {
    let mut payload = encoded;
    let outer = RlpHeader::decode(&mut payload).map_err(|_| Gov5BlockError::InvalidRlp)?;
    if !outer.list || outer.payload_length != payload.len() {
        return Err(Gov5BlockError::InvalidRlp);
    }
    let header_rlp = take_rlp_item(&mut payload).ok_or(Gov5BlockError::InvalidRlp)?;
    let profile1 = Profile1Header::decode(header_rlp)
        .map_err(|error| Gov5BlockError::HeaderProfile(error.to_string()))?;
    let Profile1Header {
        header,
        mobile_registry_root,
    } = profile1;
    decode_gov5_block_body(payload, header_rlp, header, Some(mobile_registry_root))
}

/// Decodes with the header layout of the chain: Profile 1 when `profile1`.
pub fn decode_block_rlp_for(
    encoded: &[u8],
    profile1: bool,
) -> Result<Gov5GossipBlock, Gov5BlockError> {
    if profile1 {
        decode_profile1_block_rlp(encoded)
    } else {
        decode_gov5_block_rlp(encoded)
    }
}

/// The body items after the header, checked against the decoded header.
fn decode_gov5_block_body(
    mut payload: &[u8],
    header_rlp: &[u8],
    header: Header,
    mobile_registry_root: Option<B256>,
) -> Result<Gov5GossipBlock, Gov5BlockError> {
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
    // The block hash commits to the rewards only through the header. Without
    // this check a peer can append a reward to an honest body and keep its hash,
    // replacing the served copy and retiring the fetch for that hash. Same rule
    // as the import plan, which would only reject the body much later.
    let rewards_match = match header
        .withdrawals_root
        .filter(|_| n42_consensus::validate_gov5_replay_v2_header(&header).is_err())
    {
        Some(root) => n42_consensus::gov5_native_rewards_root(&rewards) == root,
        None => rewards.is_empty(),
    };
    if !rewards_match {
        return Err(Gov5BlockError::RewardsRootMismatch);
    }

    // Remember the exact encoding: the engine seals the block with this hash
    // and must be able to prove the header behind it.
    let block_hash = remember_gov5_native_header(header_rlp);
    Ok(Gov5GossipBlock {
        block_hash,
        header,
        transactions,
        rewards,
        mobile_registry_root,
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

fn decode_transactions(encoded: &[u8]) -> Result<Vec<N42TxEnvelope>, Gov5BlockError> {
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
            N42TxEnvelope::decode_2718(&mut tx_cursor).map_err(|_| Gov5BlockError::InvalidRlp)?;
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
    use alloy_consensus::{Block, BlockBody, TxEnvelope, proofs::calculate_transaction_root};
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
        let block: Block<N42TxEnvelope> = Block {
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

    /// A 0x50 transaction survives the gov5 H2 block wire: the codec carries
    /// any N42 envelope, and whether 0x50 is admitted stays the chain's
    /// `altSigTx` rule, enforced by the execution layer.
    #[test]
    fn native_normalization_edits_only_the_header_and_matches_a_full_rebuild() {
        use alloy_eips::eip4895::{Withdrawal, Withdrawals};
        let parent = Gov5NativeHeader {
            header: Header {
                number: 77,
                ..Default::default()
            },
            mobile_registry_root: Some(B256::ZERO),
        };
        let parent_hash = remember_gov5_native_header(&parent.encode());
        let key = ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]);
        let transactions: Vec<N42TxEnvelope> = (0..3)
            .map(|nonce| {
                N42TxEnvelope::AltSig(
                    n42_tx_types::TxAltSig {
                        chain_id: 941050,
                        nonce,
                        max_priority_fee_per_gas: 1,
                        max_fee_per_gas: 10,
                        gas_limit: 21_000,
                        to: Address::repeat_byte(0x22),
                        value: U256::from(1),
                        input: Bytes::new(),
                        access_list: Default::default(),
                        alg_type: n42_tx_types::ALG_ED25519,
                        pubkey: Bytes::copy_from_slice(key.verifying_key().as_bytes()),
                    }
                    .sign_ed25519(&key),
                )
            })
            .collect();
        let withdrawals = Withdrawals::new(vec![Withdrawal {
            index: 0,
            validator_index: 0,
            address: Address::repeat_byte(0x31),
            amount: 5,
        }]);
        let transactions_root = calculate_transaction_root(&transactions);
        let block: Block<N42TxEnvelope> = Block {
            header: Header {
                parent_hash,
                number: 78,
                base_fee_per_gas: Some(7),
                transactions_root,
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
                transactions,
                ommers: Vec::new(),
                withdrawals: Some(withdrawals),
            },
        };
        let execution = ExecutionData::from_block_unchecked(block.header.hash_slow(), &block);
        let (state_root, receipts_root) = (B256::repeat_byte(0x71), B256::repeat_byte(0x72));

        // The full rebuild this normalization replaced.
        let mut rebuilt = block.clone();
        let rewards = n42_consensus::gov5_withdrawals_to_rewards(
            rebuilt.body.withdrawals.as_ref().map(|w| w.as_slice()).unwrap_or_default(),
        );
        rebuilt.header.ommers_hash = B256::ZERO;
        rebuilt.header.difficulty = U256::ZERO;
        rebuilt.header.state_root = state_root;
        rebuilt.header.receipts_root = receipts_root;
        rebuilt.header.withdrawals_root = Some(n42_consensus::gov5_native_rewards_root(&rewards));
        rebuilt.header.requests_hash = None;
        let mut extra = b"N42H".to_vec();
        extra.extend_from_slice(&5u64.to_le_bytes());
        extra.resize(12 + GOV5_H2_SEAL_BYTES, 0);
        rebuilt.header.extra_data = extra.into();
        let native = Gov5NativeHeader {
            header: rebuilt.header.clone(),
            mobile_registry_root: Some(B256::ZERO),
        };
        let expected = ExecutionData::from_block_unchecked(native.hash(), &rebuilt);

        for root in [None, Some(transactions_root)] {
            let normalized = normalize_execution_payload_for_gov5_h2_with_transactions_root(
                &execution,
                5,
                state_root,
                receipts_root,
                root,
            )
            .unwrap();
            assert_eq!(normalized.payload, expected.payload);
            assert_eq!(normalized.sidecar.cancun(), expected.sidecar.cancun());
            assert_eq!(normalized.sidecar.prague(), expected.sidecar.prague());
            assert!(normalized.sidecar.requests_hash().is_none());
            assert!(gov5_normalization_preserves_execution_input(&execution, &normalized));
            // Handed over, the payload normalizes to the same one.
            let built = normalize_built_payload_for_gov5_h2(
                execution.clone(),
                5,
                state_root,
                receipts_root,
                root,
                None,
            )
            .unwrap();
            assert!(built.preserves_execution_input);
            assert_eq!(built.payload.payload, normalized.payload);
            assert_eq!(built.payload.sidecar.cancun(), normalized.sidecar.cancun());
            assert_eq!(built.payload.sidecar.prague(), normalized.sidecar.prague());
            assert_eq!(built.payload.payload.as_v1().transactions.len(), 3);
            let decoded =
                decode_gov5_block_rlp(&encode_gov5_block_rlp(&normalized).unwrap()).unwrap();
            assert_eq!(decoded.block_hash, normalized.block_hash());
            assert_eq!(decoded.transactions, block.body.transactions);
        }
    }

    /// A header `Extra` gov5 produced on chain 94 (block 13,560,375, view
    /// 11029): `magic || view || QC || seal`, the QC of view 11028 with a
    /// 7-validator bitmap, the seal the leader's BLS signature.
    const GOV5_CHAIN94_EXTRA: &str = "4e343248152b000000000000142b00000000000020000000910d74febb5dac309b7389bb53ebea19bf91a664a4cfc4d74c127c15428be4ee60000000977324f952eb3d8f92cd1a9f790dc0c4fbf518c57fc70ced923e4c36d7829610d8532f5912de3e6066fc3b22eca090cc022980b768946ebea5a8b583a2c09d98def9724bdc047462dac65210ba4876921cf217b1e340c0adeb3fdf59862b71020300000007003b9517bc3a0e87fcdda01e00ac99ef1458cff9f7de7e9701c6ab5618cc9dded960654c9ae9a754fb779f6ef188a35399ee1395be42a6b86c6d4e56807ec9938ef0c6a753510cf3c7cde0218487b5b32acda479cacb530a2af80efaffbc302cfc90";

    fn test_commit_qc(view: u64, block_hash: B256) -> n42_primitives::consensus::QuorumCertificate {
        use bitvec::prelude::{BitVec, Msb0};
        let key = n42_primitives::BlsSecretKey::key_gen(&[7u8; 32]).unwrap();
        let mut signers: BitVec<u8, Msb0> = BitVec::repeat(false, 7);
        for index in [0, 2, 3, 6] {
            signers.set(index, true);
        }
        n42_primitives::consensus::QuorumCertificate {
            view,
            block_hash,
            aggregate_signature: key.sign(b"commit"),
            signers,
        }
    }

    #[test]
    fn header_qc_matches_gov5_bytes() {
        let extra = alloy_primitives::hex::decode(GOV5_CHAIN94_EXTRA).unwrap();
        let decoded = decode_gov5_header_extra(&extra).unwrap();
        assert_eq!(decoded.view, 11029);
        assert!(decoded.sealed);
        let qc = decoded.qc.unwrap();
        assert_eq!(qc.view, 11028);
        assert_eq!(qc.signers_bitmap, [0x07, 0x00, 0x3b]);
        let wire = &extra[12..extra.len() - GOV5_H2_SEAL_BYTES];
        assert_eq!(crate::h2_wire::encode_h2_qc(&qc).unwrap(), wire);

        // Through the native certificate, as a producer holds it, the header
        // form is gov5's byte for byte, and so is the unsealed Extra.
        let native = crate::h2_bridge::quorum_certificate_from_h2(qc).unwrap();
        let header_qc = Gov5HeaderQc::from_commit_qc(&native).unwrap();
        assert_eq!(header_qc.view, 11028);
        assert_eq!(header_qc.wire, wire);
        let built = gov5_header_extra(11029, &header_qc.wire);
        assert_eq!(built[..extra.len() - GOV5_H2_SEAL_BYTES], extra[..extra.len() - GOV5_H2_SEAL_BYTES]);
        assert_eq!(built.len(), extra.len());
        assert!(built[extra.len() - GOV5_H2_SEAL_BYTES..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn header_extra_reads_every_layout() {
        let qc = test_commit_qc(6, B256::repeat_byte(3));
        let header_qc = Gov5HeaderQc::from_commit_qc(&qc).unwrap();
        let h2 = crate::h2_bridge::quorum_certificate_to_h2(&qc).unwrap();
        // view 8 + (4 + 32) + (4 + 96) + (4 + 2 + 1)
        assert_eq!(header_qc.wire.len(), 151);
        assert_eq!(h2.signers_bitmap, [7, 0, 0b0100_1101]);
        let mut prefix = b"N42H".to_vec();
        prefix.extend_from_slice(&7u64.to_le_bytes());

        let bare = decode_gov5_header_extra(&prefix).unwrap();
        assert_eq!((bare.view, bare.qc, bare.sealed), (7, None, false));
        let legacy = gov5_header_extra(7, &[]);
        assert_eq!(legacy, [prefix.clone(), vec![0; 96]].concat());
        let sealed = decode_gov5_header_extra(&legacy).unwrap();
        assert_eq!((sealed.qc, sealed.sealed), (None, true));
        let current = gov5_header_extra(7, &header_qc.wire);
        assert_eq!(current, [prefix.clone(), header_qc.wire.clone(), vec![0; 96]].concat());
        let with_qc = decode_gov5_header_extra(&current).unwrap();
        assert_eq!((with_qc.qc.as_ref(), with_qc.sealed), (Some(&h2), true));
        let qc_only = decode_gov5_header_extra(&[prefix.clone(), header_qc.wire.clone()].concat())
            .unwrap();
        assert_eq!((qc_only.qc.as_ref(), qc_only.sealed), (Some(&h2), false));
        // A non-zero seal is not looked at.
        let mut signed = current.clone();
        let len = signed.len();
        signed[len - 96..].fill(0xab);
        assert_eq!(decode_gov5_header_extra(&signed).unwrap().qc, Some(h2));

        assert!(decode_gov5_header_extra(&[prefix.clone(), vec![1; 40]].concat()).is_err());
        assert!(decode_gov5_header_extra(&[prefix.clone(), vec![1; 140]].concat()).is_err());
        assert!(decode_gov5_header_extra(b"N42H").is_err());
        assert!(decode_gov5_header_extra(&[b"XXXX".as_slice(), &[0; 104]].concat()).is_err());
        assert!(decode_gov5_header_extra(&[prefix, vec![0; 4096]].concat()).is_err());

        // The genesis sentinel is never embedded.
        assert!(
            Gov5HeaderQc::from_commit_qc(&n42_primitives::consensus::QuorumCertificate::genesis())
                .is_none()
        );
    }

    #[test]
    fn producer_embeds_a_commit_qc_naming_the_parent_or_lower() {
        let grandparent = Gov5NativeHeader {
            header: Header {
                number: 76,
                extra_data: gov5_header_extra(3, &[]).into(),
                ..Default::default()
            },
            mobile_registry_root: Some(B256::ZERO),
        };
        let grandparent_hash = remember_gov5_native_header(&grandparent.encode());
        let parent = Gov5NativeHeader {
            header: Header {
                number: 77,
                parent_hash: grandparent_hash,
                extra_data: gov5_header_extra(4, &[]).into(),
                ..Default::default()
            },
            mobile_registry_root: Some(B256::ZERO),
        };
        let parent_hash = remember_gov5_native_header(&parent.encode());
        let block: Block<N42TxEnvelope> = Block {
            header: Header {
                parent_hash,
                number: 78,
                base_fee_per_gas: Some(7),
                transactions_root: calculate_transaction_root::<N42TxEnvelope>(&[]),
                withdrawals_root: Some(alloy_consensus::constants::EMPTY_ROOT_HASH),
                blob_gas_used: Some(0),
                excess_blob_gas: Some(0),
                parent_beacon_block_root: Some(B256::repeat_byte(0x44)),
                ..Default::default()
            },
            body: BlockBody {
                transactions: Vec::new(),
                ommers: Vec::new(),
                withdrawals: Some(Default::default()),
            },
        };
        let execution = ExecutionData::from_block_unchecked(block.header.hash_slow(), &block);
        let build = |qc: Option<&Gov5HeaderQc>| {
            normalize_built_payload_for_gov5_h2(
                execution.clone(),
                5,
                B256::repeat_byte(0x71),
                B256::repeat_byte(0x72),
                None,
                qc,
            )
            .unwrap()
            .payload
        };

        let legacy = build(None);
        assert_eq!(legacy.payload.as_v1().extra_data.as_ref(), gov5_header_extra(5, &[]));
        for named in [parent_hash, grandparent_hash] {
            let qc = Gov5HeaderQc::from_commit_qc(&test_commit_qc(4, named)).unwrap();
            let built = build(Some(&qc));
            let extra = built.payload.as_v1().extra_data.clone();
            assert_eq!(extra.as_ref(), gov5_header_extra(5, &qc.wire));
            assert_ne!(built.block_hash(), legacy.block_hash());
            let decoded = decode_gov5_header_extra(&extra).unwrap();
            assert_eq!(decoded.view, 5);
            assert_eq!(decoded.qc.unwrap().block_hash, named);
            // A follower recovers the same block, and its hash, from the wire.
            let wire = decode_gov5_block_rlp(&encode_gov5_block_rlp(&built).unwrap()).unwrap();
            assert_eq!(wire.block_hash, built.block_hash());
            assert_eq!(gov5_header_view(&wire.header).unwrap(), 5);
            assert_eq!(reconstruct_gov5_block(&built).unwrap().header.extra_data, extra);
        }
        // Not an ancestor, or not below the block's view: no QC.
        let stranger = Gov5HeaderQc::from_commit_qc(&test_commit_qc(4, B256::repeat_byte(0x5e)))
            .unwrap();
        assert_eq!(build(Some(&stranger)).block_hash(), legacy.block_hash());
        let same_view = Gov5HeaderQc::from_commit_qc(&test_commit_qc(5, parent_hash)).unwrap();
        assert_eq!(build(Some(&same_view)).block_hash(), legacy.block_hash());
    }

    #[test]
    fn alt_sig_transaction_round_trips_through_gov5_block_wire() {
        let key = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let transaction = N42TxEnvelope::AltSig(
            n42_tx_types::TxAltSig {
                chain_id: 941050,
                nonce: 0,
                max_priority_fee_per_gas: 1,
                max_fee_per_gas: 10,
                gas_limit: 21_000,
                to: Address::repeat_byte(0x22),
                value: U256::from(1),
                input: Bytes::new(),
                access_list: Default::default(),
                alg_type: n42_tx_types::ALG_ED25519,
                pubkey: Bytes::copy_from_slice(key.verifying_key().as_bytes()),
            }
            .sign_ed25519(&key),
        );
        let root = calculate_transaction_root(std::slice::from_ref(&transaction));
        let original = decode_gov5_block_rlp(&block_fixture_with_transactions(
            root,
            vec![transaction.encoded_2718().into()],
        ))
        .unwrap();
        assert_eq!(original.transactions, vec![transaction]);
        let block: Block<N42TxEnvelope> = Block {
            header: original.header,
            body: BlockBody {
                transactions: original.transactions,
                ommers: Vec::new(),
                withdrawals: None,
            },
        };
        let execution = ExecutionData::from_block_unchecked(original.block_hash, &block);
        let recovered = decode_gov5_block_rlp(&encode_gov5_block_rlp(&execution).unwrap()).unwrap();
        assert_eq!(recovered.block_hash, original.block_hash);
        assert_eq!(recovered.transactions, block.body.transactions);
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
            gov5_native_receipts_root::<alloy_consensus::TxType>(&[]),
        )
        .unwrap();
        assert_ne!(normalized.block_hash(), standard_hash);
        // Off the native profile a handed-over payload takes the same path,
        // whatever root comes with it, and is not reusable under the new hash.
        for root in [None, Some(block.header.transactions_root)] {
            let built = normalize_built_payload_for_gov5_h2(
                standard.clone(),
                19,
                native_state_root,
                gov5_native_receipts_root::<alloy_consensus::TxType>(&[]),
                root,
                None,
            )
            .unwrap();
            assert_eq!(built.payload.payload, normalized.payload);
            assert_eq!(
                built.preserves_execution_input,
                gov5_normalization_preserves_execution_input(&standard, &normalized)
            );
        }
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
    fn pre_shanghai_leader_payload_is_registered_for_direct_broadcast() {
        let block: Block<TxEnvelope> = Block {
            header: Header {
                number: 2,
                gas_limit: 30_000_000,
                timestamp: 1_794_902_942,
                transactions_root: calculate_transaction_root::<TxEnvelope>(&[]),
                base_fee_per_gas: Some(7),
                ..Default::default()
            },
            body: BlockBody::default(),
        };
        let standard = ExecutionData::from_block_unchecked(block.header.hash_slow(), &block);
        let normalized = normalize_execution_payload_for_gov5_h2(
            &standard,
            3,
            B256::repeat_byte(0x31),
            gov5_native_receipts_root::<alloy_consensus::TxType>(&[]),
        )
        .unwrap();
        // The leader refuses to propose a block whose native header it cannot
        // look up, so both normalization paths must register it.
        let hash = normalized.block_hash();
        let raw = n42_consensus::gov5_native_header_rlp(&hash).unwrap();
        assert_eq!(keccak256(&raw[..]), hash);
        let again = normalize_execution_payload_for_gov5_h2(
            &normalized,
            3,
            B256::repeat_byte(0x31),
            gov5_native_receipts_root::<alloy_consensus::TxType>(&[]),
        )
        .unwrap();
        assert_eq!(again.block_hash(), hash);
    }

    #[test]
    fn cancun_genesis_bootstraps_the_native_first_child() {
        let parent = Gov5NativeHeader {
            header: Header {
                number: 0,
                withdrawals_root: Some(alloy_consensus::constants::EMPTY_ROOT_HASH),
                blob_gas_used: Some(0),
                excess_blob_gas: Some(0),
                parent_beacon_block_root: Some(B256::ZERO),
                ..Default::default()
            },
            // Gov5's genesis builder does not stamp mobileRegistryRoot.
            mobile_registry_root: None,
        };
        let parent_hash = remember_gov5_native_header(&parent.encode());
        let block: Block<TxEnvelope> = Block {
            header: Header {
                parent_hash,
                number: 1,
                transactions_root: calculate_transaction_root::<TxEnvelope>(&[]),
                withdrawals_root: Some(alloy_consensus::constants::EMPTY_ROOT_HASH),
                blob_gas_used: Some(0),
                excess_blob_gas: Some(0),
                parent_beacon_block_root: Some(B256::ZERO),
                ..Default::default()
            },
            body: BlockBody {
                withdrawals: Some(Default::default()),
                ..Default::default()
            },
        };
        let execution = ExecutionData::from_block_unchecked(block.header.hash_slow(), &block);
        let normalized = normalize_execution_payload_for_gov5_h2(
            &execution,
            1,
            B256::repeat_byte(0x71),
            keccak256([]),
        )
        .unwrap();
        assert!(gov5_normalization_preserves_execution_input(
            &execution,
            &normalized
        ));
        let mut changed = normalized.clone();
        changed.payload.as_v1_mut().timestamp += 1;
        assert!(!gov5_normalization_preserves_execution_input(
            &execution, &changed
        ));
        let decoded = decode_gov5_block_rlp(&encode_gov5_block_rlp(&normalized).unwrap()).unwrap();
        assert_eq!(decoded.header.blob_gas_used, Some(0));
        assert_eq!(decoded.header.excess_blob_gas, Some(0));
        assert_eq!(decoded.header.parent_beacon_block_root, Some(B256::ZERO));
        assert_eq!(decoded.header.withdrawals_root, Some(keccak256([])));
        assert_eq!(decoded.mobile_registry_root, Some(B256::ZERO));
    }

    fn append_reward_to_block_rlp(encoded: &[u8]) -> Vec<u8> {
        let mut payload = encoded;
        RlpHeader::decode(&mut payload).unwrap();
        let header = take_rlp_item(&mut payload).unwrap();
        let transactions = take_rlp_item(&mut payload).unwrap();
        let verifiers = take_rlp_item(&mut payload).unwrap();
        let mut rewards = take_rlp_list_item(&mut payload).unwrap();
        RlpHeader::decode(&mut rewards).unwrap();

        let address = Address::repeat_byte(0x32);
        let amount = U256::from(1u64);
        let mut items = rewards.to_vec();
        RlpHeader {
            list: true,
            payload_length: address.length() + amount.length(),
        }
        .encode(&mut items);
        address.encode(&mut items);
        amount.encode(&mut items);

        let mut rewards_rlp = Vec::new();
        RlpHeader {
            list: true,
            payload_length: items.len(),
        }
        .encode(&mut rewards_rlp);
        rewards_rlp.extend_from_slice(&items);

        let mut forged = Vec::new();
        RlpHeader {
            list: true,
            payload_length: header.len()
                + transactions.len()
                + verifiers.len()
                + rewards_rlp.len(),
        }
        .encode(&mut forged);
        forged.extend_from_slice(header);
        forged.extend_from_slice(transactions);
        forged.extend_from_slice(verifiers);
        forged.extend_from_slice(&rewards_rlp);
        forged
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

        // A reward appended to the honest body leaves the block hash unchanged.
        let forged = append_reward_to_block_rlp(&encoded);
        let error = decode_gov5_block_rlp(&forged).unwrap_err();
        assert!(matches!(error, Gov5BlockError::RewardsRootMismatch));
        assert!(!error.is_permanent());

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
            gov5_native_receipts_root::<alloy_consensus::TxType>(&[]),
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

        let native_receipts_root = gov5_native_receipts_root::<alloy_consensus::TxType>(&[]);
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
