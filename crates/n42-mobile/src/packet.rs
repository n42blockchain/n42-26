use alloy_primitives::{Address, Bytes, B256, U256};
use n42_execution::read_log::{self, ReadLogEntry};
use serde::{Deserialize, Serialize};

/// A single account's state included in the verification witness.
///
/// Contains all data a mobile verifier needs to construct a temporary
/// in-memory state DB for this account. Storage only includes slots
/// that are actually accessed during block execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WitnessAccount {
    /// Account address.
    pub address: Address,
    /// Account nonce.
    pub nonce: u64,
    /// Account balance.
    pub balance: U256,
    /// Hash of the account's contract code (or keccak256(&[]) for EOAs).
    pub code_hash: B256,
    /// Storage slots accessed during execution: (slot_key, value).
    pub storage: Vec<(U256, U256)>,
}

/// The complete verification packet sent from an IDC node to mobile devices.
///
/// Contains everything a phone needs to independently re-execute a block
/// and verify the resulting state root. The packet is designed to be
/// self-contained — no additional chain state lookups are required.
///
/// ## Data flow
///
/// 1. IDC executes block, captures `ExecutionWitness` (all state accessed)
/// 2. IDC compacts witness by removing bytecodes the phone has cached
/// 3. IDC constructs `VerificationPacket` from header + txs + compact witness
/// 4. IDC pushes packet to connected phones via QUIC
/// 5. Phone reconstructs temporary state DB → re-executes txs → verifies roots
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationPacket {
    /// Hash of the block being verified.
    pub block_hash: B256,
    /// Block number.
    pub block_number: u64,
    /// Parent block hash.
    pub parent_hash: B256,
    /// Expected post-execution state root (phone verifies this).
    pub state_root: B256,
    /// Expected transactions root.
    pub transactions_root: B256,
    /// Expected receipts root.
    pub receipts_root: B256,
    /// Block timestamp.
    pub timestamp: u64,
    /// Block gas limit.
    pub gas_limit: u64,
    /// Beneficiary address (coinbase).
    pub beneficiary: Address,
    /// Full RLP-encoded block header.
    /// Contains all fields needed for EVM execution (base_fee, prevrandao, etc.)
    /// that aren't covered by the individual convenience fields above.
    pub header_rlp: Bytes,
    /// RLP-encoded transactions for re-execution.
    pub transactions: Vec<Bytes>,
    /// Account states accessed during execution (the witness).
    pub witness_accounts: Vec<WitnessAccount>,
    /// Contract bytecodes NOT in the phone's cache: (code_hash, bytecode).
    /// Phone combines these with its local cache to get all needed code.
    pub uncached_bytecodes: Vec<(B256, Bytes)>,
    /// Lowest block number referenced by BLOCKHASH opcode.
    /// Phone may need ancestor block hashes for verification.
    pub lowest_block_number: Option<u64>,
    /// Ancestor block hashes needed for BLOCKHASH opcode: (number, hash).
    pub block_hashes: Vec<(u64, B256)>,
}

impl VerificationPacket {
    /// Returns the approximate serialized size in bytes.
    pub fn estimated_size(&self) -> usize {
        // Header fields: ~200 bytes + header_rlp
        let header_size = 200 + self.header_rlp.len();
        // Transactions: sum of encoded sizes
        let tx_size: usize = self.transactions.iter().map(|t| t.len()).sum();
        // Witness accounts: ~100 bytes per account + storage
        let witness_size: usize = self
            .witness_accounts
            .iter()
            .map(|a| 100 + a.storage.len() * 64)
            .sum();
        // Uncached bytecodes
        let code_size: usize = self.uncached_bytecodes.iter().map(|(_, c)| 32 + c.len()).sum();

        header_size + tx_size + witness_size + code_size
    }
}

/// Errors from packet encoding/decoding.
#[derive(Debug, thiserror::Error)]
pub enum PacketError {
    /// Failed to serialize the verification packet.
    #[error("packet serialization failed: {0}")]
    Encode(#[from] bincode::Error),
    /// Failed to deserialize the verification packet.
    #[error("packet deserialization failed: {0}")]
    Decode(bincode::Error),
}

/// Encodes a verification packet to bytes for transmission.
pub fn encode_packet(packet: &VerificationPacket) -> Result<Vec<u8>, PacketError> {
    bincode::serialize(packet).map_err(PacketError::Encode)
}

/// Decodes a verification packet from bytes.
pub fn decode_packet(data: &[u8]) -> Result<VerificationPacket, PacketError> {
    bincode::deserialize(data).map_err(PacketError::Decode)
}

// ─── StreamPacket V1 ────────────────────────────────────────────────────────

/// V2 verification packet using ordered value streams instead of keyed witness.
///
/// The core insight: same block + same txs → revm Database calls happen in a
/// 100% deterministic order. So we drop address/slot keys and send only values
/// in call order. The phone replays entries sequentially via `StreamReplayDB`.
///
/// ## Format (encoded)
/// ```text
/// wire_header(4B) + block_metadata + read_log + bytecodes
/// ```
#[derive(Debug, Clone)]
pub struct StreamPacket {
    /// Hash of the block being verified.
    pub block_hash: B256,
    /// Full RLP-encoded block header.
    pub header_rlp: Bytes,
    /// EIP-2718 encoded transactions.
    pub transactions: Vec<Bytes>,
    /// Ordered state read log entries (replayed sequentially by phone).
    pub read_log: Vec<ReadLogEntry>,
    /// Contract bytecodes NOT in the phone's cache: (code_hash, bytecode).
    pub bytecodes: Vec<(B256, Bytes)>,
}

impl StreamPacket {
    /// Returns the approximate serialized size in bytes.
    pub fn estimated_size(&self) -> usize {
        let header_size = 4 + 32 + 4 + self.header_rlp.len(); // wire + hash + rlp_len + rlp
        let tx_size: usize = self.transactions.iter().map(|t| 4 + t.len()).sum();
        let log_size = self.read_log.len() * 40; // rough estimate
        let code_size: usize = self.bytecodes.iter().map(|(_, c)| 32 + 4 + c.len()).sum();
        header_size + tx_size + log_size + code_size
    }
}

/// Encodes a `StreamPacket` into compact binary format with wire header.
pub fn encode_stream_packet(packet: &StreamPacket) -> Vec<u8> {
    use crate::wire::{self, FLAG_HAS_BYTECODES};

    let flags = if packet.bytecodes.is_empty() { 0x00 } else { FLAG_HAS_BYTECODES };
    let mut buf = Vec::with_capacity(packet.estimated_size());

    // Wire header
    wire::encode_header(&mut buf, wire::VERSION_1, flags);

    // Block metadata
    buf.extend_from_slice(packet.block_hash.as_slice()); // 32B

    // Header RLP
    buf.extend_from_slice(&(packet.header_rlp.len() as u32).to_le_bytes());
    buf.extend_from_slice(&packet.header_rlp);

    // Transactions
    buf.extend_from_slice(&(packet.transactions.len() as u32).to_le_bytes());
    for tx in &packet.transactions {
        buf.extend_from_slice(&(tx.len() as u32).to_le_bytes());
        buf.extend_from_slice(tx);
    }

    // State read log (delegates to read_log module's compact encoder)
    let log_bytes = read_log::encode_read_log(&packet.read_log);
    buf.extend_from_slice(&log_bytes);

    // Bytecodes section
    buf.extend_from_slice(&(packet.bytecodes.len() as u32).to_le_bytes());
    for (hash, code) in &packet.bytecodes {
        buf.extend_from_slice(hash.as_slice());
        buf.extend_from_slice(&(code.len() as u32).to_le_bytes());
        buf.extend_from_slice(code);
    }

    buf
}

/// Decodes a `StreamPacket` from compact binary format.
pub fn decode_stream_packet(data: &[u8]) -> Result<StreamPacket, crate::wire::WireError> {
    use crate::wire::{self, WireError};

    let (_header, payload) = wire::decode_header(data)?;
    let mut pos: usize = 0;

    // Helper closures
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

    // Block hash
    ensure(pos, 32)?;
    let block_hash = B256::from_slice(&payload[pos..pos + 32]);
    pos += 32;

    // Header RLP
    ensure(pos, 4)?;
    let header_rlp_len = u32::from_le_bytes(payload[pos..pos + 4].try_into().unwrap()) as usize;
    pos += 4;
    ensure(pos, header_rlp_len)?;
    let header_rlp = Bytes::copy_from_slice(&payload[pos..pos + header_rlp_len]);
    pos += header_rlp_len;

    // Transactions
    ensure(pos, 4)?;
    let tx_count = u32::from_le_bytes(payload[pos..pos + 4].try_into().unwrap()) as usize;
    pos += 4;
    let mut transactions = Vec::with_capacity(tx_count);
    for _ in 0..tx_count {
        ensure(pos, 4)?;
        let tx_len = u32::from_le_bytes(payload[pos..pos + 4].try_into().unwrap()) as usize;
        pos += 4;
        ensure(pos, tx_len)?;
        transactions.push(Bytes::copy_from_slice(&payload[pos..pos + tx_len]));
        pos += tx_len;
    }

    // State read log
    let log_data = &payload[pos..];
    let read_log = read_log::decode_read_log(log_data)
        .map_err(|_| WireError::InvalidTag(0, pos))?;

    // Advance pos past the read log section:
    // entry_count(4) + per entry: tag(1) + data_len(2) + data
    pos += 4; // entry_count
    let entry_count = u32::from_le_bytes(payload[pos - 4..pos].try_into().unwrap()) as usize;
    for _ in 0..entry_count {
        pos += 1; // tag
        ensure(pos, 2)?;
        let data_len = u16::from_le_bytes(payload[pos..pos + 2].try_into().unwrap()) as usize;
        pos += 2;
        pos += data_len;
    }

    // Bytecodes section
    ensure(pos, 4)?;
    let code_count = u32::from_le_bytes(payload[pos..pos + 4].try_into().unwrap()) as usize;
    pos += 4;
    let mut bytecodes = Vec::with_capacity(code_count);
    for _ in 0..code_count {
        ensure(pos, 36)?; // 32 hash + 4 len
        let hash = B256::from_slice(&payload[pos..pos + 32]);
        pos += 32;
        let code_len = u32::from_le_bytes(payload[pos..pos + 4].try_into().unwrap()) as usize;
        pos += 4;
        ensure(pos, code_len)?;
        let code = Bytes::copy_from_slice(&payload[pos..pos + code_len]);
        pos += code_len;
        bytecodes.push((hash, code));
    }

    Ok(StreamPacket {
        block_hash,
        header_rlp,
        transactions,
        read_log,
        bytecodes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, Bytes, B256, U256, KECCAK256_EMPTY};

    /// Helper: creates a minimal VerificationPacket with known contents.
    fn make_packet() -> VerificationPacket {
        let tx1 = Bytes::from(vec![0xAA; 100]);
        let tx2 = Bytes::from(vec![0xBB; 200]);

        let account = WitnessAccount {
            address: Address::ZERO,
            nonce: 5,
            balance: U256::from(1_000_000u64),
            code_hash: B256::from([0xCC; 32]),
            storage: vec![
                (U256::from(0), U256::from(42)),
                (U256::from(1), U256::from(99)),
            ],
        };

        let uncached_code = (B256::from([0xDD; 32]), Bytes::from(vec![0xEE; 500]));

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
            transactions: vec![tx1, tx2],
            witness_accounts: vec![account],
            uncached_bytecodes: vec![uncached_code],
            lowest_block_number: Some(12300),
            block_hashes: vec![(12300, B256::from([6u8; 32]))],
        }
    }

    #[test]
    fn test_packet_encode_decode_roundtrip() {
        let packet = make_packet();

        // Encode.
        let encoded = encode_packet(&packet).expect("encoding should succeed");
        assert!(!encoded.is_empty(), "encoded bytes must not be empty");

        // Decode.
        let decoded = decode_packet(&encoded).expect("decoding should succeed");

        // Verify all fields match.
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

        // The packet contains:
        // - header: 200 bytes + 508 bytes header_rlp = 708 bytes
        // - transactions: 100 + 200 = 300 bytes
        // - witness: 1 account with 2 storage slots = 100 + 2*64 = 228 bytes
        // - uncached bytecodes: 32 + 500 = 532 bytes
        // Total expected: 708 + 300 + 228 + 532 = 1768
        let expected = 708 + 300 + 228 + 532;
        assert_eq!(size, expected, "estimated_size should match manual calculation");

        // The estimated size must be strictly positive for any non-empty packet.
        assert!(size > 0);

        // An empty packet should still have the header overhead.
        let empty_packet = VerificationPacket {
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
        assert_eq!(empty_packet.estimated_size(), 200, "empty packet should only have header overhead");
    }

    #[test]
    fn test_decode_packet_invalid_data() {
        let garbage = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let result = decode_packet(&garbage);
        assert!(result.is_err(), "garbage data should fail to decode");
        let err = result.unwrap_err();
        assert!(matches!(err, PacketError::Decode(_)), "should be a Decode error variant");
        let msg = format!("{}", err);
        assert!(msg.contains("deserialization"), "error message should mention deserialization");
    }

    // ── StreamPacket tests ──

    fn make_stream_packet() -> StreamPacket {
        StreamPacket {
            block_hash: B256::from([1u8; 32]),
            header_rlp: Bytes::from(vec![0xF8; 508]),
            transactions: vec![
                Bytes::from(vec![0xAA; 100]),
                Bytes::from(vec![0xBB; 200]),
            ],
            read_log: vec![
                ReadLogEntry::Account {
                    nonce: 5,
                    balance: U256::from(1_000_000u64),
                    code_hash: B256::from([0xCC; 32]),
                },
                ReadLogEntry::Storage(U256::from(42u64)),
                ReadLogEntry::AccountNotFound,
                ReadLogEntry::BlockHash(B256::from([0xDD; 32])),
                ReadLogEntry::Account {
                    nonce: 0,
                    balance: U256::ZERO,
                    code_hash: KECCAK256_EMPTY,
                },
            ],
            bytecodes: vec![
                (B256::from([0xEE; 32]), Bytes::from(vec![0x60, 0x00, 0xf3])),
            ],
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
        assert_eq!(decoded.read_log.len(), 5);
        assert_eq!(decoded.read_log, packet.read_log);
        assert_eq!(decoded.bytecodes.len(), 1);
        assert_eq!(decoded.bytecodes[0].0, packet.bytecodes[0].0);
        assert_eq!(decoded.bytecodes[0].1, packet.bytecodes[0].1);
    }

    #[test]
    fn test_stream_packet_empty() {
        let packet = StreamPacket {
            block_hash: B256::ZERO,
            header_rlp: Bytes::new(),
            transactions: vec![],
            read_log: vec![],
            bytecodes: vec![],
        };
        let encoded = encode_stream_packet(&packet);
        let decoded = decode_stream_packet(&encoded).expect("should decode empty packet");

        assert_eq!(decoded.block_hash, B256::ZERO);
        assert!(decoded.header_rlp.is_empty());
        assert!(decoded.transactions.is_empty());
        assert!(decoded.read_log.is_empty());
        assert!(decoded.bytecodes.is_empty());
    }

    #[test]
    fn test_stream_packet_invalid_magic() {
        let result = decode_stream_packet(&[0xFF, 0xFF, 0x01, 0x00]);
        assert!(result.is_err());
    }

    #[test]
    fn test_stream_packet_wire_header() {
        let packet = make_stream_packet();
        let encoded = encode_stream_packet(&packet);
        // Check wire header
        assert_eq!(encoded[0], 0x4E); // 'N'
        assert_eq!(encoded[1], 0x32); // '2'
        assert_eq!(encoded[2], 0x01); // VERSION_1
        // Has bytecodes → FLAG_HAS_BYTECODES set
        assert_ne!(encoded[3] & 0x01, 0);
    }

    #[test]
    fn test_stream_packet_no_bytecodes_flag() {
        let packet = StreamPacket {
            block_hash: B256::ZERO,
            header_rlp: Bytes::new(),
            transactions: vec![],
            read_log: vec![],
            bytecodes: vec![],  // empty
        };
        let encoded = encode_stream_packet(&packet);
        assert_eq!(encoded[3], 0x00); // no FLAG_HAS_BYTECODES
    }
}
