//! Authenticated, replay-protected cold-start bundles for Gov5 H2-v4 participants.

use crate::persistence::{ConsensusSnapshot, load_consensus_state, save_consensus_state};
use crate::validator_peers::expected_validator_peer_ids_with_policy;
use alloy_primitives::{B256, keccak256};
use n42_chainspec::ValidatorInfo;
use n42_consensus::{
    ValidatorSet,
    protocol::quorum::{
        ConsensusSigningProfile, verify_commit_qc_with_profile, verify_qc_any_domain_with_profile,
    },
};
use n42_network::verify_finalized_range_stream;
use n42_primitives::{QuorumCertificate, consensus::H2V4ChainIdentity};
use n42_twig_core::qmdb_compat::verify_portable_stream;
use serde::{Deserialize, Serialize};
use std::{
    fs::OpenOptions,
    io::{self, BufReader, Cursor, Write},
    path::{Path, PathBuf},
};

pub const BOOTSTRAP_BUNDLE_VERSION: u32 = 1;
const BOOTSTRAP_SNAPSHOT_VERSION: u32 = 5;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Gov5BootstrapPayload {
    pub format_version: u32,
    /// Strictly increasing operator-issued sequence number.
    pub sequence: u64,
    /// Unique, non-zero issuance nonce. It prevents two bundles at one sequence
    /// from being treated as equivalent.
    pub replay_nonce: B256,
    pub issued_at_unix_secs: u64,
    pub chain_id: u64,
    pub genesis_hash: B256,
    pub checkpoint_block: u64,
    pub checkpoint_block_hash: B256,
    pub checkpoint_qmdb_root: B256,
    pub commit_qc: QuorumCertificate,
    pub locked_qc: QuorumCertificate,
    pub next_view: u64,
    pub validators: Vec<ValidatorInfo>,
    pub fault_tolerance: u32,
    pub last_execution_validated_view: u64,
    /// Authenticated block-zero range used to select native versus replay-v2
    /// execution genesis semantics.
    #[serde(with = "hex_bytes")]
    pub genesis_range: Vec<u8>,
    /// Full authenticated range from block one through `checkpoint_block`.
    #[serde(with = "hex_bytes")]
    pub finalized_range: Vec<u8>,
    /// Portable replay-v2 QMDB slot log at `checkpoint_block`.
    #[serde(with = "hex_bytes")]
    pub qmdb_checkpoint: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Gov5BootstrapBundle {
    pub payload: Gov5BootstrapPayload,
    pub content_digest: B256,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct BootstrapReceipt {
    format_version: u32,
    sequence: u64,
    replay_nonce: B256,
    content_digest: B256,
    checkpoint_block: u64,
    checkpoint_block_hash: B256,
    checkpoint_qmdb_root: B256,
}

#[derive(Clone, Debug)]
pub struct MaterializedBootstrap {
    pub snapshot: ConsensusSnapshot,
    pub genesis_range_path: PathBuf,
    pub finalized_range_path: PathBuf,
    pub qmdb_checkpoint_path: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum BootstrapError {
    #[error("bootstrap I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("invalid bootstrap JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported bootstrap bundle version {0}")]
    UnsupportedVersion(u32),
    #[error("bootstrap content digest mismatch")]
    DigestMismatch,
    #[error("bootstrap chain identity does not match the configured chain")]
    ChainIdentityMismatch,
    #[error("bootstrap sequence and replay nonce must be non-zero")]
    MissingReplayProtection,
    #[error("bootstrap checkpoint identity is incomplete")]
    IncompleteCheckpoint,
    #[error("bootstrap consensus fields are inconsistent: {0}")]
    InconsistentConsensus(String),
    #[error("bootstrap validator set does not exactly match the configured validator set")]
    ValidatorSetMismatch,
    #[error("bootstrap validator peer bindings are invalid: {0}")]
    PeerBindings(String),
    #[error("bootstrap CommitQC is invalid: {0}")]
    InvalidCommitQc(String),
    #[error("bootstrap locked QC is invalid: {0}")]
    InvalidLockedQc(String),
    #[error("bootstrap execution artifacts are invalid: {0}")]
    InvalidArtifacts(String),
    #[error("bootstrap would regress persisted state: {0}")]
    Regression(String),
}

impl Gov5BootstrapBundle {
    pub fn from_path(path: &Path) -> Result<Self, BootstrapError> {
        let bytes = std::fs::read(path).map_err(|source| BootstrapError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    pub fn digest_payload(payload: &Gov5BootstrapPayload) -> Result<B256, BootstrapError> {
        Ok(keccak256(serde_json::to_vec(payload)?))
    }

    pub fn seal(payload: Gov5BootstrapPayload) -> Result<Self, BootstrapError> {
        let content_digest = Self::digest_payload(&payload)?;
        Ok(Self {
            payload,
            content_digest,
        })
    }

    pub fn verify(
        &self,
        expected_identity: H2V4ChainIdentity,
        expected_validators: &[ValidatorInfo],
        expected_fault_tolerance: u32,
    ) -> Result<ConsensusSnapshot, BootstrapError> {
        let payload = &self.payload;
        if payload.format_version != BOOTSTRAP_BUNDLE_VERSION {
            return Err(BootstrapError::UnsupportedVersion(payload.format_version));
        }
        if Self::digest_payload(payload)? != self.content_digest {
            return Err(BootstrapError::DigestMismatch);
        }
        if payload.sequence == 0 || payload.replay_nonce == B256::ZERO {
            return Err(BootstrapError::MissingReplayProtection);
        }
        if payload.chain_id != expected_identity.chain_id
            || payload.genesis_hash != expected_identity.genesis_hash
        {
            return Err(BootstrapError::ChainIdentityMismatch);
        }
        if payload.checkpoint_block == 0
            || payload.checkpoint_block_hash == B256::ZERO
            || payload.checkpoint_qmdb_root == B256::ZERO
            || payload.genesis_range.is_empty()
            || payload.finalized_range.is_empty()
            || payload.qmdb_checkpoint.is_empty()
        {
            return Err(BootstrapError::IncompleteCheckpoint);
        }
        if !same_validators(&payload.validators, expected_validators)
            || payload.fault_tolerance != expected_fault_tolerance
        {
            return Err(BootstrapError::ValidatorSetMismatch);
        }
        expected_validator_peer_ids_with_policy(&payload.validators, false)
            .map_err(|error| BootstrapError::PeerBindings(error.to_string()))?;

        let validator_set = ValidatorSet::try_new(&payload.validators, payload.fault_tolerance)
            .map_err(|error| BootstrapError::PeerBindings(error.to_string()))?;
        let profile = ConsensusSigningProfile::H2V4(expected_identity);
        if payload.commit_qc.block_hash != payload.checkpoint_block_hash
            || payload.commit_qc.view != payload.last_execution_validated_view
            || payload.locked_qc.view < payload.commit_qc.view
            || payload.next_view <= payload.locked_qc.view
        {
            return Err(BootstrapError::InconsistentConsensus(
                "checkpoint, CommitQC, locked QC, execution head, and next view do not form one monotonic state"
                    .to_string(),
            ));
        }
        verify_commit_qc_with_profile(&payload.commit_qc, &validator_set, &B256::ZERO, profile)
            .map_err(|error| BootstrapError::InvalidCommitQc(error.to_string()))?;
        if payload.locked_qc != payload.commit_qc {
            verify_qc_any_domain_with_profile(
                &payload.locked_qc,
                &validator_set,
                &B256::ZERO,
                profile,
            )
            .map_err(|error| BootstrapError::InvalidLockedQc(error.to_string()))?;
        }
        self.verify_execution_artifacts()?;

        let snapshot = ConsensusSnapshot {
            version: BOOTSTRAP_SNAPSHOT_VERSION,
            current_view: payload.next_view,
            locked_qc: payload.locked_qc.clone(),
            last_committed_qc: payload.commit_qc.clone(),
            consecutive_timeouts: 0,
            scheduled_epoch_transition: None,
            authorized_verifiers: Vec::new(),
            committed_block_count: payload.checkpoint_block,
            // A bootstrap starts after the certified view. Conservatively
            // suppress both vote phases through the lock view.
            last_voted_view: payload.locked_qc.view,
            last_commit_voted_view: payload.locked_qc.view,
            current_epoch_validators: Some((
                0,
                payload.validators.clone(),
                payload.fault_tolerance,
            )),
            execution_validated_head_view: payload.last_execution_validated_view,
            execution_validated_head_hash: payload.checkpoint_block_hash,
        };
        snapshot
            .validate()
            .map_err(BootstrapError::InconsistentConsensus)?;
        Ok(snapshot)
    }

    /// Authenticates every embedded execution artifact before anything is
    /// materialized in the participant data directory.
    fn verify_execution_artifacts(&self) -> Result<(), BootstrapError> {
        let payload = &self.payload;
        let qmdb = verify_portable_stream(
            Cursor::new(&payload.qmdb_checkpoint),
            payload.chain_id,
            &payload.genesis_hash.into(),
        )
        .map_err(|error| BootstrapError::InvalidArtifacts(error.to_string()))?;
        let range = verify_finalized_range_stream(
            BufReader::new(Cursor::new(&payload.finalized_range)),
            payload.chain_id,
            payload.genesis_hash,
        )
        .map_err(|error| BootstrapError::InvalidArtifacts(error.to_string()))?;
        let genesis = verify_finalized_range_stream(
            BufReader::new(Cursor::new(&payload.genesis_range)),
            payload.chain_id,
            payload.genesis_hash,
        )
        .map_err(|error| BootstrapError::InvalidArtifacts(error.to_string()))?;
        if genesis.from_block != 0 || genesis.to_block != 0 {
            return Err(BootstrapError::InvalidArtifacts(
                "genesis range must contain exactly block zero".to_string(),
            ));
        }
        if range.from_block != 1
            || range.to_block != payload.checkpoint_block
            || range.last_block_hash != payload.checkpoint_block_hash
            || range.last_state_root != payload.checkpoint_qmdb_root
            || qmdb.block_number != payload.checkpoint_block
            || B256::from(qmdb.block_hash) != payload.checkpoint_block_hash
            || B256::from(qmdb.root) != payload.checkpoint_qmdb_root
        {
            return Err(BootstrapError::InvalidArtifacts(
                "finalized range, QMDB checkpoint, and certified checkpoint do not identify one execution head"
                    .to_string(),
            ));
        }
        Ok(())
    }

    /// Verifies anti-regression state and atomically materializes all bundle
    /// components. Reapplying the byte-identical bundle is idempotent.
    pub fn materialize(
        &self,
        data_dir: &Path,
        snapshot: ConsensusSnapshot,
    ) -> Result<MaterializedBootstrap, BootstrapError> {
        std::fs::create_dir_all(data_dir).map_err(|source| BootstrapError::Io {
            path: data_dir.to_path_buf(),
            source,
        })?;
        let receipt_path = data_dir.join("gov5-bootstrap-receipt.json");
        let mut same_bundle_reapply = false;
        if let Some(receipt) = load_json_optional::<BootstrapReceipt>(&receipt_path)? {
            if receipt.sequence > self.payload.sequence {
                return Err(BootstrapError::Regression(format!(
                    "persisted bundle sequence {} is newer than {}",
                    receipt.sequence, self.payload.sequence
                )));
            }
            if receipt.sequence == self.payload.sequence
                && (receipt.content_digest != self.content_digest
                    || receipt.replay_nonce != self.payload.replay_nonce)
            {
                return Err(BootstrapError::Regression(
                    "a different bundle already used this sequence".to_string(),
                ));
            }
            same_bundle_reapply = receipt.sequence == self.payload.sequence
                && receipt.content_digest == self.content_digest
                && receipt.replay_nonce == self.payload.replay_nonce;
        }

        let snapshot_path = data_dir.join("consensus_state.json");
        let existing_snapshot =
            load_consensus_state(&snapshot_path).map_err(|source| BootstrapError::Io {
                path: snapshot_path.clone(),
                source,
            })?;
        let effective_snapshot = if let Some(existing) = existing_snapshot {
            if existing.current_view > snapshot.current_view
                || existing.last_committed_qc.view > snapshot.last_committed_qc.view
                || existing.committed_block_count > snapshot.committed_block_count
            {
                if same_bundle_reapply {
                    existing
                } else {
                    return Err(BootstrapError::Regression(format!(
                        "persisted snapshot view/block {}/{} is newer than bundle {}/{}",
                        existing.current_view,
                        existing.committed_block_count,
                        snapshot.current_view,
                        snapshot.committed_block_count
                    )));
                }
            } else {
                if existing.current_view == snapshot.current_view
                    && (existing.last_committed_qc.block_hash
                        != snapshot.last_committed_qc.block_hash
                        || existing.execution_validated_head_hash
                            != snapshot.execution_validated_head_hash)
                {
                    return Err(BootstrapError::Regression(
                        "persisted snapshot conflicts with the bundle at the same view".to_string(),
                    ));
                }
                snapshot
            }
        } else {
            snapshot
        };

        let asset_dir = data_dir.join("gov5-bootstrap");
        std::fs::create_dir_all(&asset_dir).map_err(|source| BootstrapError::Io {
            path: asset_dir.clone(),
            source,
        })?;
        let genesis_range_path = asset_dir.join("genesis-range.v1");
        let finalized_range_path = asset_dir.join("finalized-range.v1");
        let qmdb_checkpoint_path = asset_dir.join("qmdb-checkpoint.portable");
        atomic_write(&genesis_range_path, &self.payload.genesis_range)?;
        atomic_write(&finalized_range_path, &self.payload.finalized_range)?;
        atomic_write(&qmdb_checkpoint_path, &self.payload.qmdb_checkpoint)?;
        save_consensus_state(&snapshot_path, &effective_snapshot).map_err(|source| {
            BootstrapError::Io {
                path: snapshot_path,
                source,
            }
        })?;

        let receipt = BootstrapReceipt {
            format_version: BOOTSTRAP_BUNDLE_VERSION,
            sequence: self.payload.sequence,
            replay_nonce: self.payload.replay_nonce,
            content_digest: self.content_digest,
            checkpoint_block: self.payload.checkpoint_block,
            checkpoint_block_hash: self.payload.checkpoint_block_hash,
            checkpoint_qmdb_root: self.payload.checkpoint_qmdb_root,
        };
        atomic_write(&receipt_path, &serde_json::to_vec_pretty(&receipt)?)?;

        Ok(MaterializedBootstrap {
            snapshot: effective_snapshot,
            genesis_range_path,
            finalized_range_path,
            qmdb_checkpoint_path,
        })
    }
}

fn same_validators(left: &[ValidatorInfo], right: &[ValidatorInfo]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(left, right)| {
            left.address == right.address
                && left.bls_public_key == right.bls_public_key
                && left.p2p_peer_id == right.p2p_peer_id
        })
}

fn load_json_optional<T: for<'de> Deserialize<'de>>(
    path: &Path,
) -> Result<Option<T>, BootstrapError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(BootstrapError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), BootstrapError> {
    let tmp = path.with_extension("tmp");
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&tmp)
        .map_err(|source| BootstrapError::Io {
            path: tmp.clone(),
            source,
        })?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|source| BootstrapError::Io {
            path: tmp.clone(),
            source,
        })?;
    std::fs::rename(&tmp, path).map_err(|source| BootstrapError::Io {
        path: path.to_path_buf(),
        source,
    })
}

mod hex_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&hex::encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let value = String::deserialize(deserializer)?;
        hex::decode(value).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// gov5's persisted HotStuff state (v1 layout: view | timeouts |
    /// len+lockedQC | committedQC) of qs-node6 on chain 94 at block
    /// 13,560,375, view 11,031, taken while the node was stopped. The fleet
    /// runs without `hotstuff.interopV4`, so its QCs verify only under the
    /// legacy gov5 domains and the POP ciphersuite.
    const CHAIN94_HOTSTUFF_STATE_HEX: &str = "172b0000000000000000000097000000162b000000000000200000000e37dae9d0cbf1c8e09c335654dc4cae3e18760dade40039e0e693368cc796d760000000b9e4ba533cf17ae8bbed3f81e73d3c80ca7930213c0cac23efec13908cb3b8ed1a93d6af36af4e47c5a1407a2f48f31f119be6302a39814d38251fa2da6e01f6a90ae57ef6a396ff15b1b77b0a002028c2772701527ccf22015b8fdc670be74d0300000007003b162b000000000000200000000e37dae9d0cbf1c8e09c335654dc4cae3e18760dade40039e0e693368cc796d7600000008dca20e36a2d15c3c167a9f0e31850b1314cd616e6b28495cd5a43d227588ac16016bb39c9364b3378d40595d80075b609098a8831c576fc6e0fbd218226af48e1d7a7ef28d739a7afc7332079b2bbaae18ce26f28ff5d91d485edb9243fd0ab03000000070037";
    const CHAIN94_VALIDATORS: [(&str, &str); 7] = [
        (
            "0xd2a316a1cd3a777141cb7e5aace46fb01df90eac",
            "ae05c725f988dba181cd77d1d5eb2116ac25170fdc428e6e1dd23178d412451824a493ed5776a8ecb222fe6a18b68ab0",
        ),
        (
            "0xf7dc5c92fa9e812eb0c3157492da65457ae5de46",
            "81bf2bdc42c311f9f175d4cd58f5f79b3ca1ac5f981c6298b475bc08699dc9e22008ac8b649c0c6f336323cb77af06b9",
        ),
        (
            "0xa97ab21a7efbcb7995b67ed39287ba408850a9f1",
            "a2e796f97370d724573d50140254bfd0bde9dbe6f2432217dd7c7066ac9b44df50c5631453d6d37432e745b67bbc75a6",
        ),
        (
            "0x301631ce4b15f1a0d62a35d6c421dd5a845e555e",
            "80fceeef24a8aff98dfddb6843c87490cf71e10aa9912876121b00fd5c4a258fd37ee373f586822b769ec0905dbc261f",
        ),
        (
            "0xe5eb05fc41cdad4433f52988f83f78f46bb27e61",
            "8e172280ea9c9fba26560b2a2f1c4cbb012e5644d490188249fc7e11069ccffc66b049fdd5683c8405252e191f1ebef6",
        ),
        (
            "0x580339c31c60b974ac9f70e2f8307b2b4490f70a",
            "b93ffa07de1e9a2a2b6e564203c6753b09abfea3010e8404110e80f8678cdc9cd2098c76522248831317ef4829ddee8e",
        ),
        (
            "0x1ccde065f222f44709797be2908fc72b7801eda5",
            "9825e1ffc2471f5946445de41ee5dfe0d55eaaf7e48c04bbf5f70888d21dea121d044373459424cc44b895c89b18681a",
        ),
    ];

    #[test]
    fn chain94_persisted_commit_qc_verifies_only_under_the_gov5_legacy_profile() {
        use n42_network::{h2_wire::decode_h2_qc, quorum_certificate_from_h2};
        let bytes = alloy_primitives::hex::decode(CHAIN94_HOTSTUFF_STATE_HEX).unwrap();
        let view = u64::from_le_bytes(bytes[..8].try_into().unwrap());
        let locked_len = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
        let committed = &bytes[16 + locked_len..];
        let qc = quorum_certificate_from_h2(decode_h2_qc(committed).unwrap()).unwrap();
        assert_eq!(view, 11_031);
        assert_eq!(qc.view, 11_030);
        assert_eq!(
            qc.block_hash,
            "0x0e37dae9d0cbf1c8e09c335654dc4cae3e18760dade40039e0e693368cc796d7"
                .parse::<B256>()
                .unwrap()
        );
        let validators = CHAIN94_VALIDATORS
            .iter()
            .map(|(address, key)| ValidatorInfo {
                address: address.parse().unwrap(),
                bls_public_key: n42_primitives::BlsPublicKey::from_bytes(
                    &alloy_primitives::hex::decode(key)
                        .unwrap()
                        .try_into()
                        .unwrap(),
                )
                .unwrap(),
                p2p_peer_id: None,
            })
            .collect::<Vec<_>>();
        let set = ValidatorSet::try_new(&validators, 2).unwrap();
        verify_commit_qc_with_profile(&qc, &set, &B256::ZERO, ConsensusSigningProfile::Gov5Legacy)
            .expect("chain-94 commit QC must verify under the legacy gov5 domains");
        let identity = H2V4ChainIdentity {
            chain_id: 94,
            genesis_hash: "0xa2d2ff5d814552bb9a113b68ad7ed2b824fbb52caed42dbe573068845b57be99"
                .parse()
                .unwrap(),
        };
        assert!(
            verify_commit_qc_with_profile(
                &qc,
                &set,
                &B256::ZERO,
                ConsensusSigningProfile::H2V4(identity)
            )
            .is_err(),
            "the fleet does not sign with the chain-bound H2-v4 domains"
        );
        assert!(
            verify_commit_qc_with_profile(&qc, &set, &B256::ZERO, ConsensusSigningProfile::Native)
                .is_err()
        );
    }
    use alloy_consensus::{Header as ConsensusHeader, proofs::calculate_transaction_root};
    use alloy_primitives::Address;
    use alloy_primitives::Bytes;
    use alloy_rlp::{Encodable, Header as RlpHeader};
    use bitvec::prelude::BitVec;
    use libp2p::identity::Keypair;
    use n42_consensus::protocol::quorum::VoteCollector;
    use n42_network::gov5_native_receipts_root;
    use n42_primitives::BlsSecretKey;
    use n42_twig_core::qmdb_compat::{
        QmdbCompatTree, QmdbPortableSnapshot, QmdbSlotEntry, QmdbSlotSnapshot,
    };

    fn finalized_frame(chain_id: u64, genesis_hash: B256, header: &ConsensusHeader) -> Vec<u8> {
        let mut header_rlp = Vec::new();
        header.encode(&mut header_rlp);
        let mut block_rlp = Vec::new();
        RlpHeader {
            list: true,
            payload_length: header_rlp.len() + Vec::<Bytes>::new().length(),
        }
        .encode(&mut block_rlp);
        block_rlp.extend_from_slice(&header_rlp);
        Vec::<Bytes>::new().encode(&mut block_rlp);
        let block_hash = keccak256(&header_rlp);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"N42FRNG\x01");
        bytes.extend_from_slice(&chain_id.to_le_bytes());
        bytes.extend_from_slice(genesis_hash.as_slice());
        bytes.extend_from_slice(&header.number.to_le_bytes());
        bytes.extend_from_slice(&header.number.to_le_bytes());
        bytes.extend_from_slice(&1_u64.to_le_bytes());
        bytes.extend_from_slice(&header.number.to_le_bytes());
        bytes.extend_from_slice(block_hash.as_slice());
        bytes.extend_from_slice(header.parent_hash.as_slice());
        bytes.extend_from_slice(header.state_root.as_slice());
        bytes.extend_from_slice(header.receipts_root.as_slice());
        bytes.extend_from_slice(header.transactions_root.as_slice());
        for blob in [&header_rlp[..], &block_rlp[..], &[][..]] {
            bytes.extend_from_slice(&(blob.len() as u32).to_le_bytes());
            bytes.extend_from_slice(blob);
        }
        let digest = blake3::hash(&bytes);
        bytes.extend_from_slice(digest.as_bytes());
        bytes
    }

    fn execution_fixture(
        chain_id: u64,
    ) -> (H2V4ChainIdentity, B256, B256, Vec<u8>, Vec<u8>, Vec<u8>) {
        let empty_transactions_root =
            calculate_transaction_root::<alloy_consensus::TxEnvelope>(&[]);
        let genesis = ConsensusHeader {
            state_root: B256::repeat_byte(0x10),
            transactions_root: empty_transactions_root,
            receipts_root: empty_transactions_root,
            number: 0,
            ..Default::default()
        };
        let mut genesis_header_rlp = Vec::new();
        genesis.encode(&mut genesis_header_rlp);
        let genesis_hash = keccak256(&genesis_header_rlp);
        let mut tree = QmdbCompatTree::new();
        tree.set([0x41; 32], vec![0x42]);
        let snapshot = tree.snapshot();
        let root = B256::from(tree.root());
        let block = ConsensusHeader {
            parent_hash: genesis_hash,
            state_root: root,
            transactions_root: empty_transactions_root,
            receipts_root: gov5_native_receipts_root(&[]),
            number: 1,
            ..Default::default()
        };
        let mut block_header_rlp = Vec::new();
        block.encode(&mut block_header_rlp);
        let block_hash = keccak256(&block_header_rlp);
        let qmdb = QmdbPortableSnapshot {
            chain_id,
            genesis_hash: genesis_hash.into(),
            block_number: 1,
            block_hash: block_hash.into(),
            root: root.into(),
            slots: QmdbSlotSnapshot {
                next_slot: snapshot.next_slot,
                entries: snapshot
                    .entries
                    .into_iter()
                    .enumerate()
                    .map(|(slot, entry)| QmdbSlotEntry {
                        slot: slot as u64,
                        key: entry.key,
                        value: entry.value,
                        active: entry.active,
                    })
                    .collect(),
            },
        }
        .encode()
        .unwrap();
        (
            H2V4ChainIdentity {
                chain_id,
                genesis_hash,
            },
            block_hash,
            root,
            finalized_frame(chain_id, genesis_hash, &genesis),
            finalized_frame(chain_id, genesis_hash, &block),
            qmdb,
        )
    }

    fn fixture() -> (Gov5BootstrapBundle, H2V4ChainIdentity, Vec<ValidatorInfo>) {
        let (identity, block_hash, state_root, genesis_range, finalized_range, qmdb_checkpoint) =
            execution_fixture(1143);
        let keys: Vec<_> = (1..=4)
            .map(|seed| BlsSecretKey::key_gen(&[seed; 32]).unwrap())
            .collect();
        let validators: Vec<_> = keys
            .iter()
            .enumerate()
            .map(|(index, key)| ValidatorInfo {
                address: Address::repeat_byte(index as u8 + 1),
                bls_public_key: key.public_key(),
                p2p_peer_id: Some(
                    Keypair::generate_ed25519()
                        .public()
                        .to_peer_id()
                        .to_string(),
                ),
            })
            .collect();
        let set = ValidatorSet::try_new(&validators, 1).unwrap();
        let view = 20;
        let profile = ConsensusSigningProfile::H2V4(identity);
        let message = profile.commit_message(view, block_hash, B256::ZERO);
        let mut collector = VoteCollector::new(view, block_hash, set.len());
        for (index, key) in keys.iter().take(set.quorum_size()).enumerate() {
            collector
                .add_vote(index as u32, profile.sign(key, &message))
                .unwrap();
        }
        let commit_qc = collector
            .build_qc_with_profile_message(&set, &message, profile)
            .unwrap();
        let payload = Gov5BootstrapPayload {
            format_version: BOOTSTRAP_BUNDLE_VERSION,
            sequence: 7,
            replay_nonce: B256::repeat_byte(0x33),
            issued_at_unix_secs: 1,
            chain_id: identity.chain_id,
            genesis_hash: identity.genesis_hash,
            checkpoint_block: 1,
            checkpoint_block_hash: block_hash,
            checkpoint_qmdb_root: state_root,
            commit_qc: commit_qc.clone(),
            locked_qc: commit_qc,
            next_view: view + 1,
            validators: validators.clone(),
            fault_tolerance: 1,
            last_execution_validated_view: view,
            genesis_range,
            finalized_range,
            qmdb_checkpoint,
        };
        (
            Gov5BootstrapBundle::seal(payload).unwrap(),
            identity,
            validators,
        )
    }

    #[test]
    fn verifies_and_materializes_idempotently() {
        let (bundle, identity, validators) = fixture();
        let snapshot = bundle.verify(identity, &validators, 1).unwrap();
        assert_eq!(snapshot.current_view, 21);
        assert_eq!(snapshot.committed_block_count, 1);

        let dir = tempfile::tempdir().unwrap();
        let first = bundle.materialize(dir.path(), snapshot.clone()).unwrap();
        let second = bundle.materialize(dir.path(), snapshot).unwrap();
        assert_eq!(
            std::fs::read(first.qmdb_checkpoint_path).unwrap(),
            std::fs::read(second.qmdb_checkpoint_path).unwrap()
        );
    }

    #[test]
    fn rejects_digest_tampering_and_same_sequence_replacement() {
        let (mut bundle, identity, validators) = fixture();
        let snapshot = bundle.verify(identity, &validators, 1).unwrap();
        let dir = tempfile::tempdir().unwrap();
        bundle.materialize(dir.path(), snapshot).unwrap();

        bundle.payload.replay_nonce = B256::repeat_byte(0x55);
        assert!(matches!(
            bundle.verify(identity, &validators, 1),
            Err(BootstrapError::DigestMismatch)
        ));
        bundle.content_digest = Gov5BootstrapBundle::digest_payload(&bundle.payload).unwrap();
        let snapshot = bundle.verify(identity, &validators, 1).unwrap();
        assert!(matches!(
            bundle.materialize(dir.path(), snapshot),
            Err(BootstrapError::Regression(_))
        ));
    }

    #[test]
    fn rejects_regressing_existing_snapshot() {
        let (bundle, identity, validators) = fixture();
        let snapshot = bundle.verify(identity, &validators, 1).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut newer = snapshot.clone();
        newer.current_view += 1;
        newer.locked_qc.view += 1;
        save_consensus_state(&dir.path().join("consensus_state.json"), &newer).unwrap();
        assert!(matches!(
            bundle.materialize(dir.path(), snapshot),
            Err(BootstrapError::Regression(_))
        ));
    }

    #[test]
    fn rejects_missing_explicit_peer_bindings() {
        let (bundle, identity, mut validators) = fixture();
        validators[0].p2p_peer_id = None;
        let mut payload = bundle.payload;
        payload.validators = validators.clone();
        let bundle = Gov5BootstrapBundle::seal(payload).unwrap();
        assert!(matches!(
            bundle.verify(identity, &validators, 1),
            Err(BootstrapError::PeerBindings(_))
        ));
    }

    #[test]
    fn empty_signer_bitmap_is_never_accepted() {
        let (mut bundle, identity, validators) = fixture();
        bundle.payload.commit_qc.signers = BitVec::new();
        bundle.payload.locked_qc = bundle.payload.commit_qc.clone();
        bundle.content_digest = Gov5BootstrapBundle::digest_payload(&bundle.payload).unwrap();
        assert!(matches!(
            bundle.verify(identity, &validators, 1),
            Err(BootstrapError::InvalidCommitQc(_))
        ));
    }
}
