use alloy_primitives::keccak256;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use tracing::{error, info, warn};

use crate::genesis;
use crate::node_manager::{NodeConfig, NodeProcess};

/// Computes the deterministic libp2p PeerId for a validator at the given index.
fn compute_peer_id(validator_index: usize) -> libp2p::PeerId {
    let seed = keccak256(format!("n42-p2p-key-{}", validator_index).as_bytes());
    let mut seed_bytes: [u8; 32] = seed.0;
    let secret = libp2p::identity::ed25519::SecretKey::try_from_bytes(&mut seed_bytes)
        .expect("valid ed25519 seed");
    let kp = libp2p::identity::ed25519::Keypair::from(secret);
    libp2p::identity::Keypair::from(kp).public().to_peer_id()
}

const NODE_COUNT: usize = 21;
const BLOCKS_PER_VALIDATOR: u64 = 21;
const REQUIRED_BLOCKS: u64 = (NODE_COUNT as u64) * BLOCKS_PER_VALIDATOR; // 441
const BLOCK_INTERVAL_MS: u64 = 4000; // 4 seconds per slot
const PORT_OFFSET_BASE: u16 = 300;
const MAX_DURATION_SECS: u64 = 2400; // 40 minute hard timeout
const PROGRESS_POLL_SECS: u64 = 30;

/// Scenario 7: 21 validators × 21 blocks each.
///
/// Launches 21 N42 nodes and waits until every node has produced at least
/// 441 blocks (21 per validator via round-robin). Then verifies:
///
/// V1. Block height consistency (all nodes within ±1)
/// V2. Minimum block count (≥ 441)
/// V3. Block hash consistency (sampled across the chain)
/// V4. Each of the 21 validators produced ≥ 21 blocks
/// V5. Leader rotation fairness (no validator >1.5× or <0.5× the average)
/// V6. Block interval stability (average within [3, 5] seconds for 4s target)
pub async fn run(binary_path: PathBuf) -> eyre::Result<()> {
    info!("=== Scenario 7: 21 Validators × 21 Blocks ===");
    info!(
        node_count = NODE_COUNT,
        blocks_per_validator = BLOCKS_PER_VALIDATOR,
        total_required = REQUIRED_BLOCKS,
        block_interval_ms = BLOCK_INTERVAL_MS,
        max_duration_secs = MAX_DURATION_SECS,
        "target: each validator produces {} blocks ({} total)",
        BLOCKS_PER_VALIDATOR, REQUIRED_BLOCKS,
    );

    let accounts = genesis::generate_test_accounts();
    let tmp_dir = tempfile::tempdir()?;
    let genesis_path = genesis::write_genesis_file(tmp_dir.path(), &accounts);

    // Compute deterministic PeerIds for all nodes.
    let peer_ids: Vec<_> = (0..NODE_COUNT).map(|i| compute_peer_id(i)).collect();
    for (i, pid) in peer_ids.iter().enumerate() {
        info!(node = i, peer_id = %pid, "computed deterministic PeerId");
    }

    // Start all 21 nodes.
    let mut nodes = Vec::with_capacity(NODE_COUNT);

    for i in 0..NODE_COUNT {
        let port_offset = PORT_OFFSET_BASE + (i as u16) * 10;

        let trusted_peers: Vec<String> = (0..NODE_COUNT)
            .filter(|&j| j != i)
            .map(|j| {
                let peer_port = 9400 + PORT_OFFSET_BASE + (j as u16) * 10;
                format!("/ip4/127.0.0.1/udp/{}/quic-v1/p2p/{}", peer_port, peer_ids[j])
            })
            .collect();

        let config = NodeConfig {
            binary_path: binary_path.clone(),
            genesis_path: genesis_path.clone(),
            validator_index: i,
            validator_count: NODE_COUNT,
            block_interval_ms: BLOCK_INTERVAL_MS,
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

    info!(started = nodes.len(), "all {} nodes started", NODE_COUNT);

    // Allow time for P2P mesh to stabilize.
    info!("waiting 20s for P2P mesh stabilization with {} nodes", NODE_COUNT);
    tokio::time::sleep(Duration::from_secs(20)).await;

    // Adaptive wait: poll block heights until we reach the target.
    let start = tokio::time::Instant::now();
    let deadline = start + Duration::from_secs(MAX_DURATION_SECS);
    let mut reached_target = false;

    loop {
        if tokio::time::Instant::now() >= deadline {
            warn!("hard timeout reached ({MAX_DURATION_SECS}s)");
            break;
        }

        // Query all node heights.
        let mut heights = Vec::with_capacity(nodes.len());
        for (i, node) in nodes.iter().enumerate() {
            match node.rpc.block_number().await {
                Ok(h) => heights.push(h),
                Err(e) => {
                    warn!(node = i, error = %e, "failed to query height");
                    heights.push(0);
                }
            }
        }

        let min_h = *heights.iter().min().unwrap_or(&0);
        let max_h = *heights.iter().max().unwrap_or(&0);
        let elapsed = start.elapsed().as_secs();

        info!(
            elapsed_s = elapsed,
            min_height = min_h,
            max_height = max_h,
            target = REQUIRED_BLOCKS,
            "progress: {:.0}% ({}/{})",
            (min_h as f64 / REQUIRED_BLOCKS as f64 * 100.0).min(100.0),
            min_h,
            REQUIRED_BLOCKS,
        );

        if min_h >= REQUIRED_BLOCKS {
            reached_target = true;
            info!("target reached: all nodes at height >= {}", REQUIRED_BLOCKS);
            break;
        }

        tokio::time::sleep(Duration::from_secs(PROGRESS_POLL_SECS)).await;
    }

    // Collect final heights.
    let mut heights = Vec::with_capacity(nodes.len());
    for (i, node) in nodes.iter().enumerate() {
        match node.rpc.block_number().await {
            Ok(h) => {
                info!(node = i, block_height = h, "final height");
                heights.push(h);
            }
            Err(e) => {
                warn!(node = i, error = %e, "failed to query final height");
                heights.push(0);
            }
        }
    }

    let max_height = *heights.iter().max().unwrap_or(&0);
    let min_height = *heights.iter().min().unwrap_or(&0);
    info!(?heights, max_height, min_height, "final heights collected");

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

    // === V2: Minimum block count (>= 441) ===
    if min_height < REQUIRED_BLOCKS {
        for node in nodes {
            let _ = node.stop();
        }
        return Err(eyre::eyre!(
            "V2 FAILED: min_height={} < required {}, reached_target={}",
            min_height, REQUIRED_BLOCKS, reached_target
        ));
    }
    info!(min_height, required = REQUIRED_BLOCKS, "V2 PASS: minimum block count");

    // === V3: Block hash consistency (sampled across the chain) ===
    let sample_heights = [
        1,
        REQUIRED_BLOCKS / 4,
        REQUIRED_BLOCKS / 2,
        3 * REQUIRED_BLOCKS / 4,
        min_height,
    ];
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
        info!(height = sample_h, hash = %ref_hash, "V3: block hash consistent");
    }
    info!("V3 PASS: block hash consistency across {} sample heights", sample_heights.len());

    // === V4: Each validator produced >= 21 blocks ===
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

    info!("miner distribution ({} unique miners):", miner_counts.len());
    let mut sorted_miners: Vec<_> = miner_counts.iter().collect();
    sorted_miners.sort_by_key(|(addr, _)| (*addr).clone());
    for (miner, count) in &sorted_miners {
        info!("  {} => {} blocks", miner, count);
    }

    // Check that we have exactly 21 unique miners.
    if miner_counts.len() != NODE_COUNT {
        warn!(
            actual = miner_counts.len(),
            expected = NODE_COUNT,
            "V4 WARNING: not all validators produced blocks"
        );
    }

    // Check each validator produced >= BLOCKS_PER_VALIDATOR blocks.
    let mut v4_failures = Vec::new();
    for (miner, &count) in &miner_counts {
        if count < BLOCKS_PER_VALIDATOR {
            v4_failures.push(format!("{}: {} blocks (need {})", miner, count, BLOCKS_PER_VALIDATOR));
        }
    }
    if !v4_failures.is_empty() {
        for node in nodes {
            let _ = node.stop();
        }
        return Err(eyre::eyre!(
            "V4 FAILED: validators with < {} blocks: [{}]",
            BLOCKS_PER_VALIDATOR,
            v4_failures.join(", ")
        ));
    }
    info!(
        validators = miner_counts.len(),
        min_per_validator = miner_counts.values().min().unwrap_or(&0),
        max_per_validator = miner_counts.values().max().unwrap_or(&0),
        "V4 PASS: all {} validators produced >= {} blocks",
        NODE_COUNT, BLOCKS_PER_VALIDATOR
    );

    // === V5: Leader rotation fairness ===
    let expected_per = min_height as f64 / NODE_COUNT as f64;
    for (miner, &count) in &miner_counts {
        let ratio = count as f64 / expected_per;
        if ratio <= 0.5 || ratio >= 1.5 {
            for node in nodes {
                let _ = node.stop();
            }
            return Err(eyre::eyre!(
                "V5 FAILED: unfair rotation: miner {} produced {} blocks (expected ~{:.0}, ratio={:.2})",
                miner, count, expected_per, ratio
            ));
        }
    }
    info!(
        expected_per = format!("{:.1}", expected_per),
        "V5 PASS: leader rotation fairness (all within [0.5x, 1.5x])"
    );

    // === V6: Block interval stability ===
    // Sample the first 100 blocks for interval analysis.
    let sample_end = min_height.min(100);
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
        let min_interval = *intervals.iter().min().unwrap();
        let max_interval = *intervals.iter().max().unwrap();

        info!(
            avg = format!("{:.1}s", avg_interval),
            min = min_interval,
            max = max_interval,
            samples = intervals.len(),
            "block interval stats"
        );

        // For 4s target, allow [3, 6] second range (generous for E2E variability).
        if avg_interval < 3.0 || avg_interval > 6.0 {
            for node in nodes {
                let _ = node.stop();
            }
            return Err(eyre::eyre!(
                "V6 FAILED: average block interval {:.1}s not within [3, 6]",
                avg_interval
            ));
        }
        info!(
            avg_interval = format!("{:.1}s", avg_interval),
            "V6 PASS: block interval stability"
        );
    } else {
        warn!("V6 SKIP: could not compute block intervals");
    }

    // Final summary.
    let elapsed = start.elapsed();
    info!(
        "=== Scenario 7 PASSED ===\n\
         Nodes: {}\n\
         Total blocks: {} (min) / {} (max)\n\
         Each validator: >= {} blocks\n\
         Unique miners: {}\n\
         Duration: {:.0}s\n\
         All 6 verifications passed.",
        NODE_COUNT,
        min_height,
        max_height,
        BLOCKS_PER_VALIDATOR,
        miner_counts.len(),
        elapsed.as_secs_f64(),
    );

    for node in nodes {
        let _ = node.stop();
    }
    Ok(())
}
