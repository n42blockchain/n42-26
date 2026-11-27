//! Resident memory of the structures that grow with the chain, measured in
//! process (B5). Ignored benches; run one at a time so their RSS deltas do
//! not mix:
//!
//! `cargo test --release -p n42-node --test memory_footprint -- --ignored --nocapture --test-threads 1`

use alloy_primitives::{Address, B256, U256};
use n42_twig_core::{qmdb_compat::QmdbOperation, qmdb_leaf_tree::QmdbLeafTree};
use reth_execution_cache::ExecutionCache;
use reth_primitives_traits::Account;

#[cfg(unix)]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

fn resident_mib() -> f64 {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap();
    let pages: u64 = statm.split_whitespace().nth(1).unwrap().parse().unwrap();
    (pages * 4096) as f64 / (1 << 20) as f64
}

fn address(n: u64) -> Address {
    let mut bytes = [0u8; 20];
    bytes[..8].copy_from_slice(&n.wrapping_mul(0x9e37_79b9_7f4a_7c15).to_be_bytes());
    bytes[12..].copy_from_slice(&n.to_be_bytes());
    Address::from(bytes)
}

/// reth's cross-block execution cache at the node's default (4 GiB, which
/// N42 did not override) and at a small bound: what it costs
/// up front and per distinct account it has seen.
#[test]
#[ignore = "bench"]
fn cross_block_execution_cache() {
    for mib in [4096usize, 256] {
        let before = resident_mib();
        let cache = ExecutionCache::new(mib << 20);
        let created = resident_mib();
        let mut report = Vec::new();
        let mut inserted = 0u64;
        for target in [10_000u64, 100_000, 400_000, 1_000_000, 4_000_000] {
            while inserted < target {
                cache.insert_account(
                    address(inserted),
                    Some(Account {
                        nonce: 1,
                        balance: U256::from(inserted),
                        bytecode_hash: None,
                    }),
                );
                // A transfer workload touches one system storage slot per
                // ~50 accounts (history and beacon-root rings, rewards).
                if inserted.is_multiple_of(50) {
                    cache.insert_storage(
                        address(inserted),
                        B256::with_last_byte(1),
                        Some(U256::from(1)),
                    );
                }
                inserted += 1;
            }
            report.push(format!(
                "{target} accounts: +{:.0} MiB",
                resident_mib() - created
            ));
        }
        println!(
            "cross-block cache {mib} MiB: on creation +{:.0} MiB | {}",
            created - before,
            report.join(" | ")
        );
        drop(cache);
    }
}

/// The QMDB leaf tree (every live state entry in memory): resident bytes per
/// entry, with account-sized values (gov5 account encoding, ~10 bytes for a
/// transfer-only state as in S1's WAL).
#[test]
#[ignore = "bench"]
fn qmdb_leaf_tree_per_entry() {
    let before = resident_mib();
    let mut tree = QmdbLeafTree::new();
    let mut entries = 0u64;
    let mut report = Vec::new();
    for target in [100_000u64, 400_000, 1_000_000, 4_000_000] {
        while entries < target {
            let mut ops: Vec<QmdbOperation> = (entries..(entries + 10_000).min(target))
                .map(|n| QmdbOperation {
                    key: *alloy_primitives::keccak256(n.to_be_bytes()),
                    value: Some(vec![0x5a; 10]),
                })
                .collect();
            ops.sort_by_key(|op| op.key);
            entries += ops.len() as u64;
            tree.apply_sorted_ops(ops).unwrap();
        }
        let grown = resident_mib() - before;
        report.push(format!(
            "{target} entries: +{grown:.0} MiB ({:.0} B/entry)",
            grown * (1 << 20) as f64 / target as f64
        ));
    }
    println!("QMDB leaf tree: {}", report.join(" | "));
}
