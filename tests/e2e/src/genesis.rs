use alloy_primitives::{Address, B256, U256, keccak256};
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// Number of pre-funded test accounts.
pub const NUM_TEST_ACCOUNTS: usize = 10;

/// Each test account starts with 100,000,000 N42 (100M * 10^18 wei).
const INITIAL_BALANCE: &str = "0x4B3B4CA85A86C47A098A224000000"; // 100_000_000 * 10^18

/// Chain ID for the test network.
pub const TEST_CHAIN_ID: u64 = 4242;

/// A test account with a known private key.
#[derive(Debug, Clone)]
pub struct TestAccount {
    pub private_key: B256,
    pub address: Address,
}

/// Generates deterministic test private keys using keccak256(b"n42-test-key-{i}").
pub fn generate_test_accounts() -> Vec<TestAccount> {
    (0..NUM_TEST_ACCOUNTS)
        .map(|i| {
            let seed = format!("n42-test-key-{i}");
            let private_key = keccak256(seed.as_bytes());
            let address = private_key_to_address(&private_key);
            TestAccount { private_key, address }
        })
        .collect()
}

/// Derives an Ethereum address from a secp256k1 private key.
fn private_key_to_address(private_key: &B256) -> Address {
    use alloy_signer_local::PrivateKeySigner;

    let signer = PrivateKeySigner::from_bytes(private_key)
        .expect("valid private key");
    signer.address()
}

/// Generates a genesis JSON configuration for the test network.
pub fn generate_genesis_json(accounts: &[TestAccount]) -> Value {
    let mut alloc = BTreeMap::new();
    for account in accounts {
        alloc.insert(
            format!("{:?}", account.address),
            json!({
                "balance": INITIAL_BALANCE
            }),
        );
    }

    json!({
        "config": {
            "chainId": TEST_CHAIN_ID,
            "homesteadBlock": 0,
            "eip150Block": 0,
            "eip155Block": 0,
            "eip158Block": 0,
            "byzantiumBlock": 0,
            "constantinopleBlock": 0,
            "petersburgBlock": 0,
            "istanbulBlock": 0,
            "muirGlacierBlock": 0,
            "berlinBlock": 0,
            "londonBlock": 0,
            "arrowGlacierBlock": 0,
            "grayGlacierBlock": 0,
            "mergeNetsplitBlock": 0,
            "shanghaiTime": 0,
            "cancunTime": 0,
            "terminalTotalDifficulty": "0x0",
            "terminalTotalDifficultyPassed": true
        },
        "nonce": "0x0",
        "timestamp": "0x0",
        "extraData": "0x",
        "gasLimit": "0x1C9C380",
        "difficulty": "0x0",
        "mixHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
        "coinbase": "0x0000000000000000000000000000000000000000",
        "alloc": alloc,
        "number": "0x0",
        "gasUsed": "0x0",
        "parentHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
        "baseFeePerGas": "0x3B9ACA00",
        "excessBlobGas": "0x0",
        "blobGasUsed": "0x0"
    })
}

/// Writes the genesis JSON to a file and returns the path.
pub fn write_genesis_file(dir: &std::path::Path, accounts: &[TestAccount]) -> std::path::PathBuf {
    let genesis = generate_genesis_json(accounts);
    let path = dir.join("genesis.json");
    std::fs::write(&path, serde_json::to_string_pretty(&genesis).unwrap()).unwrap();
    path
}

/// Returns the initial balance as a U256 value.
/// Must match the INITIAL_BALANCE hex constant used in genesis allocation.
pub fn initial_balance() -> U256 {
    U256::from_str_radix(&INITIAL_BALANCE[2..], 16).expect("valid hex balance")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_accounts_deterministic() {
        let accounts1 = generate_test_accounts();
        let accounts2 = generate_test_accounts();

        for (a, b) in accounts1.iter().zip(accounts2.iter()) {
            assert_eq!(a.private_key, b.private_key);
            assert_eq!(a.address, b.address);
        }
    }

    #[test]
    fn test_generate_accounts_unique() {
        let accounts = generate_test_accounts();
        for i in 0..accounts.len() {
            for j in (i + 1)..accounts.len() {
                assert_ne!(accounts[i].address, accounts[j].address);
            }
        }
    }

    #[test]
    fn test_genesis_json_has_all_accounts() {
        let accounts = generate_test_accounts();
        let genesis = generate_genesis_json(&accounts);
        let alloc = genesis["alloc"].as_object().unwrap();
        assert_eq!(alloc.len(), NUM_TEST_ACCOUNTS);
    }
}
