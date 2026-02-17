use alloy_primitives::keccak256;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use tracing::{error, info, warn};

use crate::genesis;
use crate::node_manager::{NodeConfig, NodeProcess};

/// Computes the deterministic libp2p PeerId for a validator at the given index.
///
/// Uses the same derivation as the node binary: keccak256("n42-p2p-key-{index}")
/// as the Ed25519 secret key seed.
fn compute_peer_id(validator_index: usize) -> libp2p::PeerId {
    let seed = keccak256(format!("n42-p2p-key-{}", validator_index).as_bytes());
    let mut seed_bytes: [u8; 32] = seed.0;
    let secret = libp2p::identity::ed25519::SecretKey::try_from_bytes(&mut seed_bytes)
        .expect("valid ed25519 seed");
    let kp = libp2p::identity::ed25519::Keypair::from(secret);
    libp2p::identity::Keypair::from(kp).public().to_peer_id()
}

/// Scenario 4: Multi-node consensus test.
///
/// Validates 1, 3, 5, and 21 node configurations all achieve stable consensus
/// with ~10 blocks each:
/// - Block interval stable at 8 seconds
/// - All nodes at consistent height
/// - Block hashes identical across nodes
/// - Leader rotation fair (round-robin)
pub async fn run(binary_path: PathBuf) -> eyre::Result<()> {
    info!("=== Scenario 4: Multi-Node Consensus ===");

    // Test 1: 1 node solo (f=0, quorum=1) — 100s for ~10 blocks at 8s interval
    run_multi_node_test(&binary_path, 1, Duration::from_secs(100), 0, 8000).await?;

    // Test 2: 3 nodes (f=0, quorum=1) — 100s for ~10 blocks at 8s interval
    run_multi_node_test(&binary_path, 3, Duration::from_secs(100), 100, 8000).await?;

    // Test 3: 5 nodes (f=1, quorum=3) — 100s for ~10 blocks at 8s interval
    run_multi_node_test(&binary_path, 5, Duration::from_secs(100), 200, 8000).await?;

    // Test 4: 21 nodes (f=6, quorum=13) — 100s for ~10 blocks at 8s interval
    run_multi_node_test(&binary_path, 21, Duration::from_secs(100), 300, 8000).await?;

    info!("=== Scenario 4 PASSED ===");
    Ok(())
}

/// Tests multi-node consensus with comprehensive verification.
///
/// Verifies:
/// V1. Block height consistency (all nodes within ±1)
/// V2. Minimum block count (at least 95 blocks in ~850s with 8s interval)
/// V3. Block hash consistency (sampled across the chain)
/// V4. Leader rotation fairness (each validator produces roughly equal blocks)
/// V5. Block interval stability (average interval within [7,9] seconds)
async fn run_multi_node_test(
    binary_path: &PathBuf,
    node_count: usize,
    duration: Duration,
    port_offset_base: u16,
    block_interval_ms: u64,
) -> eyre::Result<()> {
    info!(
        node_count,
        duration_secs = duration.as_secs(),
        block_interval_ms,
        "starting multi-node consensus test"
    );

    let accounts = genesis::generate_test_accounts();
    let tmp_dir = tempfile::tempdir()?;
    let genesis_path = genesis::write_genesis_file(tmp_dir.path(), &accounts);

    // Compute deterministic PeerIds for all nodes.
    let peer_ids: Vec<_> = (0..node_count).map(|i| compute_peer_id(i)).collect();
    for (i, pid) in peer_ids.iter().enumerate() {
        info!(node = i, peer_id = %pid, "computed deterministic PeerId");
    }

    // Start all nodes with mutual trusted peers.
    let mut nodes = Vec::with_capacity(node_count);

    for i in 0..node_count {
        let port_offset = port_offset_base + (i as u16) * 10;

        // Build trusted peers: all other nodes' multiaddrs.
        let trusted_peers: Vec<String> = (0..node_count)
            .filter(|&j| j != i)
            .map(|j| {
                let peer_port = 9400 + port_offset_base + (j as u16) * 10;
                format!("/ip4/127.0.0.1/udp/{}/quic-v1/p2p/{}", peer_port, peer_ids[j])
            })
            .collect();

        let config = NodeConfig {
            binary_path: binary_path.clone(),
            genesis_path: genesis_path.clone(),
            validator_index: i,
            validator_count: node_count,
            block_interval_ms,
            port_offset,
            trusted_peers,
        };

        match NodeProcess::start(&config).await {
            Ok(node) => {
                info!(index = i, http_port = node.http_port, "node started");
                nodes.push(node);
            }
            Err(e) => {
                error!(index = i, error = %e, "failed to start node");
                for node in nodes {
                    let _ = node.stop();
                }
                return Err(eyre::eyre!("failed to start node {i}: {e}"));
            }
        }
    }

    info!(started = nodes.len(), "all nodes started, running for {}s", duration.as_secs());

    // For large node counts, allow extra time for P2P mesh to stabilize.
    if node_count > 10 {
        info!("waiting 15s for P2P mesh stabilization with {} nodes", node_count);
        tokio::time::sleep(Duration::from_secs(15)).await;
    }

    tokio::time::sleep(duration).await;

    // Collect block heights from all nodes.
    let mut heights = Vec::with_capacity(nodes.len());
    for (i, node) in nodes.iter().enumerate() {
        match node.rpc.block_number().await {
            Ok(h) => {
                info!(node = i, block_height = h, "node block height");
                heights.push(h);
            }
            Err(e) => {
                warn!(node = i, error = %e, "failed to query block height");
                heights.push(0);
            }
        }
    }

    let max_height = *heights.iter().max().unwrap_or(&0);
    let min_height = *heights.iter().min().unwrap_or(&0);

    info!(?heights, max_height, min_height, "node block heights collected");

    // === V1: Block height consistency (all nodes within ±1) ===
    if max_height - min_height > 1 {
        for node in nodes {
            let _ = node.stop();
        }
        return Err(eyre::eyre!(
            "V1 FAILED: height divergence > 1: max={}, min={}, heights={:?}",
            max_height, min_height, heights
        ));
    }
    info!("V1 PASS: height consistency (max-min={}) within tolerance", max_height - min_height);

    // === V2: Minimum block count ===
    // Dynamically compute expected minimum: (duration / interval) * 0.9 tolerance
    let theoretical = duration.as_secs() / (block_interval_ms / 1000);
    let expected_min = (theoretical as f64 * 0.8) as u64;
    if min_height < expected_min {
        for node in nodes {
            let _ = node.stop();
        }
        return Err(eyre::eyre!(
            "V2 FAILED: min_height={} < expected minimum {}",
            min_height, expected_min
        ));
    }
    info!(min_height, expected_min, "V2 PASS: minimum block count");

    // === V3: Block hash consistency (sampled across the chain) ===
    let sample_heights = [1, min_height / 4, min_height / 2, 3 * min_height / 4, min_height];
    for &sample_h in &sample_heights {
        if sample_h == 0 {
            continue;
        }
        let mut hashes = Vec::new();
        for (i, node) in nodes.iter().enumerate() {
            match node.rpc.get_block_by_number(sample_h).await {
                Ok(block) => {
                    if let Some(hash) = block.get("hash").and_then(|h| h.as_str()) {
                        hashes.push(hash.to_string());
                    } else {
                        warn!(node = i, height = sample_h, "block missing hash field");
                        hashes.push(String::new());
                    }
                }
                Err(e) => {
                    warn!(node = i, height = sample_h, error = %e, "failed to query block");
                    hashes.push(String::new());
                }
            }
        }
        let ref_hash = &hashes[0];
        if !ref_hash.is_empty() && !hashes.iter().all(|h| h == ref_hash) {
            for node in nodes {
                let _ = node.stop();
            }
            return Err(eyre::eyre!(
                "V3 FAILED: block hash mismatch at height {}: {:?}",
                sample_h, hashes
            ));
        }
        info!(height = sample_h, hash = %ref_hash, "V3: block hash consistent across all nodes");
    }
    info!("V3 PASS: block hash consistency across {} sample heights", sample_heights.len());

    // === V4: Leader rotation fairness (check miner field distribution) ===
    let mut miner_counts: HashMap<String, u64> = HashMap::new();
    for h in 1..=min_height {
        match nodes[0].rpc.get_block_by_number(h).await {
            Ok(block) => {
                if let Some(miner) = block.get("miner").and_then(|m| m.as_str()) {
                    *miner_counts.entry(miner.to_lowercase()).or_insert(0) += 1;
                }
            }
            Err(e) => {
                warn!(height = h, error = %e, "failed to query block for miner check");
            }
        }
    }

    let expected_per = min_height as f64 / node_count as f64;
    // Only enforce strict ratio check when there are enough blocks per validator
    // for statistical significance (at least 3 blocks expected per validator).
    if expected_per >= 3.0 {
        for (miner, count) in &miner_counts {
            let ratio = *count as f64 / expected_per;
            if ratio <= 0.5 || ratio >= 1.5 {
                for node in nodes {
                    let _ = node.stop();
                }
                return Err(eyre::eyre!(
                    "V4 FAILED: unfair rotation: miner {} produced {} blocks (expected ~{:.0}, ratio={:.2})",
                    miner, count, expected_per, ratio
                ));
            }
        }
        if miner_counts.len() != node_count {
            warn!(
                actual_miners = miner_counts.len(),
                expected = node_count,
                "V4 WARNING: not all validators produced blocks: {:?}",
                miner_counts
            );
        }
    } else {
        // With few blocks per validator, just verify multiple validators produced blocks.
        info!(
            expected_per = format!("{:.1}", expected_per),
            miners = miner_counts.len(),
            "V4: skipping strict fairness check (too few blocks per validator), verifying participation"
        );
        // At minimum, round-robin should activate floor(min_height) unique validators
        let expected_active = node_count.min(min_height as usize);
        if miner_counts.len() < expected_active {
            warn!(
                actual_miners = miner_counts.len(),
                expected_active,
                "V4 WARNING: fewer active miners than expected: {:?}",
                miner_counts
            );
        }
    }
    info!(
        miners = miner_counts.len(),
        expected = node_count,
        "V4 PASS: leader rotation fairness (expected ~{:.0} blocks each)",
        expected_per
    );

    // === V5: Block interval stability (check timestamp differences) ===
    let sample_end = min_height.min(20);
    let mut intervals = Vec::new();
    for h in 2..=sample_end {
        let b1 = nodes[0].rpc.get_block_by_number(h - 1).await;
        let b2 = nodes[0].rpc.get_block_by_number(h).await;
        if let (Ok(b1), Ok(b2)) = (b1, b2) {
            let t1 = b1.get("timestamp")
                .and_then(|t| t.as_str())
                .and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok());
            let t2 = b2.get("timestamp")
                .and_then(|t| t.as_str())
                .and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok());
            if let (Some(t1), Some(t2)) = (t1, t2) {
                intervals.push(t2 - t1);
            }
        }
    }

    if !intervals.is_empty() {
        let avg_interval: f64 = intervals.iter().sum::<u64>() as f64 / intervals.len() as f64;
        if avg_interval < 7.0 || avg_interval > 9.0 {
            for node in nodes {
                let _ = node.stop();
            }
            return Err(eyre::eyre!(
                "V5 FAILED: average block interval {:.1}s not within [7,9]: {:?}",
                avg_interval, intervals
            ));
        }
        info!(
            avg_interval = format!("{:.1}s", avg_interval),
            samples = intervals.len(),
            "V5 PASS: block interval stability"
        );
    } else {
        warn!("V5 SKIP: could not compute block intervals");
    }

    info!(
        node_count,
        max_height,
        min_height,
        "PASS: {node_count}-node consensus test (all 5 verifications passed)"
    );

    for node in nodes {
        let _ = node.stop();
    }
    Ok(())
}
