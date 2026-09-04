//! gov5's native header wire form.
//!
//! gov5 encodes its header with Go's `rlp:"optional,nil"` semantics: an optional
//! field that is nil is omitted when nothing after it is set, and written as the
//! empty string (`0x80`) when a later optional field is present. The header
//! also carries two fields Ethereum does not have: the EIP-7928 block access
//! list hash (field 22, unused on chain 94) and the mobile-registry root
//! (field 23), stamped on every block since the `mobileAnchor` fork.
//!
//! Alloy's `Header` cannot represent either shape: it decodes a `0x80` hash
//! placeholder as an error and has no field for the mobile-registry root. This
//! module decodes such headers into an alloy `Header` plus the extra fields,
//! re-encodes them byte-for-byte, and remembers the raw encoding of every
//! header seen so the engine can seal a block with the hash gov5 committed to
//! rather than the hash alloy's lossy re-encoding would produce.

use alloy_consensus::Header;
use alloy_primitives::{B256, U256, keccak256};
use alloy_rlp::{Decodable, Encodable, Header as RlpHeader};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};

/// Errors from decoding a gov5 native header.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Gov5NativeHeaderError {
    #[error("gov5 native header is not an RLP list")]
    NotAList,
    #[error("gov5 native header field {0} is malformed: {1}")]
    Field(&'static str, String),
    #[error("gov5 native header has {0} trailing bytes")]
    Trailing(usize),
    #[error("gov5 native header has more than 23 fields")]
    TooManyFields,
}

/// A gov5 header decoded losslessly: the alloy view plus the two fields alloy
/// cannot carry and the knowledge of which optional slots were written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Gov5NativeHeader {
    /// The alloy view. Optional hash fields that gov5 wrote as `0x80`
    /// placeholders are `None`; integer placeholders (`base_fee_per_gas`,
    /// `blob_gas_used`, `excess_blob_gas`) are `Some(0)` because gov5 encodes
    /// a nil pointer and a zero value identically, the two re-encode to the
    /// same byte, and reth's Cancun validation needs `Some`.
    pub header: Header,
    /// Field 23: the committed mobile-registry BMT root (`mobileAnchor` fork).
    pub mobile_registry_root: Option<B256>,
}

const OPTIONAL_FIELD_NAMES: [&str; 8] = [
    "baseFeePerGas",
    "withdrawalsRoot",
    "blobGasUsed",
    "excessBlobGas",
    "parentBeaconBlockRoot",
    "requestsHash",
    "blockAccessListHash",
    "mobileRegistryRoot",
];

fn field_error(name: &'static str, error: impl std::fmt::Display) -> Gov5NativeHeaderError {
    Gov5NativeHeaderError::Field(name, error.to_string())
}

/// Reads one optional item: `None` for gov5's empty-string placeholder.
fn take_optional_item<'a>(
    buf: &mut &'a [u8],
    name: &'static str,
) -> Result<Option<&'a [u8]>, Gov5NativeHeaderError> {
    let mut probe = *buf;
    let head = RlpHeader::decode(&mut probe).map_err(|error| field_error(name, error))?;
    if head.list {
        return Err(field_error(name, "unexpected list"));
    }
    if head.payload_length == 0 {
        *buf = probe;
        return Ok(None);
    }
    let item = probe
        .get(..head.payload_length)
        .ok_or_else(|| field_error(name, "truncated"))?;
    *buf = &probe[head.payload_length..];
    Ok(Some(item))
}

fn optional_hash(
    buf: &mut &[u8],
    name: &'static str,
) -> Result<Option<B256>, Gov5NativeHeaderError> {
    match take_optional_item(buf, name)? {
        None => Ok(None),
        Some(item) if item.len() == 32 => Ok(Some(B256::from_slice(item))),
        Some(item) => Err(field_error(name, format!("hash of {} bytes", item.len()))),
    }
}

fn optional_u64(buf: &mut &[u8], name: &'static str) -> Result<Option<u64>, Gov5NativeHeaderError> {
    match take_optional_item(buf, name)? {
        None => Ok(None),
        Some(item) => {
            if item.len() > 8 || item.first() == Some(&0) {
                return Err(field_error(name, "non-canonical integer"));
            }
            let mut value = 0_u64;
            for byte in item {
                value = (value << 8) | u64::from(*byte);
            }
            Ok(Some(value))
        }
    }
}

impl Gov5NativeHeader {
    /// Decodes gov5's native header RLP. Accepts every header alloy accepts and
    /// additionally the placeholder-bearing and 23-field shapes.
    pub fn decode(raw: &[u8]) -> Result<Self, Gov5NativeHeaderError> {
        let mut buf = raw;
        let head = RlpHeader::decode(&mut buf).map_err(|error| field_error("header", error))?;
        if !head.list {
            return Err(Gov5NativeHeaderError::NotAList);
        }
        if buf.len() != head.payload_length {
            return Err(Gov5NativeHeaderError::Trailing(
                buf.len().saturating_sub(head.payload_length),
            ));
        }
        let mut header = Header {
            parent_hash: Decodable::decode(&mut buf).map_err(|e| field_error("parentHash", e))?,
            ommers_hash: Decodable::decode(&mut buf).map_err(|e| field_error("sha3Uncles", e))?,
            beneficiary: Decodable::decode(&mut buf).map_err(|e| field_error("miner", e))?,
            state_root: Decodable::decode(&mut buf).map_err(|e| field_error("stateRoot", e))?,
            transactions_root: Decodable::decode(&mut buf)
                .map_err(|e| field_error("transactionsRoot", e))?,
            receipts_root: Decodable::decode(&mut buf)
                .map_err(|e| field_error("receiptsRoot", e))?,
            logs_bloom: Decodable::decode(&mut buf).map_err(|e| field_error("logsBloom", e))?,
            difficulty: Decodable::decode(&mut buf).map_err(|e| field_error("difficulty", e))?,
            number: u64::decode(&mut buf).map_err(|e| field_error("number", e))?,
            gas_limit: u64::decode(&mut buf).map_err(|e| field_error("gasLimit", e))?,
            gas_used: u64::decode(&mut buf).map_err(|e| field_error("gasUsed", e))?,
            timestamp: u64::decode(&mut buf).map_err(|e| field_error("timestamp", e))?,
            extra_data: Decodable::decode(&mut buf).map_err(|e| field_error("extraData", e))?,
            mix_hash: Decodable::decode(&mut buf).map_err(|e| field_error("mixHash", e))?,
            nonce: Decodable::decode(&mut buf).map_err(|e| field_error("nonce", e))?,
            base_fee_per_gas: None,
            withdrawals_root: None,
            blob_gas_used: None,
            excess_blob_gas: None,
            parent_beacon_block_root: None,
            requests_hash: None,
            block_access_list_hash: None,
            slot_number: None,
        };
        let mut mobile_registry_root = None;
        let mut index = 0;
        while !buf.is_empty() {
            match index {
                // Integer fields: a nil pointer and a zero value share the
                // `0x80` encoding, so a written slot always decodes to `Some`.
                0 => {
                    header.base_fee_per_gas =
                        Some(optional_u64(&mut buf, OPTIONAL_FIELD_NAMES[0])?.unwrap_or(0))
                }
                1 => header.withdrawals_root = optional_hash(&mut buf, OPTIONAL_FIELD_NAMES[1])?,
                2 => {
                    header.blob_gas_used =
                        Some(optional_u64(&mut buf, OPTIONAL_FIELD_NAMES[2])?.unwrap_or(0))
                }
                3 => {
                    header.excess_blob_gas =
                        Some(optional_u64(&mut buf, OPTIONAL_FIELD_NAMES[3])?.unwrap_or(0))
                }
                4 => {
                    header.parent_beacon_block_root =
                        optional_hash(&mut buf, OPTIONAL_FIELD_NAMES[4])?
                }
                5 => header.requests_hash = optional_hash(&mut buf, OPTIONAL_FIELD_NAMES[5])?,
                6 => {
                    header.block_access_list_hash =
                        optional_hash(&mut buf, OPTIONAL_FIELD_NAMES[6])?
                }
                7 => mobile_registry_root = optional_hash(&mut buf, OPTIONAL_FIELD_NAMES[7])?,
                _ => return Err(Gov5NativeHeaderError::TooManyFields),
            }
            index += 1;
        }
        Ok(Self {
            header,
            mobile_registry_root,
        })
    }

    /// Wraps an alloy header that carries no gov5-only fields.
    pub fn from_header(header: Header) -> Self {
        Self {
            header,
            mobile_registry_root: None,
        }
    }

    /// The optional fields in wire order: present values and the index of the
    /// last one that must be written.
    fn optional_fields(&self) -> [OptionalField; 8] {
        let h = &self.header;
        [
            OptionalField::U64(h.base_fee_per_gas),
            OptionalField::Hash(h.withdrawals_root),
            OptionalField::U64(h.blob_gas_used),
            OptionalField::U64(h.excess_blob_gas),
            OptionalField::Hash(h.parent_beacon_block_root),
            OptionalField::Hash(h.requests_hash),
            OptionalField::Hash(h.block_access_list_hash),
            OptionalField::Hash(self.mobile_registry_root),
        ]
    }

    fn written_optional_count(&self) -> usize {
        self.optional_fields()
            .iter()
            .rposition(OptionalField::is_some)
            .map_or(0, |index| index + 1)
    }

    fn payload_length(&self) -> usize {
        let h = &self.header;
        let mut length = h.parent_hash.length()
            + h.ommers_hash.length()
            + h.beneficiary.length()
            + h.state_root.length()
            + h.transactions_root.length()
            + h.receipts_root.length()
            + h.logs_bloom.length()
            + h.difficulty.length()
            + U256::from(h.number).length()
            + U256::from(h.gas_limit).length()
            + U256::from(h.gas_used).length()
            + h.timestamp.length()
            + h.extra_data.length()
            + h.mix_hash.length()
            + h.nonce.length();
        let fields = self.optional_fields();
        for field in &fields[..self.written_optional_count()] {
            length += field.length();
        }
        length
    }

    /// gov5's exact header RLP.
    pub fn encode(&self) -> Vec<u8> {
        let h = &self.header;
        let payload_length = self.payload_length();
        let mut out = Vec::with_capacity(payload_length + 4);
        RlpHeader {
            list: true,
            payload_length,
        }
        .encode(&mut out);
        h.parent_hash.encode(&mut out);
        h.ommers_hash.encode(&mut out);
        h.beneficiary.encode(&mut out);
        h.state_root.encode(&mut out);
        h.transactions_root.encode(&mut out);
        h.receipts_root.encode(&mut out);
        h.logs_bloom.encode(&mut out);
        h.difficulty.encode(&mut out);
        U256::from(h.number).encode(&mut out);
        U256::from(h.gas_limit).encode(&mut out);
        U256::from(h.gas_used).encode(&mut out);
        h.timestamp.encode(&mut out);
        h.extra_data.encode(&mut out);
        h.mix_hash.encode(&mut out);
        h.nonce.encode(&mut out);
        let fields = self.optional_fields();
        for field in &fields[..self.written_optional_count()] {
            field.encode(&mut out);
        }
        out
    }

    /// The block hash gov5 commits to: keccak of the native encoding.
    pub fn hash(&self) -> B256 {
        keccak256(self.encode())
    }

    /// True when alloy's own encoding reproduces gov5's bytes, i.e. the header
    /// carries neither placeholders nor gov5-only fields.
    pub fn is_alloy_exact(&self) -> bool {
        self.mobile_registry_root.is_none() && {
            let mut alloy = Vec::new();
            self.header.encode(&mut alloy);
            alloy == self.encode()
        }
    }
}

#[derive(Clone, Copy)]
enum OptionalField {
    U64(Option<u64>),
    Hash(Option<B256>),
}

impl OptionalField {
    fn is_some(&self) -> bool {
        match self {
            Self::U64(value) => value.is_some(),
            Self::Hash(value) => value.is_some(),
        }
    }

    fn length(&self) -> usize {
        match self {
            Self::U64(Some(value)) => U256::from(*value).length(),
            Self::Hash(Some(value)) => value.length(),
            Self::U64(None) | Self::Hash(None) => 1,
        }
    }

    fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Self::U64(Some(value)) => U256::from(*value).encode(out),
            Self::Hash(Some(value)) => value.encode(out),
            Self::U64(None) | Self::Hash(None) => out.push(alloy_rlp::EMPTY_STRING_CODE),
        }
    }
}

/// Bound on remembered raw headers. Live tracking needs the current proposal
/// and its recent ancestry; catch-up batches are at most 1024 blocks.
const NATIVE_HEADER_REGISTRY_CAPACITY: usize = 8192;

struct NativeHeaderRegistry {
    by_hash: HashMap<B256, Arc<[u8]>>,
    order: VecDeque<B256>,
}

fn registry() -> &'static Mutex<NativeHeaderRegistry> {
    static REGISTRY: OnceLock<Mutex<NativeHeaderRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        Mutex::new(NativeHeaderRegistry {
            by_hash: HashMap::new(),
            order: VecDeque::new(),
        })
    })
}

/// Remembers a header's exact gov5 encoding under its keccak hash and returns
/// that hash. Idempotent; the newest entries win when the bound is reached.
pub fn remember_gov5_native_header(raw: &[u8]) -> B256 {
    let hash = keccak256(raw);
    let mut registry = registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if registry.by_hash.contains_key(&hash) {
        return hash;
    }
    while registry.order.len() >= NATIVE_HEADER_REGISTRY_CAPACITY {
        if let Some(evicted) = registry.order.pop_front() {
            registry.by_hash.remove(&evicted);
        }
    }
    registry.by_hash.insert(hash, Arc::from(raw));
    registry.order.push_back(hash);
    hash
}

/// The remembered gov5 encoding of a header, if it was seen recently.
pub fn gov5_native_header_rlp(hash: &B256) -> Option<Arc<[u8]>> {
    registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .by_hash
        .get(hash)
        .cloned()
}

/// Decodes a remembered gov5 header and re-checks that its encoding hashes to
/// `hash`, so a registry entry can never vouch for a different header.
pub fn remembered_gov5_native_header(hash: &B256) -> Option<Gov5NativeHeader> {
    let raw = gov5_native_header_rlp(hash)?;
    if keccak256(&raw) != *hash {
        return None;
    }
    let decoded = Gov5NativeHeader::decode(&raw).ok()?;
    (decoded.encode().as_slice() == raw.as_ref()).then_some(decoded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::hex;

    /// Chain 94 block 13,560,375 (`0x0e37dae9…`), read from a gov5 datadir:
    /// withdrawals root, `0x80` blob placeholders, parent beacon root, `0x80`
    /// requests hash, `0x80` block access list hash, zero mobile-registry root.
    const CHAIN94_HEADER_RLP: &str = "f90368a059f7f5e67d917fb749f1b7f6255d476cca61e2c4fc7de6a21d7760928eea7e61a0000000000000000000000000000000000000000000000000000000000000000094580339c31c60b974ac9f70e2f8307b2b4490f70aa0a697c095ee299396deee1f7c63f0f6e50bd7143023e2461e84b09edab2e8495da0b7fbcd5eeea17f9e6eb08bc37bbbc09db6fe6350e0fe9497febc25c8d35e68d7a0e8c427cdf8308a6d37d006bc2b5d9df8618f052b19d2b10d813d4c5ecc277dacb90100000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000008083ceea378401c9c38083019a28846a8bb578b901034e343248152b000000000000142b00000000000020000000910d74febb5dac309b7389bb53ebea19bf91a664a4cfc4d74c127c15428be4ee60000000977324f952eb3d8f92cd1a9f790dc0c4fbf518c57fc70ced923e4c36d7829610d8532f5912de3e6066fc3b22eca090cc022980b768946ebea5a8b583a2c09d98def9724bdc047462dac65210ba4876921cf217b1e340c0adeb3fdf59862b71020300000007003b9517bc3a0e87fcdda01e00ac99ef1458cff9f7de7e9701c6ab5618cc9dded960654c9ae9a754fb779f6ef188a35399ee1395be42a6b86c6d4e56807ec9938ef0c6a753510cf3c7cde0218487b5b32acda479cacb530a2af80efaffbc302cfc90a0000000000000000000000000000000000000000000000000000000000000000088000000000000000007a0cff8fe1aae31883367719c195d4a0b551a24ea62c33f8447ff57cf587ea86b088080a0b6f70d3e799599e153f794f255a6dcf69d86e56cde3a83dc9ba17b92b618f2fb8080a00000000000000000000000000000000000000000000000000000000000000000";
    const CHAIN94_HEADER_HASH: &str =
        "0e37dae9d0cbf1c8e09c335654dc4cae3e18760dade40039e0e693368cc796d7";

    #[test]
    fn chain94_live_header_round_trips_to_its_hash() {
        let raw = hex::decode(CHAIN94_HEADER_RLP).unwrap();
        assert!(
            Header::decode(&mut raw.as_slice()).is_err(),
            "alloy must not accept this shape"
        );
        let native = Gov5NativeHeader::decode(&raw).unwrap();
        assert_eq!(native.encode(), raw);
        assert_eq!(
            native.hash(),
            B256::from_slice(&hex::decode(CHAIN94_HEADER_HASH).unwrap())
        );
        assert_eq!(native.header.number, 13_560_375);
        assert_eq!(native.header.timestamp, 1_787_540_856);
        assert!(native.header.withdrawals_root.is_some());
        assert_eq!(native.header.blob_gas_used, Some(0));
        assert_eq!(native.header.excess_blob_gas, Some(0));
        assert!(native.header.parent_beacon_block_root.is_some());
        assert!(native.header.requests_hash.is_none());
        assert!(native.header.block_access_list_hash.is_none());
        assert_eq!(native.mobile_registry_root, Some(B256::ZERO));
        assert!(!native.is_alloy_exact());
        let hash = remember_gov5_native_header(&raw);
        assert_eq!(remembered_gov5_native_header(&hash), Some(native));
    }

    #[test]
    fn placeholder_and_mobile_root_headers_round_trip() {
        // Build the shape synthetically so the test does not depend on the
        // exact extra bytes of a live header: base fee, withdrawals root,
        // two blob placeholders, parent beacon root, two placeholders and
        // the mobile-registry root.
        let mut header = Header {
            number: 13_560_375,
            gas_limit: 30_000_000,
            gas_used: 798_280,
            timestamp: 1_787_540_856,
            base_fee_per_gas: Some(7),
            withdrawals_root: Some(B256::repeat_byte(0x11)),
            blob_gas_used: Some(0),
            excess_blob_gas: Some(0),
            parent_beacon_block_root: Some(B256::repeat_byte(0x22)),
            ..Default::default()
        };
        header.extra_data =
            alloy_primitives::Bytes::from(vec![0x4e, 0x34, 0x32, 0x48, 1, 0, 0, 0, 0, 0, 0, 0]);
        let native = Gov5NativeHeader {
            header,
            mobile_registry_root: Some(B256::ZERO),
        };
        let encoded = native.encode();
        // Optional tail: base fee (1), withdrawals (33), 0x80, 0x80, pbr (33), 0x80, 0x80, mobile (33).
        let tail = &encoded[encoded.len() - (1 + 33 + 1 + 1 + 33 + 1 + 1 + 33)..];
        assert_eq!(tail[0], 0x07);
        assert_eq!(tail[1], 0xa0);
        assert_eq!(tail[34], 0x80);
        assert_eq!(tail[35], 0x80);
        assert_eq!(tail[36], 0xa0);
        assert_eq!(tail[69], 0x80);
        assert_eq!(tail[70], 0x80);
        assert_eq!(tail[71], 0xa0);
        let decoded = Gov5NativeHeader::decode(&encoded).unwrap();
        assert_eq!(decoded, native);
        assert_eq!(decoded.encode(), encoded);
        assert!(!native.is_alloy_exact());
        assert!(Header::decode(&mut encoded.as_slice()).is_err());
    }

    #[test]
    fn alloy_exact_headers_decode_identically() {
        let header = Header {
            number: 7,
            base_fee_per_gas: Some(1_000_000_000),
            withdrawals_root: Some(B256::repeat_byte(0x33)),
            blob_gas_used: Some(0),
            excess_blob_gas: Some(0),
            parent_beacon_block_root: Some(B256::repeat_byte(0x44)),
            requests_hash: Some(B256::repeat_byte(0x55)),
            ..Default::default()
        };
        let mut alloy = Vec::new();
        header.encode(&mut alloy);
        let native = Gov5NativeHeader::decode(&alloy).unwrap();
        assert_eq!(native.header, header);
        assert!(native.mobile_registry_root.is_none());
        assert!(native.is_alloy_exact());
        assert_eq!(native.hash(), header.hash_slow());
    }

    #[test]
    fn zero_base_fee_written_by_alloy_round_trips() {
        let header = Header {
            number: 5,
            ommers_hash: B256::ZERO,
            base_fee_per_gas: Some(0),
            ..Default::default()
        };
        let mut alloy = Vec::new();
        header.encode(&mut alloy);
        let native = Gov5NativeHeader::decode(&alloy).unwrap();
        assert_eq!(native.header.base_fee_per_gas, Some(0));
        assert_eq!(native.encode(), alloy);
        assert!(native.is_alloy_exact());
    }

    #[test]
    fn trailing_placeholders_are_never_written() {
        let native = Gov5NativeHeader::from_header(Header {
            base_fee_per_gas: Some(3),
            ..Default::default()
        });
        let encoded = native.encode();
        assert_eq!(*encoded.last().unwrap(), 0x03);
        let mut alloy = Vec::new();
        native.header.encode(&mut alloy);
        assert_eq!(encoded, alloy);
    }

    #[test]
    fn registry_returns_only_matching_headers() {
        let native = Gov5NativeHeader {
            header: Header {
                number: 99,
                base_fee_per_gas: Some(7),
                withdrawals_root: Some(B256::repeat_byte(0x66)),
                blob_gas_used: Some(0),
                excess_blob_gas: Some(0),
                parent_beacon_block_root: Some(B256::repeat_byte(0x77)),
                ..Default::default()
            },
            mobile_registry_root: Some(B256::repeat_byte(0x88)),
        };
        let raw = native.encode();
        let hash = remember_gov5_native_header(&raw);
        assert_eq!(hash, native.hash());
        assert_eq!(remembered_gov5_native_header(&hash), Some(native));
        assert!(remembered_gov5_native_header(&B256::repeat_byte(0x01)).is_none());
    }

    #[test]
    fn rejects_extra_fields_and_trailing_bytes() {
        let native = Gov5NativeHeader::from_header(Header::default());
        let mut encoded = native.encode();
        let payload = encoded.split_off(3);
        let mut too_many = Vec::new();
        let mut extended = payload.clone();
        extended.extend(std::iter::repeat_n(0x80, 9));
        RlpHeader {
            list: true,
            payload_length: extended.len(),
        }
        .encode(&mut too_many);
        too_many.extend_from_slice(&extended);
        assert_eq!(
            Gov5NativeHeader::decode(&too_many),
            Err(Gov5NativeHeaderError::TooManyFields)
        );
        let mut trailing = native.encode();
        trailing.push(0x00);
        assert!(matches!(
            Gov5NativeHeader::decode(&trailing),
            Err(Gov5NativeHeaderError::Trailing(1))
        ));
    }
}
