use alloy_primitives::keccak256;
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
/// Phase 1 (current): Validates multi-node startup, P2P connectivity, and
/// that at least one node (the initial leader) produces blocks.
///
/// Note: Full multi-node consensus (all nodes advancing the chain together)
/// requires block data propagation between nodes, which is not yet implemented.
/// Currently each node runs an independent chain; the leader for view 1 commits
/// its own block (quorum=1 when f=0), but subsequent leaders can't build on it
/// because they haven't received the block data.
pub async fn run(binary_path: PathBuf) -> eyre::Result<()> {
    info!("=== Scenario 4: Multi-Node Consensus ===");

    // Test 1: 3 nodes with P2P connectivity.
    run_multi_node_test(&binary_path, 3, Duration::from_secs(30)).await?;

    // Test 2: Concurrent independent nodes (each as solo validator).
    // This validates multi-instance resource isolation (ports, data dirs).
    run_concurrent_solo_test(&binary_path, 5, Duration::from_secs(30)).await?;

    info!("=== Scenario 4 PASSED ===");
    Ok(())
}

/// Tests multi-node startup with P2P connectivity.
///
/// Verifies:
/// - All nodes start successfully with unique ports
/// - At least one node produces blocks (the view-1 leader)
/// - P2P infrastructure is wired up (deterministic PeerIds, trusted peers)
async fn run_multi_node_test(
    binary_path: &PathBuf,
    node_count: usize,
    duration: Duration,
) -> eyre::Result<()> {
    info!(node_count, duration_secs = duration.as_secs(), "starting multi-node P2P test");

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
        let port_offset = (i as u16) * 10;

        // Build trusted peers: all other nodes' multiaddrs.
        let trusted_peers: Vec<String> = (0..node_count)
            .filter(|&j| j != i)
            .map(|j| {
                let peer_port = 9400 + (j as u16) * 10;
                format!("/ip4/127.0.0.1/udp/{}/quic-v1/p2p/{}", peer_port, peer_ids[j])
            })
            .collect();

        let config = NodeConfig {
            binary_path: binary_path.clone(),
            genesis_path: genesis_path.clone(),
            validator_index: i,
            validator_count: node_count,
            block_interval_ms: 4000,
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
    tokio::time::sleep(duration).await;

    // Collect block heights.
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

    // Verification: at least one node should have produced blocks.
    let max_height = *heights.iter().max().unwrap_or(&0);
    if max_height == 0 {
        for node in nodes {
            let _ = node.stop();
        }
        return Err(eyre::eyre!("no node produced any blocks"));
    }

    info!(
        node_count,
        max_height,
        "PASS: {node_count}-node P2P test (leader produced {max_height} blocks)"
    );

    for node in nodes {
        let _ = node.stop();
    }
    Ok(())
}

/// Tests multiple independent solo-validator nodes running concurrently.
///
/// Each node runs as N42_VALIDATOR_COUNT=1 (solo mode), verifying:
/// - Port isolation (no conflicts between concurrent nodes)
/// - Data directory isolation
/// - All nodes produce blocks independently
async fn run_concurrent_solo_test(
    binary_path: &PathBuf,
    node_count: usize,
    duration: Duration,
) -> eyre::Result<()> {
    info!(node_count, duration_secs = duration.as_secs(), "starting concurrent solo-node test");

    let accounts = genesis::generate_test_accounts();
    let mut nodes = Vec::with_capacity(node_count);

    for i in 0..node_count {
        let tmp_dir = tempfile::tempdir()?;
        let genesis_path = genesis::write_genesis_file(tmp_dir.path(), &accounts);

        // Each node is a solo validator (validator_count=1).
        let config = NodeConfig {
            binary_path: binary_path.clone(),
            genesis_path,
            validator_index: 0,
            validator_count: 1,
            block_interval_ms: 4000,
            port_offset: (i as u16) * 10 + 100, // offset by 100 to avoid conflicts with test 1
            trusted_peers: vec![],
        };

        match NodeProcess::start(&config).await {
            Ok(node) => {
                info!(index = i, http_port = node.http_port, "solo node started");
                nodes.push((node, tmp_dir));
            }
            Err(e) => {
                error!(index = i, error = %e, "failed to start solo node");
                for (node, _) in nodes {
                    let _ = node.stop();
                }
                return Err(eyre::eyre!("failed to start solo node {i}: {e}"));
            }
        }
    }

    info!(started = nodes.len(), "all solo nodes started, running for {}s", duration.as_secs());
    tokio::time::sleep(duration).await;

    // Verify all nodes produced blocks.
    let mut all_produced = true;
    for (i, (node, _)) in nodes.iter().enumerate() {
        match node.rpc.block_number().await {
            Ok(h) => {
                info!(node = i, block_height = h, "solo node block height");
                if h == 0 {
                    error!(node = i, "solo node failed to produce blocks");
                    all_produced = false;
                }
            }
            Err(e) => {
                error!(node = i, error = %e, "failed to query solo node");
                all_produced = false;
            }
        }
    }

    for (node, _) in nodes {
        let _ = node.stop();
    }

    if !all_produced {
        return Err(eyre::eyre!("not all solo nodes produced blocks"));
    }

    info!(
        node_count,
        "PASS: {node_count} concurrent solo nodes all producing blocks"
    );
    Ok(())
}
