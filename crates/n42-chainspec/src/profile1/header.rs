//! The Profile-1 header layout (mixed production standard B1-B12, AM3, AM4).
//!
//! A Profile-1 header is exactly 24 RLP elements (numbered from 1 as the
//! standard does):
//!
//! | # | field | # | field |
//! |---|-------|---|-------|
//! | 1-15 | the Ethereum pre-London fields | 20 | parentBeaconBlockRoot |
//! | 16 | baseFeePerGas | 21 | requestsHash |
//! | 17 | withdrawalsRoot (rewards root, B6) | 22 | the empty string `0x80` (B12) |
//! | 18 | blobGasUsed | 23 | slotNumber (= number, B10) |
//! | 19 | excessBlobGas | 24 | mobileRegistryRoot (B11) |
//!
//! There is one layout and no optional element: a header that stops early,
//! carries element 22 as a hash or has a 25th element does not decode. The
//! legacy gov5 layouts (`Gov5NativeHeader`) are not readable here (P1).
//!
//! Alloy's `Header` cannot encode the element-22 gap or carry element 24, so
//! [`Profile1Header`] keeps the alloy view plus the registry root and encodes
//! the 24 elements itself. Its hash is the keccak of that encoding.

use alloy_consensus::Header;
use alloy_primitives::{B256, Bytes, U256, keccak256};
use alloy_rlp::{Decodable, Encodable, Header as RlpHeader};
use n42_primitives::{BlsPublicKey, BlsSecretKey, BlsSignature};

/// The four bytes every Profile-1 `extraData` starts with.
pub const PROFILE1_EXTRA_MAGIC: [u8; 4] = *b"N42H";
/// Magic plus the little-endian view.
pub const PROFILE1_EXTRA_PREFIX_LEN: usize = 12;
/// The proposer's BLS seal at the end of `extraData` (B2).
pub const PROFILE1_SEAL_LEN: usize = 96;
/// Upper bound of `extraData` (B1).
pub const PROFILE1_MAX_EXTRA_LEN: usize = 4096;
/// Number of RLP elements of a Profile-1 header.
pub const PROFILE1_HEADER_ELEMENTS: usize = 24;

/// AM24: every Profile-1 header, genesis included, carries EIP-7685's
/// requests hash of an empty request list, `sha256("")`. The empty-trie root
/// is refused (B8).
pub const PROFILE1_REQUESTS_HASH: B256 = B256::new([
    0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14, 0x9a, 0xfb, 0xf4, 0xc8, 0x99, 0x6f, 0xb9, 0x24,
    0x27, 0xae, 0x41, 0xe4, 0x64, 0x9b, 0x93, 0x4c, 0xa4, 0x95, 0x99, 0x1b, 0x78, 0x52, 0xb8, 0x55,
]);

/// Why bytes are not a Profile-1 header.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Profile1HeaderError {
    #[error("profile 1 header is not an RLP list")]
    NotAList,
    #[error("profile 1 header has {0} trailing bytes after the list")]
    Trailing(usize),
    #[error("profile 1 header element {index} ({name}) is missing")]
    Missing { index: usize, name: &'static str },
    #[error("profile 1 header element {index} ({name}) is malformed: {reason}")]
    Field {
        index: usize,
        name: &'static str,
        reason: String,
    },
    #[error("profile 1 header element 22 is not the empty gap 0x80")]
    Element22,
    #[error("profile 1 header has more than 24 elements")]
    TooManyElements,
}

/// Why an `extraData` is not the Profile-1 layout (B1).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Profile1ExtraError {
    #[error("extraData is {0} bytes, at most 4096")]
    TooLong(usize),
    #[error("extraData is {0} bytes, shorter than N42H || view || seal")]
    TooShort(usize),
    #[error("extraData does not start with N42H")]
    Magic,
}

/// Why a seal does not verify (B2).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Profile1SealError {
    #[error(transparent)]
    Extra(#[from] Profile1ExtraError),
    #[error("seal is not a BLS signature")]
    Encoding,
    #[error("seal is not the signer's signature over sealHash {0}")]
    Signature(B256),
}

/// `extraData` split per B1: `"N42H" || view u64 LE || QC || seal(96)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Profile1Extra<'a> {
    /// The view the block was proposed in.
    pub view: u64,
    /// The embedded commit-domain QC in gov5's SSZ wire form; empty when
    /// the leader held none. Decoding and verifying it is the caller's.
    pub qc: &'a [u8],
    /// The proposer's seal.
    pub seal: &'a [u8; PROFILE1_SEAL_LEN],
}

impl<'a> Profile1Extra<'a> {
    /// Splits `extra` into view, QC bytes and seal. Only the single Profile-1
    /// layout is accepted: the seal is always present.
    pub fn parse(extra: &'a [u8]) -> Result<Self, Profile1ExtraError> {
        if extra.len() > PROFILE1_MAX_EXTRA_LEN {
            return Err(Profile1ExtraError::TooLong(extra.len()));
        }
        if extra.len() < PROFILE1_EXTRA_PREFIX_LEN + PROFILE1_SEAL_LEN {
            return Err(Profile1ExtraError::TooShort(extra.len()));
        }
        if extra[..4] != PROFILE1_EXTRA_MAGIC {
            return Err(Profile1ExtraError::Magic);
        }
        let view = u64::from_le_bytes(
            extra[4..PROFILE1_EXTRA_PREFIX_LEN]
                .try_into()
                .expect("fixed-width slice"),
        );
        let seal_start = extra.len() - PROFILE1_SEAL_LEN;
        Ok(Self {
            view,
            qc: &extra[PROFILE1_EXTRA_PREFIX_LEN..seal_start],
            seal: extra[seal_start..].try_into().expect("fixed-width slice"),
        })
    }
}

/// `"N42H" || view || qc || zero seal`: what a leader writes before sealing.
pub fn profile1_extra(view: u64, qc: &[u8]) -> Bytes {
    let mut extra = Vec::with_capacity(PROFILE1_EXTRA_PREFIX_LEN + qc.len() + PROFILE1_SEAL_LEN);
    extra.extend_from_slice(&PROFILE1_EXTRA_MAGIC);
    extra.extend_from_slice(&view.to_le_bytes());
    extra.extend_from_slice(qc);
    extra.resize(extra.len() + PROFILE1_SEAL_LEN, 0);
    extra.into()
}

/// A Profile-1 header: the alloy view plus element 24.
///
/// In the alloy view `block_access_list_hash` is always `None` (element 22 is
/// the gap) and every other optional field is `Some` after [`Self::decode`].
/// [`Self::encode`] writes a `None` integer as 0 and a `None` hash as zero, so
/// a constructed header always has the 24-element shape.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile1Header {
    pub header: Header,
    /// Element 24: the mobile-registry root committed by the proposer.
    pub mobile_registry_root: B256,
}

const NAMES: [&str; PROFILE1_HEADER_ELEMENTS] = [
    "parentHash",
    "ommersHash",
    "coinbase",
    "stateRoot",
    "transactionsRoot",
    "receiptsRoot",
    "logsBloom",
    "difficulty",
    "number",
    "gasLimit",
    "gasUsed",
    "timestamp",
    "extraData",
    "mixHash",
    "nonce",
    "baseFeePerGas",
    "withdrawalsRoot",
    "blobGasUsed",
    "excessBlobGas",
    "parentBeaconBlockRoot",
    "requestsHash",
    "blockAccessListHash",
    "slotNumber",
    "mobileRegistryRoot",
];

fn decode_element<T: Decodable>(
    buf: &mut &[u8],
    index: usize,
) -> Result<T, Profile1HeaderError> {
    let name = NAMES[index - 1];
    if buf.is_empty() {
        return Err(Profile1HeaderError::Missing { index, name });
    }
    T::decode(buf).map_err(|error| Profile1HeaderError::Field {
        index,
        name,
        reason: error.to_string(),
    })
}

impl Profile1Header {
    fn payload(&self) -> Vec<u8> {
        let h = &self.header;
        let mut out = Vec::with_capacity(640 + h.extra_data.len());
        h.parent_hash.encode(&mut out);
        h.ommers_hash.encode(&mut out);
        h.beneficiary.encode(&mut out);
        h.state_root.encode(&mut out);
        h.transactions_root.encode(&mut out);
        h.receipts_root.encode(&mut out);
        h.logs_bloom.encode(&mut out);
        h.difficulty.encode(&mut out);
        h.number.encode(&mut out);
        h.gas_limit.encode(&mut out);
        h.gas_used.encode(&mut out);
        h.timestamp.encode(&mut out);
        h.extra_data.encode(&mut out);
        h.mix_hash.encode(&mut out);
        h.nonce.encode(&mut out);
        h.base_fee_per_gas.unwrap_or_default().encode(&mut out);
        h.withdrawals_root.unwrap_or_default().encode(&mut out);
        h.blob_gas_used.unwrap_or_default().encode(&mut out);
        h.excess_blob_gas.unwrap_or_default().encode(&mut out);
        h.parent_beacon_block_root
            .unwrap_or_default()
            .encode(&mut out);
        h.requests_hash.unwrap_or_default().encode(&mut out);
        out.push(alloy_rlp::EMPTY_STRING_CODE);
        h.slot_number.unwrap_or_default().encode(&mut out);
        self.mobile_registry_root.encode(&mut out);
        out
    }

    /// The header's RLP.
    pub fn encode(&self) -> Vec<u8> {
        let payload = self.payload();
        let mut out = Vec::with_capacity(payload.len() + 4);
        RlpHeader {
            list: true,
            payload_length: payload.len(),
        }
        .encode(&mut out);
        out.extend_from_slice(&payload);
        out
    }

    /// The block hash: keccak of [`Self::encode`].
    pub fn hash(&self) -> B256 {
        keccak256(self.encode())
    }

    /// Decodes exactly one Profile-1 header.
    pub fn decode(raw: &[u8]) -> Result<Self, Profile1HeaderError> {
        let mut buf = raw;
        let head = RlpHeader::decode(&mut buf).map_err(|_| Profile1HeaderError::NotAList)?;
        if !head.list {
            return Err(Profile1HeaderError::NotAList);
        }
        if buf.len() < head.payload_length {
            return Err(Profile1HeaderError::NotAList);
        }
        if buf.len() > head.payload_length {
            return Err(Profile1HeaderError::Trailing(buf.len() - head.payload_length));
        }
        let mut header = Header {
            parent_hash: decode_element(&mut buf, 1)?,
            ommers_hash: decode_element(&mut buf, 2)?,
            beneficiary: decode_element(&mut buf, 3)?,
            state_root: decode_element(&mut buf, 4)?,
            transactions_root: decode_element(&mut buf, 5)?,
            receipts_root: decode_element(&mut buf, 6)?,
            logs_bloom: decode_element(&mut buf, 7)?,
            difficulty: decode_element::<U256>(&mut buf, 8)?,
            number: decode_element(&mut buf, 9)?,
            gas_limit: decode_element(&mut buf, 10)?,
            gas_used: decode_element(&mut buf, 11)?,
            timestamp: decode_element(&mut buf, 12)?,
            extra_data: decode_element(&mut buf, 13)?,
            mix_hash: decode_element(&mut buf, 14)?,
            nonce: decode_element(&mut buf, 15)?,
            ..Default::default()
        };
        header.base_fee_per_gas = Some(decode_element(&mut buf, 16)?);
        header.withdrawals_root = Some(decode_element(&mut buf, 17)?);
        header.blob_gas_used = Some(decode_element(&mut buf, 18)?);
        header.excess_blob_gas = Some(decode_element(&mut buf, 19)?);
        header.parent_beacon_block_root = Some(decode_element(&mut buf, 20)?);
        header.requests_hash = Some(decode_element(&mut buf, 21)?);
        match buf.first() {
            None => {
                return Err(Profile1HeaderError::Missing {
                    index: 22,
                    name: NAMES[21],
                });
            }
            Some(&alloy_rlp::EMPTY_STRING_CODE) => buf = &buf[1..],
            Some(_) => return Err(Profile1HeaderError::Element22),
        }
        header.slot_number = Some(decode_element(&mut buf, 23)?);
        let mobile_registry_root = decode_element(&mut buf, 24)?;
        if !buf.is_empty() {
            return Err(Profile1HeaderError::TooManyElements);
        }
        Ok(Self {
            header,
            mobile_registry_root,
        })
    }

    /// The parsed `extraData` (B1 layout only; the QC is not decoded).
    pub fn extra(&self) -> Result<Profile1Extra<'_>, Profile1ExtraError> {
        Profile1Extra::parse(&self.header.extra_data)
    }

    /// AM4: `keccak256(rlp(H'))`, `H'` being the whole header with the last
    /// 96 bytes of `extraData` removed when `extraData` is longer than 96.
    pub fn seal_hash(&self) -> B256 {
        let extra = &self.header.extra_data;
        if extra.len() <= PROFILE1_SEAL_LEN {
            return self.hash();
        }
        let mut unsealed = self.clone();
        unsealed.header.extra_data = Bytes::copy_from_slice(&extra[..extra.len() - PROFILE1_SEAL_LEN]);
        unsealed.hash()
    }

    /// Signs the seal into the last 96 bytes of `extraData` with the H2-v4
    /// proof-of-possession DST (AM4).
    pub fn seal(&mut self, key: &BlsSecretKey) -> Result<(), Profile1ExtraError> {
        self.extra()?;
        let signature = key.sign_h2_v4(self.seal_hash().as_slice());
        let mut extra = self.header.extra_data.to_vec();
        let start = extra.len() - PROFILE1_SEAL_LEN;
        extra[start..].copy_from_slice(&signature.to_bytes());
        self.header.extra_data = extra.into();
        Ok(())
    }

    /// Checks the seal against `signer`'s key (validated at the trust
    /// boundary, e.g. the genesis validator list).
    pub fn verify_seal(&self, signer: &BlsPublicKey) -> Result<(), Profile1SealError> {
        let extra = self.extra()?;
        let signature =
            BlsSignature::from_bytes(extra.seal).map_err(|_| Profile1SealError::Encoding)?;
        let seal_hash = self.seal_hash();
        signer
            .verify_h2_v4_prevalidated(seal_hash.as_slice(), &signature)
            .map_err(|_| Profile1SealError::Signature(seal_hash))
    }
}
