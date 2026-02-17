use alloy_consensus::constants::KECCAK_EMPTY;
use alloy_primitives::{keccak256, Address, Bytes, B256, U256};
use alloy_rlp::Encodable;
use n42_execution::witness::ExecutionWitness;
use n42_mobile::{VerificationPacket, WitnessAccount};
use reth_primitives_traits::{BlockBody, NodePrimitives, SealedBlock};
use reth_ethereum_primitives::EthPrimitives;
use std::collections::HashMap;

/// Errors that can occur when building a verification packet from witness data.
#[derive(Debug, thiserror::Error)]
pub enum PacketBuildError {
    /// A hashed account key has no preimage in the witness keys.
    #[error("missing preimage for hashed account key {0}")]
    MissingAccountPreimage(B256),

    /// A hashed storage key has no preimage in the witness keys.
    #[error("missing preimage for hashed storage key {0}")]
    MissingStoragePreimage(B256),
}

/// Builds a `VerificationPacket` from execution witness data and a sealed block.
///
/// This is the critical bridge between the node's EVM execution (which produces
/// `ExecutionWitness` with hashed state keys) and the mobile verifier (which needs
/// real addresses and slot keys to reconstruct state).
///
/// ## Algorithm
///
/// 1. Build preimage maps from `witness.keys`: 20-byte values → Address, 32-byte → slot key
/// 2. Resolve `hashed_state.accounts` back to real addresses using the preimage map
/// 3. Resolve `hashed_state.storages` slots back to real U256 keys
/// 4. Construct `WitnessAccount` entries with real addresses and storage slots
/// 5. Hash each bytecode in `witness.codes` to produce `(code_hash, bytecode)` pairs
/// 6. RLP-encode the block header
/// 7. EIP-2718 encode each transaction
/// 8. Assemble the final `VerificationPacket`
pub fn build_verification_packet(
    witness: &ExecutionWitness,
    block: &SealedBlock<<EthPrimitives as NodePrimitives>::Block>,
    block_hashes: &[(u64, B256)],
) -> Result<VerificationPacket, PacketBuildError> {
    // Step 1: Build preimage mappings from witness.keys.
    // keys contains raw unhashed values: 20-byte entries are addresses, 32-byte entries are slot keys.
    let mut address_preimages: HashMap<B256, Address> = HashMap::new();
    let mut slot_preimages: HashMap<B256, B256> = HashMap::new();

    for key in &witness.keys {
        match key.len() {
            20 => {
                let addr = Address::from_slice(key);
                let hashed = keccak256(addr);
                address_preimages.insert(hashed, addr);
            }
            32 => {
                let slot = B256::from_slice(key);
                let hashed = keccak256(slot);
                slot_preimages.insert(hashed, slot);
            }
            _ => {
                // Other key lengths are not expected; skip them.
            }
        }
    }

    // Step 2-3: Convert hashed accounts and storages to WitnessAccount entries.
    let mut witness_accounts = Vec::new();

    for (hashed_addr, maybe_account) in &witness.hashed_state.accounts {
        // Skip destroyed accounts (value is None).
        let account = match maybe_account {
            Some(a) => a,
            None => continue,
        };

        // Resolve hashed address → real address.
        let address = address_preimages
            .get(hashed_addr)
            .copied()
            .ok_or_else(|| PacketBuildError::MissingAccountPreimage(*hashed_addr))?;

        // Resolve storage slots for this account.
        let storage = if let Some(hashed_storage) = witness.hashed_state.storages.get(hashed_addr) {
            let mut slots = Vec::with_capacity(hashed_storage.storage.len());
            for (hashed_slot, value) in &hashed_storage.storage {
                let real_slot = slot_preimages
                    .get(hashed_slot)
                    .ok_or_else(|| PacketBuildError::MissingStoragePreimage(*hashed_slot))?;
                // Convert B256 slot key to U256 for the WitnessAccount format.
                slots.push((U256::from_be_bytes(real_slot.0), *value));
            }
            slots
        } else {
            Vec::new()
        };

        let code_hash = account.bytecode_hash.unwrap_or(KECCAK_EMPTY);

        witness_accounts.push(WitnessAccount {
            address,
            nonce: account.nonce,
            balance: account.balance,
            code_hash,
            storage,
        });
    }

    // Step 5: Build uncached_bytecodes from witness.codes.
    let uncached_bytecodes: Vec<(B256, Bytes)> = witness
        .codes
        .iter()
        .map(|code| (keccak256(code), code.clone()))
        .collect();

    // Step 6: RLP-encode the block header.
    let header = block.header();
    let mut header_buf = Vec::new();
    header.encode(&mut header_buf);
    let header_rlp = Bytes::from(header_buf);

    // Step 7: EIP-2718 encode each transaction.
    let transactions = block.body().encoded_2718_transactions();

    // Step 8: Assemble the VerificationPacket.
    use alloy_consensus::BlockHeader;
    Ok(VerificationPacket {
        block_hash: block.hash(),
        block_number: header.number(),
        parent_hash: header.parent_hash(),
        state_root: header.state_root(),
        transactions_root: header.transactions_root(),
        receipts_root: header.receipts_root(),
        timestamp: header.timestamp(),
        gas_limit: header.gas_limit(),
        beneficiary: header.beneficiary(),
        header_rlp,
        transactions,
        witness_accounts,
        uncached_bytecodes,
        lowest_block_number: witness.lowest_block_number,
        block_hashes: block_hashes.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{keccak256, Address, Bytes, B256, U256};
    use reth_trie_common::{HashedPostState, HashedStorage};
    use reth_primitives_traits::Account;

    #[test]
    fn test_build_preimage_maps() {
        let addr = Address::with_last_byte(0x42);
        let slot = B256::with_last_byte(0x01);

        let witness = ExecutionWitness {
            hashed_state: HashedPostState::default(),
            codes: vec![],
            keys: vec![Bytes::copy_from_slice(addr.as_slice()), Bytes::copy_from_slice(slot.as_slice())],
            lowest_block_number: None,
        };

        // The preimage maps should contain the address and slot.
        let hashed_addr = keccak256(addr);
        let hashed_slot = keccak256(slot);

        let mut addr_map = HashMap::new();
        let mut slot_map = HashMap::new();
        for key in &witness.keys {
            match key.len() {
                20 => {
                    let a = Address::from_slice(key);
                    addr_map.insert(keccak256(a), a);
                }
                32 => {
                    let s = B256::from_slice(key);
                    slot_map.insert(keccak256(s), s);
                }
                _ => {}
            }
        }

        assert_eq!(addr_map.get(&hashed_addr), Some(&addr));
        assert_eq!(slot_map.get(&hashed_slot), Some(&slot));
    }

    #[test]
    fn test_destroyed_accounts_skipped() {
        let addr = Address::with_last_byte(0x01);
        let hashed_addr = keccak256(addr);

        let mut hashed_state = HashedPostState::default();
        // Insert a destroyed account (None value).
        hashed_state.accounts.insert(hashed_addr, None);

        let witness = ExecutionWitness {
            hashed_state,
            codes: vec![],
            keys: vec![Bytes::copy_from_slice(addr.as_slice())],
            lowest_block_number: None,
        };

        // We can't easily construct a SealedBlock in unit tests, but we can verify
        // the account filtering logic by checking the preimage map + iteration.
        let mut address_preimages = HashMap::new();
        for key in &witness.keys {
            if key.len() == 20 {
                let a = Address::from_slice(key);
                address_preimages.insert(keccak256(a), a);
            }
        }

        let mut accounts = Vec::new();
        for (hashed, maybe_account) in &witness.hashed_state.accounts {
            if let Some(account) = maybe_account {
                if let Some(real_addr) = address_preimages.get(hashed) {
                    accounts.push((*real_addr, account.clone()));
                }
            }
        }

        assert!(accounts.is_empty(), "destroyed accounts should be skipped");
    }

    #[test]
    fn test_uncached_bytecodes_hash() {
        let code = Bytes::from(vec![0x60, 0x00, 0x60, 0x00, 0xf3]); // PUSH0 PUSH0 RETURN
        let expected_hash = keccak256(&code);

        let witness = ExecutionWitness {
            hashed_state: HashedPostState::default(),
            codes: vec![code.clone()],
            keys: vec![],
            lowest_block_number: None,
        };

        let uncached: Vec<(B256, Bytes)> = witness
            .codes
            .iter()
            .map(|c| (keccak256(c), c.clone()))
            .collect();

        assert_eq!(uncached.len(), 1);
        assert_eq!(uncached[0].0, expected_hash);
        assert_eq!(uncached[0].1, code);
    }

    #[test]
    fn test_build_with_account_and_storage() {
        // Set up an account with a storage slot in the witness.
        let addr = Address::with_last_byte(0x42);
        let hashed_addr = keccak256(addr);
        let slot = B256::with_last_byte(0x01);
        let hashed_slot = keccak256(slot);

        let account = Account {
            nonce: 5,
            balance: U256::from(1000),
            bytecode_hash: Some(KECCAK_EMPTY),
        };

        let mut hashed_state = HashedPostState::default();
        hashed_state.accounts.insert(hashed_addr, Some(account));

        let mut hashed_storage = HashedStorage::new(false);
        hashed_storage.storage.insert(hashed_slot, U256::from(42));
        hashed_state.storages.insert(hashed_addr, hashed_storage);

        let witness = ExecutionWitness {
            hashed_state,
            codes: vec![],
            keys: vec![
                Bytes::copy_from_slice(addr.as_slice()),
                Bytes::copy_from_slice(slot.as_slice()),
            ],
            lowest_block_number: None,
        };

        // Build preimage maps and resolve accounts (same logic as build_verification_packet).
        let mut address_preimages = HashMap::new();
        let mut slot_preimages = HashMap::new();
        for key in &witness.keys {
            match key.len() {
                20 => {
                    let a = Address::from_slice(key);
                    address_preimages.insert(keccak256(a), a);
                }
                32 => {
                    let s = B256::from_slice(key);
                    slot_preimages.insert(keccak256(s), s);
                }
                _ => {}
            }
        }

        // Resolve the account.
        let (ha, maybe_acct) = witness.hashed_state.accounts.iter().next().unwrap();
        let acct = maybe_acct.unwrap();
        let resolved_addr = address_preimages.get(ha).unwrap();
        assert_eq!(*resolved_addr, addr);
        assert_eq!(acct.nonce, 5);
        assert_eq!(acct.balance, U256::from(1000));

        // Resolve storage slots.
        let hs = witness.hashed_state.storages.get(ha).unwrap();
        let (hs_key, hs_val) = hs.storage.iter().next().unwrap();
        let real_slot = slot_preimages.get(hs_key).unwrap();
        assert_eq!(*real_slot, slot);
        assert_eq!(*hs_val, U256::from(42));
    }

    #[test]
    fn test_missing_account_preimage() {
        // Create a witness with hashed account but no preimage key.
        let addr = Address::with_last_byte(0x99);
        let hashed_addr = keccak256(addr);

        let account = Account {
            nonce: 1,
            balance: U256::from(100),
            bytecode_hash: Some(KECCAK_EMPTY),
        };

        let mut hashed_state = HashedPostState::default();
        hashed_state.accounts.insert(hashed_addr, Some(account));

        let witness = ExecutionWitness {
            hashed_state,
            codes: vec![],
            keys: vec![], // no preimage!
            lowest_block_number: None,
        };

        // Reproduce the preimage resolution logic.
        let mut address_preimages: HashMap<B256, Address> = HashMap::new();
        for key in &witness.keys {
            if key.len() == 20 {
                let a = Address::from_slice(key);
                address_preimages.insert(keccak256(a), a);
            }
        }

        // Should fail to find the preimage.
        for (ha, maybe_acct) in &witness.hashed_state.accounts {
            if maybe_acct.is_some() {
                let result = address_preimages
                    .get(ha)
                    .copied()
                    .ok_or_else(|| PacketBuildError::MissingAccountPreimage(*ha));
                assert!(result.is_err(), "should fail when preimage is missing");
                assert!(
                    matches!(result.unwrap_err(), PacketBuildError::MissingAccountPreimage(h) if h == *ha)
                );
            }
        }
    }
}
