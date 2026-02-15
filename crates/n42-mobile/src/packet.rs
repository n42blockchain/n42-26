use alloy_primitives::{Address, Bytes, B256, U256};
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
        // Header fields: ~200 bytes
        let header_size = 200;
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

/// Encodes a verification packet to bytes for transmission.
pub fn encode_packet(packet: &VerificationPacket) -> Result<Vec<u8>, String> {
    bincode::serialize(packet).map_err(|e| e.to_string())
}

/// Decodes a verification packet from bytes.
pub fn decode_packet(data: &[u8]) -> Result<VerificationPacket, String> {
    bincode::deserialize(data).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, Bytes, B256, U256};

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
        // - header: 200 bytes
        // - transactions: 100 + 200 = 300 bytes
        // - witness: 1 account with 2 storage slots = 100 + 2*64 = 228 bytes
        // - uncached bytecodes: 32 + 500 = 532 bytes
        // Total expected: 200 + 300 + 228 + 532 = 1260
        let expected = 200 + 300 + 228 + 532;
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
            transactions: vec![],
            witness_accounts: vec![],
            uncached_bytecodes: vec![],
            lowest_block_number: None,
            block_hashes: vec![],
        };
        assert_eq!(empty_packet.estimated_size(), 200, "empty packet should only have header overhead");
    }
}
