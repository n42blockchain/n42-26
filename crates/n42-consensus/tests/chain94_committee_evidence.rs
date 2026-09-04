//! The committee-evidence link on real chain-94 headers.
//!
//! Two sources: a checked-in run of six consecutive raw header RLPs
//! (`testdata/chain94_headers_13560375_13560380.hex`, cut from the fleet's
//! node5 chaindata), and — when `N42_CHAIN94_RANGE_FILE` names a
//! `N42FRNG\x01` export (gov5 `export-range-linux.go`) — every header in it.

use alloy_primitives::B256;
use n42_consensus::{
    CommitteePoolConfig, Gov5NativeHeader, SimulatedCommitteePool, shared_committee_pool,
};
use std::sync::Arc;
use std::time::Instant;

/// `config.hotstuff.committeePool` of chain 94 (`mainnet_qmdb_staggered`).
fn chain94_pool() -> (Arc<SimulatedCommitteePool>, u128) {
    let config = CommitteePoolConfig {
        seed: "0x03c75de6b57f3563919956d11700f1d0c932e3c157506b23ed2c40d3ca47bb2f"
            .parse()
            .unwrap(),
        pool_size: 200_000,
        committee_size: 512,
        ramp_blocks: 1_000_000,
    };
    let started = Instant::now();
    let pool = shared_committee_pool(&config).unwrap();
    (pool, started.elapsed().as_millis())
}

/// Checks every parent→child pair of a run of consecutive native headers and
/// returns (links checked, total evidence time).
fn check_run(pool: &SimulatedCommitteePool, headers: &[(u64, Gov5NativeHeader)]) -> (usize, u128) {
    let mut links = 0;
    let mut spent = 0u128;
    for pair in headers.windows(2) {
        let (parent_number, parent) = &pair[0];
        let (child_number, child) = &pair[1];
        assert_eq!(parent_number + 1, *child_number, "consecutive run");
        assert_eq!(
            child.header.parent_hash,
            parent.hash(),
            "chain link at {child_number}"
        );
        let started = Instant::now();
        pool.verify_parent_link(
            *child_number,
            child.header.parent_beacon_block_root,
            *parent_number,
            &parent.hash(),
            &parent.header.receipts_root,
        )
        .unwrap_or_else(|error| panic!("block {child_number}: {error}"));
        spent += started.elapsed().as_micros();
        // A tampered parent (one receipts-root bit) must break the link.
        let mut tampered = parent.header.receipts_root;
        tampered.0[0] ^= 1;
        let broken = pool
            .verify_parent_link(
                *child_number,
                child.header.parent_beacon_block_root,
                *parent_number,
                &parent.hash(),
                &tampered,
            )
            .unwrap_err();
        assert!(
            broken
                .to_string()
                .contains("committee-evidence link broken")
        );
        links += 1;
    }
    (links, spent)
}

#[test]
fn checked_in_chain94_headers_link_through_rebuilt_evidence() {
    let headers: Vec<(u64, Gov5NativeHeader)> =
        include_str!("../testdata/chain94_headers_13560375_13560380.hex")
            .lines()
            .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
            .map(|line| {
                let (number, hex) = line.split_once(' ').unwrap();
                let raw = alloy_primitives::hex::decode(hex.trim()).unwrap();
                let header = Gov5NativeHeader::decode(&raw).unwrap();
                assert_eq!(header.header.number, number.parse::<u64>().unwrap());
                (header.header.number, header)
            })
            .collect();
    assert_eq!(headers.len(), 6);
    // The snapshot head of devlog-142 and the block five after it, as the
    // fleet's RPCs reported them.
    assert_eq!(
        headers[0].1.hash(),
        "0x0e37dae9d0cbf1c8e09c335654dc4cae3e18760dade40039e0e693368cc796d7"
            .parse::<B256>()
            .unwrap()
    );
    assert_eq!(
        headers[5].1.hash(),
        "0x25f834c3ca719fd788004b1483c20f910865f9eac143593bbd197438f6fc88cf"
            .parse::<B256>()
            .unwrap()
    );
    let (pool, derive_ms) = chain94_pool();
    let (links, spent_us) = check_run(&pool, &headers);
    assert_eq!(links, 5);
    eprintln!(
        "chain94 fixture: {links} links verified, pool derived in {derive_ms} ms, {} us/link",
        spent_us / links as u128
    );
}

/// The N42FRNG\x01 frame: header (magic, chain id, genesis, from, to, count),
/// per block (number, five hashes, header blob, block blob, receipts blob),
/// Blake3 trailer. Only the headers are needed here.
fn read_range_headers(path: &str) -> Vec<(u64, Gov5NativeHeader)> {
    let data = std::fs::read(path).unwrap();
    assert_eq!(&data[..8], b"N42FRNG\x01");
    let mut cursor = 8 + 8 + 32;
    let u64_at = |at: usize| u64::from_le_bytes(data[at..at + 8].try_into().unwrap());
    let from = u64_at(cursor);
    let count = u64_at(cursor + 16);
    cursor += 24;
    let mut out = Vec::with_capacity(count as usize);
    for index in 0..count {
        let number = u64_at(cursor);
        assert_eq!(number, from + index);
        cursor += 8 + 5 * 32;
        let blob = |cursor: &mut usize| {
            let len = u32::from_le_bytes(data[*cursor..*cursor + 4].try_into().unwrap()) as usize;
            let bytes = &data[*cursor + 4..*cursor + 4 + len];
            *cursor += 4 + len;
            bytes
        };
        let header_rlp = blob(&mut cursor).to_vec();
        blob(&mut cursor);
        blob(&mut cursor);
        let header = Gov5NativeHeader::decode(&header_rlp).unwrap();
        assert_eq!(header.header.number, number);
        out.push((number, header));
    }
    assert_eq!(cursor + 32, data.len());
    out
}

#[test]
fn recorded_chain94_range_links_through_rebuilt_evidence() {
    let Ok(path) = std::env::var("N42_CHAIN94_RANGE_FILE") else {
        eprintln!("N42_CHAIN94_RANGE_FILE not set: skipping the recorded-range check");
        return;
    };
    let headers = read_range_headers(&path);
    assert!(
        headers.len() >= 200,
        "need at least 200 consecutive headers"
    );
    let (pool, derive_ms) = chain94_pool();
    let started = Instant::now();
    let (links, spent_us) = check_run(&pool, &headers);
    eprintln!(
        "chain94 range {}..{}: {links} links verified in {} ms (pool derived in {derive_ms} ms, {} us/link mean)",
        headers[0].0,
        headers[headers.len() - 1].0,
        started.elapsed().as_millis(),
        spent_us / links as u128
    );
}
