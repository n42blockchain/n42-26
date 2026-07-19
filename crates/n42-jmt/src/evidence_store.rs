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
//! [1B mobile_encoding]
//! [if mobile_encoding == 0x01 (legacy v1):
//!   [32B receipts_root][96B mob_sig][2B mob_count LE][⌈mob_count/8⌉B mob_bits]
//!   [8B created_at_ms LE]
//! ]
//! [if mobile_encoding == 0x02 (v2):
//!   [32B receipts_root][96B mob_sig][2B mob_count LE][4B mob_bits_len LE]
//!   [mob_bits_len B mob_bits][8B created_at_ms LE]
//! ]
//!
//! Mobile v2 stores the bitfield length explicitly because `mob_count` is the
//! number of set bits, while bit positions are verifier-registry indices. A
//! sparse cohort containing only registry index 9,999 therefore needs 1,250
//! bytes, not `ceil(1/8)`. The decoder retains v1 support for historical rows.
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
const MOBILE_NONE: u8 = 0x00;
const MOBILE_V1: u8 = 0x01;
const MOBILE_V2: u8 = 0x02;
/// Defensive bound for corrupted evidence rows. This still supports bitfields
/// over 134 million verifier indices, far above the current 10K-per-IDC limit.
const MAX_MOBILE_BITFIELD_BYTES: usize = 16 * 1024 * 1024;

impl ConsensusEvidence {
    /// Encodes into the compact binary format described in the module docs.
    pub fn encode(&self) -> Vec<u8> {
        let signer_bytes = packed_byte_len(self.signer_count);
        debug_assert!(
            self.packed_signers.len() >= signer_bytes,
            "packed_signers too short: {} < {} for signer_count {}",
            self.packed_signers.len(),
            signer_bytes,
            self.signer_count,
        );
        let mobile_size = self
            .mobile
            .as_ref()
            .map_or(0, |m| 32 + 96 + 2 + 4 + m.packed_participants.len() + 8);
        let mut buf = Vec::with_capacity(MIN_EVIDENCE_SIZE + signer_bytes + mobile_size);

        buf.extend_from_slice(&self.view.to_le_bytes());
        buf.extend_from_slice(&self.block_hash);
        buf.extend_from_slice(&self.aggregate_signature);
        buf.extend_from_slice(&self.signer_count.to_le_bytes());
        buf.extend_from_slice(&self.packed_signers[..signer_bytes]);

        match &self.mobile {
            None => buf.push(MOBILE_NONE),
            Some(m) => {
                let mob_bytes = u32::try_from(m.packed_participants.len())
                    .expect("mobile participant bitfield exceeds the v2 u32 wire limit");
                buf.push(MOBILE_V2);
                buf.extend_from_slice(&m.receipts_root);
                buf.extend_from_slice(&m.aggregate_signature);
                buf.extend_from_slice(&m.participant_count.to_le_bytes());
                buf.extend_from_slice(&mob_bytes.to_le_bytes());
                buf.extend_from_slice(&m.packed_participants);
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

        let mobile_encoding = bytes[pos];
        pos += 1;

        let mobile = match mobile_encoding {
            MOBILE_NONE => None,
            MOBILE_V1 | MOBILE_V2 => {
                let receipts_root = read_array::<32>(bytes, &mut pos)?;
                let mob_sig = read_array::<96>(bytes, &mut pos)?;
                let mob_count = read_u16_le(bytes, &mut pos)?;
                let mob_bytes = if mobile_encoding == MOBILE_V1 {
                    packed_byte_len(mob_count)
                } else {
                    read_u32_le(bytes, &mut pos)? as usize
                };
                if mob_bytes > MAX_MOBILE_BITFIELD_BYTES {
                    return Err(EvidenceDecodeError::MobileBitfieldTooLarge {
                        have: mob_bytes,
                        max: MAX_MOBILE_BITFIELD_BYTES,
                    });
                }
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
            }
            tag => return Err(EvidenceDecodeError::UnknownMobileEncoding(tag)),
        };

        if pos != bytes.len() {
            return Err(EvidenceDecodeError::TrailingBytes(bytes.len() - pos));
        }

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
        tx.put(self.db.dbi(), key, value, WriteFlags::default())
            .map_err(|e| eyre::eyre!("evidence put block {block_number}: {e}"))?;
        tx.commit()
            .map_err(|e| eyre::eyre!("evidence commit block {block_number}: {e}"))?;
        Ok(())
    }

    /// Returns just the 32-byte evidence root for a block (zero-allocation fast path).
    pub fn get_root(&self, block_number: u64) -> eyre::Result<Option<[u8; 32]>> {
        let key = block_number.to_be_bytes();
        let tx = self.env.begin_ro_txn()?;
        match tx.get::<Vec<u8>>(self.db.dbi(), &key) {
            Ok(Some(bytes)) if bytes.len() >= 32 => {
                let root: [u8; 32] = bytes[bytes.len() - 32..].try_into().unwrap();
                Ok(Some(root))
            }
            Ok(Some(_)) => Err(eyre::eyre!(
                "evidence block {block_number}: value too short for root"
            )),
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
            Ok(Some(_)) => Err(eyre::eyre!(
                "evidence block {block_number}: value too short"
            )),
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
    #[error("unknown mobile evidence encoding 0x{0:02x}")]
    UnknownMobileEncoding(u8),
    #[error("mobile participant bitfield is too large: {have} bytes, maximum {max}")]
    MobileBitfieldTooLarge { have: usize, max: usize },
    #[error("evidence has {0} trailing bytes")]
    TrailingBytes(usize),
}

fn packed_byte_len(bit_count: u16) -> usize {
    (bit_count as usize).div_ceil(8)
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

fn read_u32_le(buf: &[u8], pos: &mut usize) -> Result<u32, EvidenceDecodeError> {
    check_remaining(buf, *pos, 4)?;
    let v = u32::from_le_bytes(buf[*pos..*pos + 4].try_into().unwrap());
    *pos += 4;
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
                    packed_participants: {
                        let mut bits = vec![0xFF; 13];
                        bits[12] = 0x0F; // 12 * 8 + 4 = 100 set bits
                        bits
                    },
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
        // 8 + 32 + 96 + 2 + 63 + 1 + 32 + 96 + 2 + 4 + 13 + 8 = 357
        assert_eq!(encoded.len(), 357);
        let decoded = ConsensusEvidence::decode(&encoded).unwrap();
        assert_eq!(decoded.signer_count, 500);
        assert_eq!(decoded.packed_signers.len(), 63);
        let mob = decoded.mobile.unwrap();
        assert_eq!(mob.participant_count, 100);
        assert_eq!(mob.packed_participants.len(), 13);
        assert_eq!(mob.created_at_ms, 1700000000000);
    }

    #[test]
    fn mobile_update_roundtrips_and_changes_root() {
        let mut ev = sample_evidence(7, false);
        let no_mobile_root = ev.evidence_root();

        ev.mobile = Some(MobileEvidence {
            receipts_root: [0x12; 32],
            aggregate_signature: [0x34; 96],
            participant_count: 21,
            packed_participants: vec![0xFF, 0xFF, 0x1F],
            created_at_ms: 1_700_000_000_000,
        });

        let encoded = ev.encode();
        let decoded = ConsensusEvidence::decode(&encoded).unwrap();
        assert_eq!(decoded.encode(), encoded);
        assert_ne!(decoded.evidence_root(), no_mobile_root);

        let mobile = decoded.mobile.unwrap();
        assert_eq!(mobile.receipts_root, [0x12; 32]);
        assert_eq!(mobile.aggregate_signature, [0x34; 96]);
        assert_eq!(mobile.participant_count, 21);
        assert_eq!(mobile.packed_participants, vec![0xFF, 0xFF, 0x1F]);
        assert_eq!(mobile.created_at_ms, 1_700_000_000_000);
    }

    #[test]
    fn sparse_mobile_index_roundtrips_without_truncation() {
        let mut ev = sample_evidence(7, false);
        let mut sparse_bits = vec![0u8; 1_250];
        sparse_bits[9_999 / 8] |= 1 << (9_999 % 8);
        ev.mobile = Some(MobileEvidence {
            receipts_root: [0x12; 32],
            aggregate_signature: [0x34; 96],
            participant_count: 1,
            packed_participants: sparse_bits.clone(),
            created_at_ms: 1_700_000_000_000,
        });

        let encoded = ev.encode();
        assert_eq!(encoded[8 + 32 + 96 + 2 + 1], MOBILE_V2);
        let decoded = ConsensusEvidence::decode(&encoded).unwrap();
        let mobile = decoded.mobile.unwrap();
        assert_eq!(mobile.participant_count, 1);
        assert_eq!(mobile.packed_participants, sparse_bits);
        assert_ne!(mobile.packed_participants[9_999 / 8], 0);
    }

    #[test]
    fn legacy_mobile_v1_remains_readable() {
        let mut encoded = sample_evidence(7, false).encode();
        assert_eq!(encoded.pop(), Some(MOBILE_NONE));
        encoded.push(MOBILE_V1);
        encoded.extend_from_slice(&[0x12; 32]);
        encoded.extend_from_slice(&[0x34; 96]);
        encoded.extend_from_slice(&21u16.to_le_bytes());
        encoded.extend_from_slice(&[0xFF, 0xFF, 0x1F]);
        encoded.extend_from_slice(&1_700_000_000_000u64.to_le_bytes());

        let decoded = ConsensusEvidence::decode(&encoded).unwrap();
        let mobile = decoded.mobile.unwrap();
        assert_eq!(mobile.participant_count, 21);
        assert_eq!(mobile.packed_participants, vec![0xFF, 0xFF, 0x1F]);
        assert_eq!(mobile.created_at_ms, 1_700_000_000_000);
    }

    #[test]
    fn unknown_mobile_encoding_is_rejected() {
        let mut encoded = sample_evidence(7, false).encode();
        *encoded.last_mut().unwrap() = 0x7F;
        assert!(matches!(
            ConsensusEvidence::decode(&encoded),
            Err(EvidenceDecodeError::UnknownMobileEncoding(0x7F))
        ));
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
