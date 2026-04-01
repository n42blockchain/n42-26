//! Compact binary evidence store for consensus proofs and mobile attestations.
//!
//! Stores [`ConsensusEvidence`] in an MDBX table keyed by block number (`u64 BE`).
//! The evidence links each committed block to its BLS aggregate signature (QC)
//! and optional mobile verification aggregate, enabling post-hoc verification
//! without the full consensus state.
//!
//! ## Binary layout
//!
//! ```text
//! [8B view LE][32B block_hash][96B agg_sig][2B signer_count LE][⌈n/8⌉B packed_signers]
//! [1B has_mobile]
//! [if has_mobile:
//!   [32B receipts_root][96B mob_sig][2B mob_count LE][⌈m/8⌉B mob_bits][8B created_at_ms LE]
//! ]
//! ```

use reth_libmdbx::{Database, DatabaseFlags, Environment, WriteFlags};

/// Consensus evidence for a committed block.
#[derive(Debug, Clone)]
pub struct ConsensusEvidence {
    pub view: u64,
    pub block_hash: [u8; 32],
    pub aggregate_signature: [u8; 96],
    pub signer_count: u16,
    pub packed_signers: Vec<u8>,
    pub mobile: Option<MobileEvidence>,
}

/// Mobile verification aggregate for a committed block (analogous to Ethereum's `SyncAggregate`).
#[derive(Debug, Clone)]
pub struct MobileEvidence {
    pub receipts_root: [u8; 32],
    pub aggregate_signature: [u8; 96],
    pub participant_count: u16,
    pub packed_participants: Vec<u8>,
    pub created_at_ms: u64,
}

// ---------------------------------------------------------------------------
// Compact binary codec
// ---------------------------------------------------------------------------

/// Minimum encoded size: 8 + 32 + 96 + 2 + 0 + 1 = 139 bytes (0 signers, no mobile).
const MIN_EVIDENCE_SIZE: usize = 8 + 32 + 96 + 2 + 1;

impl ConsensusEvidence {
    /// Encodes into the compact binary format described in the module docs.
    pub fn encode(&self) -> Vec<u8> {
        let signer_bytes = packed_byte_len(self.signer_count);
        debug_assert!(
            self.packed_signers.len() >= signer_bytes,
            "packed_signers too short: {} < {} for signer_count {}",
            self.packed_signers.len(), signer_bytes, self.signer_count,
        );
        let mobile_size = self.mobile.as_ref().map_or(0, |m| {
            debug_assert!(
                m.packed_participants.len() >= packed_byte_len(m.participant_count),
                "packed_participants too short for participant_count {}",
                m.participant_count,
            );
            32 + 96 + 2 + packed_byte_len(m.participant_count) + 8
        });
        let mut buf = Vec::with_capacity(MIN_EVIDENCE_SIZE + signer_bytes + mobile_size);

        buf.extend_from_slice(&self.view.to_le_bytes());
        buf.extend_from_slice(&self.block_hash);
        buf.extend_from_slice(&self.aggregate_signature);
        buf.extend_from_slice(&self.signer_count.to_le_bytes());
        buf.extend_from_slice(&self.packed_signers[..signer_bytes]);

        match &self.mobile {
            None => buf.push(0x00),
            Some(m) => {
                buf.push(0x01);
                buf.extend_from_slice(&m.receipts_root);
                buf.extend_from_slice(&m.aggregate_signature);
                buf.extend_from_slice(&m.participant_count.to_le_bytes());
                let mob_bytes = packed_byte_len(m.participant_count);
                buf.extend_from_slice(&m.packed_participants[..mob_bytes]);
                buf.extend_from_slice(&m.created_at_ms.to_le_bytes());
            }
        }
        buf
    }

    /// Decodes from the compact binary format.
    pub fn decode(bytes: &[u8]) -> Result<Self, EvidenceDecodeError> {
        let mut pos = 0;

        let view = read_u64_le(bytes, &mut pos)?;
        let block_hash = read_array::<32>(bytes, &mut pos)?;
        let aggregate_signature = read_array::<96>(bytes, &mut pos)?;
        let signer_count = read_u16_le(bytes, &mut pos)?;
        let signer_bytes = packed_byte_len(signer_count);
        check_remaining(bytes, pos, signer_bytes + 1)?;
        let packed_signers = bytes[pos..pos + signer_bytes].to_vec();
        pos += signer_bytes;

        let has_mobile = bytes[pos];
        pos += 1;

        let mobile = if has_mobile != 0 {
            let receipts_root = read_array::<32>(bytes, &mut pos)?;
            let mob_sig = read_array::<96>(bytes, &mut pos)?;
            let mob_count = read_u16_le(bytes, &mut pos)?;
            let mob_bytes = packed_byte_len(mob_count);
            check_remaining(bytes, pos, mob_bytes + 8)?;
            let packed_participants = bytes[pos..pos + mob_bytes].to_vec();
            pos += mob_bytes;
            let created_at_ms = read_u64_le(bytes, &mut pos)?;
            Some(MobileEvidence {
                receipts_root,
                aggregate_signature: mob_sig,
                participant_count: mob_count,
                packed_participants,
                created_at_ms,
            })
        } else {
            None
        };

        Ok(Self {
            view,
            block_hash,
            aggregate_signature,
            signer_count,
            packed_signers,
            mobile,
        })
    }

    /// Returns the Blake3 hash of the encoded evidence (used as the consensus root
    /// stored in the block header's `extra_data`).
    pub fn evidence_root(&self) -> [u8; 32] {
        let encoded = self.encode();
        *blake3::hash(&encoded).as_bytes()
    }
}

// ---------------------------------------------------------------------------
// MDBX table
// ---------------------------------------------------------------------------

const EVIDENCE_TABLE: &str = "n42_consensus_evidence";

/// MDBX-backed store for per-block consensus evidence.
pub struct EvidenceStore {
    env: Environment,
    db: Database,
}

impl std::fmt::Debug for EvidenceStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EvidenceStore").finish_non_exhaustive()
    }
}

impl EvidenceStore {
    /// Opens (or creates) the evidence table in the given MDBX environment.
    pub fn open(env: Environment) -> eyre::Result<Self> {
        let tx = env.begin_rw_txn()?;
        let db = tx
            .create_db(Some(EVIDENCE_TABLE), DatabaseFlags::default())
            .map_err(|e| eyre::eyre!("create {EVIDENCE_TABLE}: {e}"))?;
        tx.commit()
            .map_err(|e| eyre::eyre!("commit create {EVIDENCE_TABLE}: {e}"))?;
        Ok(Self { env, db })
    }

    /// Writes evidence for a single block (encodes and hashes internally).
    pub fn put(&self, block_number: u64, evidence: &ConsensusEvidence) -> eyre::Result<()> {
        let encoded = evidence.encode();
        let root = *blake3::hash(&encoded).as_bytes();
        self.put_raw(block_number, &encoded, &root)
    }

    /// Writes pre-encoded evidence bytes with their Blake3 root.
    ///
    /// The stored value is `encoded_evidence ++ root` (32B appended), so
    /// [`get_root`] can return the root without decoding the evidence.
    pub fn put_raw(&self, block_number: u64, encoded: &[u8], root: &[u8; 32]) -> eyre::Result<()> {
        let key = block_number.to_be_bytes();
        let mut value = Vec::with_capacity(encoded.len() + 32);
        value.extend_from_slice(encoded);
        value.extend_from_slice(root);

        let tx = self.env.begin_rw_txn()?;
        tx.put(self.db.dbi(), &key, &value, WriteFlags::default())
            .map_err(|e| eyre::eyre!("evidence put block {block_number}: {e}"))?;
        tx.commit()
            .map_err(|e| eyre::eyre!("evidence commit block {block_number}: {e}"))?;
        Ok(())
    }

    /// Returns just the 32-byte evidence root for a block (zero-allocation fast path).
    ///
    /// Used by [`N42Consensus::validate_block_post_execution`] to verify
    /// `parent_beacon_block_root` without decoding the full evidence.
    pub fn get_root(&self, block_number: u64) -> eyre::Result<Option<[u8; 32]>> {
        let key = block_number.to_be_bytes();
        let tx = self.env.begin_ro_txn()?;
        match tx.get::<Vec<u8>>(self.db.dbi(), &key) {
            Ok(Some(bytes)) if bytes.len() >= 32 => {
                let root: [u8; 32] = bytes[bytes.len() - 32..].try_into().unwrap();
                Ok(Some(root))
            }
            Ok(Some(_)) => Err(eyre::eyre!("evidence block {block_number}: value too short for root")),
            Ok(None) => Ok(None),
            Err(e) => Err(eyre::eyre!("evidence get_root block {block_number}: {e}")),
        }
    }

    /// Reads and decodes full evidence for a block, returning `None` if not stored.
    pub fn get(&self, block_number: u64) -> eyre::Result<Option<ConsensusEvidence>> {
        let key = block_number.to_be_bytes();
        let tx = self.env.begin_ro_txn()?;
        match tx.get::<Vec<u8>>(self.db.dbi(), &key) {
            Ok(Some(bytes)) if bytes.len() >= 32 => {
                // Strip the appended 32B root before decoding.
                let ev = ConsensusEvidence::decode(&bytes[..bytes.len() - 32])
                    .map_err(|e| eyre::eyre!("evidence decode block {block_number}: {e}"))?;
                Ok(Some(ev))
            }
            Ok(Some(_)) => Err(eyre::eyre!("evidence block {block_number}: value too short")),
            Ok(None) => Ok(None),
            Err(e) => Err(eyre::eyre!("evidence get block {block_number}: {e}")),
        }
    }

    /// Returns a clone of the MDBX environment (for sharing with JMT).
    pub fn env(&self) -> Environment {
        self.env.clone()
    }
}

// ---------------------------------------------------------------------------
// Codec helpers
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum EvidenceDecodeError {
    #[error("evidence bytes too short: have {have}, need at least {need}")]
    TooShort { have: usize, need: usize },
}

fn packed_byte_len(bit_count: u16) -> usize {
    ((bit_count as usize) + 7) / 8
}

fn read_u64_le(buf: &[u8], pos: &mut usize) -> Result<u64, EvidenceDecodeError> {
    check_remaining(buf, *pos, 8)?;
    let v = u64::from_le_bytes(buf[*pos..*pos + 8].try_into().unwrap());
    *pos += 8;
    Ok(v)
}

fn read_u16_le(buf: &[u8], pos: &mut usize) -> Result<u16, EvidenceDecodeError> {
    check_remaining(buf, *pos, 2)?;
    let v = u16::from_le_bytes(buf[*pos..*pos + 2].try_into().unwrap());
    *pos += 2;
    Ok(v)
}

fn read_array<const N: usize>(buf: &[u8], pos: &mut usize) -> Result<[u8; N], EvidenceDecodeError> {
    check_remaining(buf, *pos, N)?;
    let v: [u8; N] = buf[*pos..*pos + N].try_into().unwrap();
    *pos += N;
    Ok(v)
}

fn check_remaining(buf: &[u8], pos: usize, need: usize) -> Result<(), EvidenceDecodeError> {
    if buf.len() < pos + need {
        Err(EvidenceDecodeError::TooShort {
            have: buf.len(),
            need: pos + need,
        })
    } else {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_evidence(signer_count: u16, with_mobile: bool) -> ConsensusEvidence {
        let signer_bytes = packed_byte_len(signer_count);
        ConsensusEvidence {
            view: 42,
            block_hash: [0xAA; 32],
            aggregate_signature: [0xBB; 96],
            signer_count,
            packed_signers: vec![0xFF; signer_bytes],
            mobile: if with_mobile {
                Some(MobileEvidence {
                    receipts_root: [0xCC; 32],
                    aggregate_signature: [0xDD; 96],
                    participant_count: 100,
                    packed_participants: vec![0xEE; 13], // ceil(100/8) = 13
                    created_at_ms: 1700000000000,
                })
            } else {
                None
            },
        }
    }

    #[test]
    fn encode_decode_roundtrip_no_mobile() {
        let ev = sample_evidence(7, false);
        let encoded = ev.encode();
        assert_eq!(encoded.len(), 8 + 32 + 96 + 2 + 1 + 1); // 140 bytes
        let decoded = ConsensusEvidence::decode(&encoded).unwrap();
        assert_eq!(decoded.view, 42);
        assert_eq!(decoded.signer_count, 7);
        assert_eq!(decoded.packed_signers.len(), 1);
        assert!(decoded.mobile.is_none());
    }

    #[test]
    fn encode_decode_roundtrip_with_mobile() {
        let ev = sample_evidence(500, true);
        let encoded = ev.encode();
        // 8 + 32 + 96 + 2 + 63 + 1 + 32 + 96 + 2 + 13 + 8 = 353
        assert_eq!(encoded.len(), 353);
        let decoded = ConsensusEvidence::decode(&encoded).unwrap();
        assert_eq!(decoded.signer_count, 500);
        assert_eq!(decoded.packed_signers.len(), 63);
        let mob = decoded.mobile.unwrap();
        assert_eq!(mob.participant_count, 100);
        assert_eq!(mob.packed_participants.len(), 13);
        assert_eq!(mob.created_at_ms, 1700000000000);
    }

    #[test]
    fn encode_size_7_validators() {
        let ev = sample_evidence(7, false);
        assert_eq!(ev.encode().len(), 140);
    }

    #[test]
    fn encode_size_500_validators() {
        let ev = sample_evidence(500, false);
        // 8 + 32 + 96 + 2 + 63 + 1 = 202
        assert_eq!(ev.encode().len(), 202);
    }

    #[test]
    fn decode_too_short() {
        let result = ConsensusEvidence::decode(&[0u8; 10]);
        assert!(result.is_err());
    }

    #[test]
    fn evidence_root_deterministic() {
        let ev = sample_evidence(7, false);
        let r1 = ev.evidence_root();
        let r2 = ev.evidence_root();
        assert_eq!(r1, r2);
        assert_ne!(r1, [0u8; 32]);
    }

    #[test]
    fn mdbx_store_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let env = crate::disk_store::open_jmt_env(dir.path()).unwrap();
        let store = EvidenceStore::open(env).unwrap();

        let ev = sample_evidence(7, true);
        store.put(100, &ev).unwrap();

        let loaded = store.get(100).unwrap().expect("should exist");
        assert_eq!(loaded.view, 42);
        assert!(loaded.mobile.is_some());

        assert!(store.get(999).unwrap().is_none());
    }
}
