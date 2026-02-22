use alloy_primitives::{Address, Bytes, B256, U256};
use serde::{Deserialize, Serialize};

/// A single account's state in the V1 verification witness.
///
/// Storage contains only the slots accessed during block execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WitnessAccount {
    pub address: Address,
    pub nonce: u64,
    pub balance: U256,
    /// keccak256(bytecode), or keccak256(&[]) for EOAs.
    pub code_hash: B256,
    /// Storage slots accessed during execution: (slot_key, value).
    pub storage: Vec<(U256, U256)>,
}

/// V1 verification packet sent from an IDC node to mobile devices.
///
/// Contains everything a phone needs to re-execute a block and verify roots.
/// Contract bytecodes the phone already has cached are excluded from
/// `uncached_bytecodes` to reduce packet size.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationPacket {
    pub block_hash: B256,
    pub block_number: u64,
    pub parent_hash: B256,
    pub state_root: B256,
    pub transactions_root: B256,
    pub receipts_root: B256,
    pub timestamp: u64,
    pub gas_limit: u64,
    /// Block coinbase address.
    pub beneficiary: Address,
    /// Full RLP-encoded block header (contains base_fee, prevrandao, etc.).
    pub header_rlp: Bytes,
    pub transactions: Vec<Bytes>,
    pub witness_accounts: Vec<WitnessAccount>,
    /// Contract bytecodes not in the phone's cache: (code_hash, bytecode).
    pub uncached_bytecodes: Vec<(B256, Bytes)>,
    /// Lowest block number referenced by BLOCKHASH opcode.
    pub lowest_block_number: Option<u64>,
    /// Ancestor block hashes for BLOCKHASH opcode: (number, hash).
    pub block_hashes: Vec<(u64, B256)>,
}

impl VerificationPacket {
    /// Returns the approximate serialized size in bytes.
    pub fn estimated_size(&self) -> usize {
        let header_size = 200 + self.header_rlp.len();
        let tx_size: usize = self.transactions.iter().map(|t| t.len()).sum();
        let witness_size: usize =
            self.witness_accounts.iter().map(|a| 100 + a.storage.len() * 64).sum();
        let code_size: usize = self.uncached_bytecodes.iter().map(|(_, c)| 32 + c.len()).sum();
        header_size + tx_size + witness_size + code_size
    }
}

/// Errors from packet encoding/decoding.
#[derive(Debug, thiserror::Error)]
pub enum PacketError {
    #[error("packet serialization failed: {0}")]
    Encode(#[from] bincode::Error),
    #[error("packet deserialization failed: {0}")]
    Decode(bincode::Error),
}

/// Encodes a V1 verification packet to bytes for transmission.
pub fn encode_packet(packet: &VerificationPacket) -> Result<Vec<u8>, PacketError> {
    bincode::serialize(packet).map_err(PacketError::Encode)
}

/// Decodes a V1 verification packet from bytes.
pub fn decode_packet(data: &[u8]) -> Result<VerificationPacket, PacketError> {
    bincode::deserialize(data).map_err(PacketError::Decode)
}

// ─── StreamPacket (V2) ──────────────────────────────────────────────────────

/// V2 verification packet using an ordered state read log instead of a keyed witness.
///
/// Since same block + same txs → revm Database calls happen in a deterministic order,
/// address/slot keys are dropped and only values are sent in call order.
/// The phone replays entries by cursor on raw bytes via `StreamReplayDB`.
///
/// Wire format: `wire_header(4B) + block_hash(32B) + header_rlp + txs + read_log + bytecodes`
#[derive(Debug, Clone)]
pub struct StreamPacket {
    pub block_hash: B256,
    pub header_rlp: Bytes,
    pub transactions: Vec<Bytes>,
    /// Pre-encoded read log bytes (output of `encode_read_log`).
    pub read_log_data: Vec<u8>,
    /// Contract bytecodes not in the phone's cache: (code_hash, bytecode).
    pub bytecodes: Vec<(B256, Bytes)>,
}

impl StreamPacket {
    /// Extracts block number and receipts root from the embedded header RLP.
    ///
    /// Returns `None` if the header cannot be decoded.
    pub fn header_info(&self) -> Option<(u64, B256)> {
        use alloy_consensus::BlockHeader;
        use alloy_rlp::Decodable;
        let header = alloy_consensus::Header::decode(&mut &self.header_rlp[..]).ok()?;
        Some((header.number, header.receipts_root()))
    }

    /// Returns the approximate serialized size in bytes.
    pub fn estimated_size(&self) -> usize {
        let header_size = 4 + 32 + 4 + self.header_rlp.len();
        let tx_size: usize = self.transactions.iter().map(|t| 4 + t.len()).sum();
        let log_size = 4 + self.read_log_data.len();
        let code_size: usize = self.bytecodes.iter().map(|(_, c)| 32 + 4 + c.len()).sum();
        header_size + tx_size + log_size + code_size
    }
}

/// Encodes a `StreamPacket` into compact binary format with wire header.
pub fn encode_stream_packet(packet: &StreamPacket) -> Vec<u8> {
    use crate::wire::{self, FLAG_HAS_BYTECODES};

    let flags = if packet.bytecodes.is_empty() { 0x00 } else { FLAG_HAS_BYTECODES };
    let mut buf = Vec::with_capacity(packet.estimated_size());

    wire::encode_header(&mut buf, wire::VERSION_1, flags);
    buf.extend_from_slice(packet.block_hash.as_slice());

    buf.extend_from_slice(&(packet.header_rlp.len() as u32).to_le_bytes());
    buf.extend_from_slice(&packet.header_rlp);

    buf.extend_from_slice(&(packet.transactions.len() as u32).to_le_bytes());
    for tx in &packet.transactions {
        buf.extend_from_slice(&(tx.len() as u32).to_le_bytes());
        buf.extend_from_slice(tx);
    }

    buf.extend_from_slice(&(packet.read_log_data.len() as u32).to_le_bytes());
    buf.extend_from_slice(&packet.read_log_data);

    buf.extend_from_slice(&(packet.bytecodes.len() as u32).to_le_bytes());
    for (hash, code) in &packet.bytecodes {
        buf.extend_from_slice(hash.as_slice());
        buf.extend_from_slice(&(code.len() as u32).to_le_bytes());
        buf.extend_from_slice(code);
    }

    buf
}

/// Maximum transaction count to prevent OOM from corrupted data.
const MAX_TX_COUNT: usize = 100_000;
/// Maximum bytecode count to prevent OOM from corrupted data.
const MAX_BYTECODE_COUNT: usize = 10_000;

/// Decodes a `StreamPacket` from compact binary format.
pub fn decode_stream_packet(data: &[u8]) -> Result<StreamPacket, crate::wire::WireError> {
    use crate::wire::{self, WireError};

    let (_header, payload) = wire::decode_header(data)?;
    let mut pos: usize = 0;

    let ensure = |pos: usize, need: usize| -> Result<(), WireError> {
        if pos + need > payload.len() {
            Err(WireError::LengthOverflow {
                offset: pos,
                need,
                remaining: payload.len().saturating_sub(pos),
            })
        } else {
            Ok(())
        }
    };

    ensure(pos, 32)?;
    let block_hash = B256::from_slice(&payload[pos..pos + 32]);
    pos += 32;

    ensure(pos, 4)?;
    let header_rlp_len = u32::from_le_bytes(payload[pos..pos + 4].try_into().unwrap()) as usize;
    pos += 4;
    ensure(pos, header_rlp_len)?;
    let header_rlp = Bytes::copy_from_slice(&payload[pos..pos + header_rlp_len]);
    pos += header_rlp_len;

    ensure(pos, 4)?;
    let tx_count = u32::from_le_bytes(payload[pos..pos + 4].try_into().unwrap()) as usize;
    pos += 4;
    if tx_count > MAX_TX_COUNT {
        return Err(WireError::LengthOverflow { offset: pos - 4, need: tx_count, remaining: MAX_TX_COUNT });
    }
    let mut transactions = Vec::with_capacity(tx_count);
    for _ in 0..tx_count {
        ensure(pos, 4)?;
        let tx_len = u32::from_le_bytes(payload[pos..pos + 4].try_into().unwrap()) as usize;
        pos += 4;
        ensure(pos, tx_len)?;
        transactions.push(Bytes::copy_from_slice(&payload[pos..pos + tx_len]));
        pos += tx_len;
    }

    ensure(pos, 4)?;
    let read_log_len = u32::from_le_bytes(payload[pos..pos + 4].try_into().unwrap()) as usize;
    pos += 4;
    ensure(pos, read_log_len)?;
    let read_log_data = payload[pos..pos + read_log_len].to_vec();
    pos += read_log_len;

    ensure(pos, 4)?;
    let code_count = u32::from_le_bytes(payload[pos..pos + 4].try_into().unwrap()) as usize;
    pos += 4;
    if code_count > MAX_BYTECODE_COUNT {
        return Err(WireError::LengthOverflow { offset: pos - 4, need: code_count, remaining: MAX_BYTECODE_COUNT });
    }
    let mut bytecodes = Vec::with_capacity(code_count);
    for _ in 0..code_count {
        ensure(pos, 36)?;
        let hash = B256::from_slice(&payload[pos..pos + 32]);
        pos += 32;
        let code_len = u32::from_le_bytes(payload[pos..pos + 4].try_into().unwrap()) as usize;
        pos += 4;
        ensure(pos, code_len)?;
        let code = Bytes::copy_from_slice(&payload[pos..pos + code_len]);
        pos += code_len;
        bytecodes.push((hash, code));
    }

    Ok(StreamPacket { block_hash, header_rlp, transactions, read_log_data, bytecodes })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, Bytes, B256, U256, KECCAK256_EMPTY};

    fn make_packet() -> VerificationPacket {
        let account = WitnessAccount {
            address: Address::ZERO,
            nonce: 5,
            balance: U256::from(1_000_000u64),
            code_hash: B256::from([0xCC; 32]),
            storage: vec![(U256::from(0), U256::from(42)), (U256::from(1), U256::from(99))],
        };
        VerificationPacket {
            block_hash: B256::from([1u8; 32]),
            block_number: 12345,
            parent_hash: B256::from([2u8; 32]),
            state_root: B256::from([3u8; 32]),
            transactions_root: B256::from([4u8; 32]),
            receipts_root: B256::from([5u8; 32]),
            timestamp: 1_700_000_000,
            gas_limit: 30_000_000,
            beneficiary: Address::from([0xFF; 20]),
            header_rlp: Bytes::from(vec![0xF8; 508]),
            transactions: vec![Bytes::from(vec![0xAA; 100]), Bytes::from(vec![0xBB; 200])],
            witness_accounts: vec![account],
            uncached_bytecodes: vec![(B256::from([0xDD; 32]), Bytes::from(vec![0xEE; 500]))],
            lowest_block_number: Some(12300),
            block_hashes: vec![(12300, B256::from([6u8; 32]))],
        }
    }

    #[test]
    fn test_packet_encode_decode_roundtrip() {
        let packet = make_packet();
        let encoded = encode_packet(&packet).expect("encoding should succeed");
        assert!(!encoded.is_empty());
        let decoded = decode_packet(&encoded).expect("decoding should succeed");

        assert_eq!(decoded.block_hash, packet.block_hash);
        assert_eq!(decoded.block_number, packet.block_number);
        assert_eq!(decoded.parent_hash, packet.parent_hash);
        assert_eq!(decoded.state_root, packet.state_root);
        assert_eq!(decoded.transactions_root, packet.transactions_root);
        assert_eq!(decoded.receipts_root, packet.receipts_root);
        assert_eq!(decoded.timestamp, packet.timestamp);
        assert_eq!(decoded.gas_limit, packet.gas_limit);
        assert_eq!(decoded.beneficiary, packet.beneficiary);
        assert_eq!(decoded.transactions.len(), packet.transactions.len());
        assert_eq!(decoded.transactions[0], packet.transactions[0]);
        assert_eq!(decoded.transactions[1], packet.transactions[1]);
        assert_eq!(decoded.witness_accounts.len(), 1);
        assert_eq!(decoded.witness_accounts[0].address, packet.witness_accounts[0].address);
        assert_eq!(decoded.witness_accounts[0].nonce, packet.witness_accounts[0].nonce);
        assert_eq!(decoded.witness_accounts[0].balance, packet.witness_accounts[0].balance);
        assert_eq!(decoded.witness_accounts[0].code_hash, packet.witness_accounts[0].code_hash);
        assert_eq!(decoded.witness_accounts[0].storage, packet.witness_accounts[0].storage);
        assert_eq!(decoded.uncached_bytecodes.len(), 1);
        assert_eq!(decoded.uncached_bytecodes[0].0, packet.uncached_bytecodes[0].0);
        assert_eq!(decoded.uncached_bytecodes[0].1, packet.uncached_bytecodes[0].1);
        assert_eq!(decoded.lowest_block_number, packet.lowest_block_number);
        assert_eq!(decoded.block_hashes, packet.block_hashes);
    }

    #[test]
    fn test_estimated_size() {
        let packet = make_packet();
        let size = packet.estimated_size();
        // header: 200 + 508 = 708, txs: 300, witness: 100 + 2*64 = 228, codes: 32 + 500 = 532
        assert_eq!(size, 708 + 300 + 228 + 532);
        assert!(size > 0);

        let empty = VerificationPacket {
            block_hash: B256::ZERO,
            block_number: 0,
            parent_hash: B256::ZERO,
            state_root: B256::ZERO,
            transactions_root: B256::ZERO,
            receipts_root: B256::ZERO,
            timestamp: 0,
            gas_limit: 0,
            beneficiary: Address::ZERO,
            header_rlp: Bytes::new(),
            transactions: vec![],
            witness_accounts: vec![],
            uncached_bytecodes: vec![],
            lowest_block_number: None,
            block_hashes: vec![],
        };
        assert_eq!(empty.estimated_size(), 200);
    }

    #[test]
    fn test_decode_packet_invalid_data() {
        let result = decode_packet(&[0xDE, 0xAD, 0xBE, 0xEF]);
        assert!(matches!(result, Err(PacketError::Decode(_))));
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("deserialization"));
    }

    fn make_stream_packet() -> StreamPacket {
        use n42_execution::read_log::{encode_read_log, ReadLogEntry};
        let read_log = vec![
            ReadLogEntry::Account {
                nonce: 5,
                balance: U256::from(1_000_000u64),
                code_hash: B256::from([0xCC; 32]),
            },
            ReadLogEntry::Storage(U256::from(42u64)),
            ReadLogEntry::AccountNotFound,
            ReadLogEntry::BlockHash(B256::from([0xDD; 32])),
            ReadLogEntry::Account { nonce: 0, balance: U256::ZERO, code_hash: KECCAK256_EMPTY },
        ];
        StreamPacket {
            block_hash: B256::from([1u8; 32]),
            header_rlp: Bytes::from(vec![0xF8; 508]),
            transactions: vec![Bytes::from(vec![0xAA; 100]), Bytes::from(vec![0xBB; 200])],
            read_log_data: encode_read_log(&read_log),
            bytecodes: vec![(B256::from([0xEE; 32]), Bytes::from(vec![0x60, 0x00, 0xf3]))],
        }
    }

    #[test]
    fn test_stream_packet_encode_decode_roundtrip() {
        let packet = make_stream_packet();
        let encoded = encode_stream_packet(&packet);
        let decoded = decode_stream_packet(&encoded).expect("should decode");

        assert_eq!(decoded.block_hash, packet.block_hash);
        assert_eq!(decoded.header_rlp, packet.header_rlp);
        assert_eq!(decoded.transactions.len(), 2);
        assert_eq!(decoded.transactions[0], packet.transactions[0]);
        assert_eq!(decoded.transactions[1], packet.transactions[1]);
        assert_eq!(decoded.read_log_data, packet.read_log_data);
        assert_eq!(decoded.bytecodes.len(), 1);
        assert_eq!(decoded.bytecodes[0].0, packet.bytecodes[0].0);
        assert_eq!(decoded.bytecodes[0].1, packet.bytecodes[0].1);
    }

    #[test]
    fn test_stream_packet_empty() {
        use n42_execution::read_log::encode_read_log;
        let packet = StreamPacket {
            block_hash: B256::ZERO,
            header_rlp: Bytes::new(),
            transactions: vec![],
            read_log_data: encode_read_log(&[]),
            bytecodes: vec![],
        };
        let encoded = encode_stream_packet(&packet);
        let decoded = decode_stream_packet(&encoded).expect("should decode empty packet");
        assert_eq!(decoded.block_hash, B256::ZERO);
        assert!(decoded.header_rlp.is_empty());
        assert!(decoded.transactions.is_empty());
        assert_eq!(decoded.read_log_data, packet.read_log_data);
        assert!(decoded.bytecodes.is_empty());
    }

    #[test]
    fn test_stream_packet_invalid_magic() {
        assert!(decode_stream_packet(&[0xFF, 0xFF, 0x01, 0x00]).is_err());
    }

    #[test]
    fn test_stream_packet_wire_header() {
        let packet = make_stream_packet();
        let encoded = encode_stream_packet(&packet);
        assert_eq!(encoded[0], 0x4E);
        assert_eq!(encoded[1], 0x32);
        assert_eq!(encoded[2], 0x01);
        assert_ne!(encoded[3] & 0x01, 0); // FLAG_HAS_BYTECODES set
    }

    #[test]
    fn test_stream_packet_no_bytecodes_flag() {
        use n42_execution::read_log::encode_read_log;
        let packet = StreamPacket {
            block_hash: B256::ZERO,
            header_rlp: Bytes::new(),
            transactions: vec![],
            read_log_data: encode_read_log(&[]),
            bytecodes: vec![],
        };
        let encoded = encode_stream_packet(&packet);
        assert_eq!(encoded[3], 0x00);
    }

    #[test]
    fn test_v1_v2_size_comparison() {
        use n42_execution::read_log::{encode_read_log, ReadLogEntry};

        let mut witness_accounts = Vec::new();
        let mut read_log_entries = Vec::new();

        for i in 0u8..10 {
            let is_contract = i % 3 == 0;
            let code_hash =
                if is_contract { B256::from([0xC0 + i; 32]) } else { KECCAK256_EMPTY };
            let nonce = if i % 2 == 0 { i as u64 * 10 } else { 0 };
            let balance = if i > 3 { U256::from(i as u64 * 1_000_000) } else { U256::ZERO };
            let num_slots = (i % 5) as usize;

            let storage: Vec<(U256, U256)> = (0..num_slots)
                .map(|s| (U256::from(s), U256::from(s * 100 + i as usize)))
                .collect();
            witness_accounts.push(WitnessAccount {
                address: Address::with_last_byte(i + 1),
                nonce,
                balance,
                code_hash,
                storage,
            });

            read_log_entries.push(ReadLogEntry::Account { nonce, balance, code_hash });
            for s in 0..num_slots {
                read_log_entries.push(ReadLogEntry::Storage(U256::from(s * 100 + i as usize)));
            }
        }
        for i in 0u8..3 {
            read_log_entries.push(ReadLogEntry::BlockHash(B256::from([0xBB + i; 32])));
        }

        let header_rlp = Bytes::from(vec![0xF8; 508]);
        let txs = vec![Bytes::from(vec![0xAA; 200]); 5];
        let uncached_code = (B256::from([0xDD; 32]), Bytes::from(vec![0x60; 300]));

        let v1 = VerificationPacket {
            block_hash: B256::from([1u8; 32]),
            block_number: 12345,
            parent_hash: B256::from([2u8; 32]),
            state_root: B256::from([3u8; 32]),
            transactions_root: B256::from([4u8; 32]),
            receipts_root: B256::from([5u8; 32]),
            timestamp: 1_700_000_000,
            gas_limit: 30_000_000,
            beneficiary: Address::from([0xFF; 20]),
            header_rlp: header_rlp.clone(),
            transactions: txs.clone(),
            witness_accounts,
            uncached_bytecodes: vec![uncached_code.clone()],
            lowest_block_number: Some(12300),
            block_hashes: vec![
                (12300, B256::from([0xBB; 32])),
                (12301, B256::from([0xBC; 32])),
                (12302, B256::from([0xBD; 32])),
            ],
        };
        let v2 = StreamPacket {
            block_hash: B256::from([1u8; 32]),
            header_rlp,
            transactions: txs,
            read_log_data: encode_read_log(&read_log_entries),
            bytecodes: vec![uncached_code],
        };

        let v1_encoded = encode_packet(&v1).expect("V1 encode");
        let v2_encoded = encode_stream_packet(&v2);

        assert!(
            v2_encoded.len() < v1_encoded.len(),
            "V2 ({} bytes) should be smaller than V1 ({} bytes)",
            v2_encoded.len(),
            v1_encoded.len()
        );
    }
}
