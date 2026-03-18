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
    let mut last_rpc_error = None;

    loop {
        if start.elapsed() > timeout {
            return match rpc.block_number().await {
                Ok(current) => {
                    if let Some(last_rpc_error) = last_rpc_error {
                        Err(eyre::eyre!(
                            "sync timeout: current={current}, target={target}, last_rpc_error={last_rpc_error}"
                        ))
                    } else {
                        Err(eyre::eyre!(
                            "sync timeout: current={current}, target={target}"
                        ))
                    }
                }
                Err(err) => {
                    let last_rpc_error = last_rpc_error.unwrap_or_else(|| format!("{err:#}"));
                    Err(eyre::eyre!(
                        "sync timeout: current=unknown, target={target}, last_rpc_error={last_rpc_error}"
                    ))
                }
            };
        }
        match rpc.block_number().await {
            Ok(h) => {
                if h >= target_min {
                    return Ok(());
                }
            }
            Err(err) => last_rpc_error = Some(format!("{err:#}")),
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
    let mut last_height = baseline;
    loop {
        if let Ok(h) = rpc.block_number().await {
            last_height = h;
            if h >= baseline + min_increase {
                return h;
            }
        }
        if start.elapsed() > timeout {
            return last_height;
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
