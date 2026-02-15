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
