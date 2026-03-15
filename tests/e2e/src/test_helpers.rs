use n42_network::PeerId;
use std::time::Duration;

use crate::node_manager::NodeProcess;
use crate::rpc_client::RpcClient;

pub async fn get_height_safe(rpc: &RpcClient) -> u64 {
    rpc.block_number().await.unwrap_or(0)
}

pub async fn wait_for_sync(rpc: &RpcClient, target: u64, timeout: Duration) -> eyre::Result<()> {
    let start = tokio::time::Instant::now();
    let poll = Duration::from_millis(500);
    let target_min = target.saturating_sub(1);

    loop {
        if start.elapsed() > timeout {
            let current = get_height_safe(rpc).await;
            return Err(eyre::eyre!(
                "sync timeout: current={current}, target={target}"
            ));
        }
        let h = get_height_safe(rpc).await;
        if h >= target_min {
            return Ok(());
        }
        tokio::time::sleep(poll).await;
    }
}

pub async fn wait_for_height_increase(
    rpc: &RpcClient,
    baseline: u64,
    min_increase: u64,
    timeout: Duration,
) -> u64 {
    let start = tokio::time::Instant::now();
    let poll = Duration::from_millis(500);
    loop {
        let h = get_height_safe(rpc).await;
        if h >= baseline + min_increase {
            return h;
        }
        if start.elapsed() > timeout {
            return h;
        }
        tokio::time::sleep(poll).await;
    }
}

pub fn compute_peer_id(validator_index: usize) -> PeerId {
    n42_network::deterministic_validator_peer_id(validator_index as u32)
        .expect("deterministic validator peer id")
}

pub fn cleanup_nodes(nodes: &mut Vec<Option<NodeProcess>>) {
    for node in nodes.drain(..).flatten() {
        let _ = node.stop();
    }
}

pub fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}
