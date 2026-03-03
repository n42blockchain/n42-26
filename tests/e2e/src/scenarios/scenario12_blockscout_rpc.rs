use std::path::PathBuf;
use std::time::Duration;
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::genesis;
use crate::node_manager::{NodeConfig, NodeProcess};

/// Scenario 12: Blockscout RPC Compatibility
///
/// Verifies that a single N42 node exposes all RPC methods required by
/// Blockscout block explorer. Ported from scripts/verify-blockscout-rpc.sh.
///
/// Categories:
/// - Required: must pass (eth_*, debug_*, net_*, web3_*)
/// - Optional: warn-only (trace_*, txpool_*)
/// - N42 custom: warn-only (n42_*)
pub async fn run(binary_path: PathBuf) -> eyre::Result<()> {
    info!("=== Scenario 12: Blockscout RPC Compatibility ===");

    let accounts = genesis::generate_test_accounts();
    let tmp_dir = tempfile::tempdir()?;
    let genesis_path = genesis::write_genesis_file(tmp_dir.path(), &accounts);

    let config = NodeConfig::single_node(binary_path, genesis_path, 8000);
    let node = NodeProcess::start(&config).await?;
    let rpc = &node.rpc;

    info!("Node started on http://127.0.0.1:{}", node.http_port);

    // Wait for a few blocks so we have data to query
    info!("Waiting for blocks to be produced...");
    let mut retries = 0;
    loop {
        if retries > 30 {
            let _ = node.stop();
            return Err(eyre::eyre!("Timeout waiting for blocks"));
        }
        if let Ok(h) = rpc.block_number().await {
            if h >= 2 {
                info!(block_height = h, "Blocks available for testing");
                break;
            }
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
        retries += 1;
    }

    let url = node.http_url();
    let client = reqwest::Client::new();

    let mut pass = 0u32;
    let mut fail = 0u32;
    let mut warn_count = 0u32;

    // Helper to make raw JSON-RPC calls and check for errors
    async fn check_rpc(
        client: &reqwest::Client,
        url: &str,
        name: &str,
        method: &str,
        params: Value,
        required: bool,
        pass: &mut u32,
        fail: &mut u32,
        warn_count: &mut u32,
    ) {
        let body = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
            "id": 1,
        });

        match client.post(url).json(&body).send().await {
            Ok(resp) => {
                match resp.text().await {
                    Ok(text) => {
                        if let Ok(parsed) = serde_json::from_str::<Value>(&text) {
                            if parsed.get("error").is_some() {
                                let code = parsed["error"]["code"].as_i64().unwrap_or(0);
                                if code == -32601 {
                                    // Method not found
                                    if required {
                                        tracing::error!("  FAIL {name} ({method}) - method not found");
                                        *fail += 1;
                                    } else {
                                        warn!("  WARN {name} ({method}) - method not found (optional)");
                                        *warn_count += 1;
                                    }
                                } else {
                                    // Other error (e.g. invalid params) means the method exists
                                    info!("  PASS {name} ({method}) - method available (error code={code})");
                                    *pass += 1;
                                }
                            } else if parsed.get("result").is_some() {
                                info!("  PASS {name} ({method}) - OK");
                                *pass += 1;
                            } else {
                                if required {
                                    tracing::error!("  FAIL {name} ({method}) - unexpected response");
                                    *fail += 1;
                                } else {
                                    warn!("  WARN {name} ({method}) - unexpected response");
                                    *warn_count += 1;
                                }
                            }
                        } else {
                            if required {
                                tracing::error!("  FAIL {name} ({method}) - invalid JSON response");
                                *fail += 1;
                            } else {
                                warn!("  WARN {name} ({method}) - invalid JSON response");
                                *warn_count += 1;
                            }
                        }
                    }
                    Err(e) => {
                        if required {
                            tracing::error!("  FAIL {name} ({method}) - read error: {e}");
                            *fail += 1;
                        } else {
                            warn!("  WARN {name} ({method}) - read error: {e}");
                            *warn_count += 1;
                        }
                    }
                }
            }
            Err(e) => {
                if required {
                    tracing::error!("  FAIL {name} ({method}) - connection failed: {e}");
                    *fail += 1;
                } else {
                    warn!("  WARN {name} ({method}) - connection failed: {e}");
                    *warn_count += 1;
                }
            }
        }
    }

    let zero_hash = "0x0000000000000000000000000000000000000000000000000000000000000000";
    let zero_addr = "0x0000000000000000000000000000000000000000";

    // --- Core eth_* methods (required) ---
    info!("--- Core eth_* methods (required) ---");

    check_rpc(&client, &url, "Block number", "eth_blockNumber",
        json!([]), true, &mut pass, &mut fail, &mut warn_count).await;

    check_rpc(&client, &url, "Chain ID", "eth_chainId",
        json!([]), true, &mut pass, &mut fail, &mut warn_count).await;

    check_rpc(&client, &url, "Get block by number", "eth_getBlockByNumber",
        json!(["0x1", true]), true, &mut pass, &mut fail, &mut warn_count).await;

    // Use block 1's hash for getBlockByHash test
    let block1_hash = {
        let body = json!({"jsonrpc":"2.0","method":"eth_getBlockByNumber","params":["0x1",false],"id":99});
        if let Ok(resp) = client.post(&url).json(&body).send().await {
            if let Ok(text) = resp.text().await {
                serde_json::from_str::<Value>(&text).ok()
                    .and_then(|v| v["result"]["hash"].as_str().map(String::from))
                    .unwrap_or_else(|| zero_hash.to_string())
            } else {
                zero_hash.to_string()
            }
        } else {
            zero_hash.to_string()
        }
    };

    check_rpc(&client, &url, "Get block by hash", "eth_getBlockByHash",
        json!([block1_hash, true]), true, &mut pass, &mut fail, &mut warn_count).await;

    check_rpc(&client, &url, "Get tx receipt", "eth_getTransactionReceipt",
        json!([zero_hash]), true, &mut pass, &mut fail, &mut warn_count).await;

    check_rpc(&client, &url, "Get logs", "eth_getLogs",
        json!([{"fromBlock": "0x0", "toBlock": "0x1"}]), true, &mut pass, &mut fail, &mut warn_count).await;

    check_rpc(&client, &url, "Get balance", "eth_getBalance",
        json!([zero_addr, "latest"]), true, &mut pass, &mut fail, &mut warn_count).await;

    check_rpc(&client, &url, "Get code", "eth_getCode",
        json!([zero_addr, "latest"]), true, &mut pass, &mut fail, &mut warn_count).await;

    check_rpc(&client, &url, "Gas price", "eth_gasPrice",
        json!([]), true, &mut pass, &mut fail, &mut warn_count).await;

    // --- Debug methods (required for Blockscout internal txs) ---
    info!("--- Debug methods (required) ---");

    check_rpc(&client, &url, "Trace transaction", "debug_traceTransaction",
        json!([zero_hash, {"tracer": "callTracer"}]), true, &mut pass, &mut fail, &mut warn_count).await;

    check_rpc(&client, &url, "Trace block by number", "debug_traceBlockByNumber",
        json!(["0x1", {"tracer": "callTracer"}]), true, &mut pass, &mut fail, &mut warn_count).await;

    // --- Net/Web3 methods (required) ---
    info!("--- Net/Web3 methods (required) ---");

    check_rpc(&client, &url, "Net version", "net_version",
        json!([]), true, &mut pass, &mut fail, &mut warn_count).await;

    check_rpc(&client, &url, "Web3 client version", "web3_clientVersion",
        json!([]), true, &mut pass, &mut fail, &mut warn_count).await;

    // --- Trace methods (optional, warn-only) ---
    info!("--- Trace methods (optional) ---");

    check_rpc(&client, &url, "Trace block", "trace_block",
        json!(["0x1"]), false, &mut pass, &mut fail, &mut warn_count).await;

    check_rpc(&client, &url, "Trace replay block", "trace_replayBlockTransactions",
        json!(["0x1", ["trace"]]), false, &mut pass, &mut fail, &mut warn_count).await;

    // --- N42 custom methods ---
    info!("--- N42 custom methods ---");

    check_rpc(&client, &url, "Consensus status", "n42_consensusStatus",
        json!([]), false, &mut pass, &mut fail, &mut warn_count).await;

    check_rpc(&client, &url, "Validator set", "n42_validatorSet",
        json!([]), false, &mut pass, &mut fail, &mut warn_count).await;

    // --- Summary ---
    let total = pass + fail + warn_count;
    info!(pass, fail, warn_count, total, "Blockscout RPC compatibility results");

    let _ = node.stop();

    if fail > 0 {
        return Err(eyre::eyre!(
            "Scenario 12 FAILED: {fail} required RPC methods missing ({pass} passed, {warn_count} warnings)"
        ));
    }

    info!("=== Scenario 12 PASSED ({pass} passed, {warn_count} warnings) ===");
    Ok(())
}
