use alloy_primitives::{Address, B256, U256};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, Ordering};

/// A minimal JSON-RPC client for interacting with N42 nodes.
///
/// Uses raw `reqwest` HTTP calls to avoid alloy-provider version conflicts.
pub struct RpcClient {
    url: String,
    client: reqwest::Client,
    request_id: AtomicU64,
}

#[derive(Debug, Deserialize)]
struct JsonRpcResponse<T> {
    result: Option<T>,
    error: Option<JsonRpcError>,
}

#[derive(Debug, Deserialize)]
struct JsonRpcError {
    code: i64,
    message: String,
}

impl std::fmt::Display for JsonRpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RPC error {}: {}", self.code, self.message)
    }
}

/// Transaction receipt returned by eth_getTransactionReceipt.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct TransactionReceipt {
    pub transaction_hash: B256,
    #[serde(deserialize_with = "deserialize_u64_hex")]
    pub status: u64,
    #[serde(deserialize_with = "deserialize_u64_hex")]
    pub block_number: u64,
    pub block_hash: B256,
    #[serde(deserialize_with = "deserialize_u64_hex")]
    pub gas_used: u64,
    #[serde(deserialize_with = "deserialize_u64_hex")]
    pub cumulative_gas_used: u64,
    pub contract_address: Option<Address>,
}

/// Consensus status response from n42_consensusStatus.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct ConsensusStatus {
    pub latest_committed_view: Option<u64>,
    pub latest_committed_block_hash: Option<String>,
    pub validator_count: u32,
    pub has_committed_qc: bool,
}

/// Attestation response from n42_submitAttestation.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttestationResponse {
    pub accepted: bool,
    pub attestation_count: u32,
    pub threshold_reached: bool,
}

fn deserialize_u64_hex<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s: String = Deserialize::deserialize(deserializer)?;
    u64::from_str_radix(s.trim_start_matches("0x"), 16).map_err(serde::de::Error::custom)
}

impl RpcClient {
    pub fn new(url: String) -> Self {
        Self {
            url,
            client: reqwest::Client::new(),
            request_id: AtomicU64::new(1),
        }
    }

    fn next_id(&self) -> u64 {
        self.request_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Sends a raw JSON-RPC request.
    async fn call<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: Value,
    ) -> eyre::Result<T> {
        let id = self.next_id();
        let body = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
            "id": id,
        });

        let resp = self.client.post(&self.url).json(&body).send().await?;

        let text = resp.text().await?;
        let rpc_resp: JsonRpcResponse<T> = serde_json::from_str(&text)
            .map_err(|e| eyre::eyre!("failed to parse RPC response: {e}\nraw: {text}"))?;

        if let Some(err) = rpc_resp.error {
            return Err(eyre::eyre!("{err}"));
        }

        rpc_resp
            .result
            .ok_or_else(|| eyre::eyre!("null result from {method}"))
    }

    async fn call_optional<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: Value,
    ) -> eyre::Result<Option<T>> {
        let id = self.next_id();
        let body = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
            "id": id,
        });

        let resp = self.client.post(&self.url).json(&body).send().await?;
        let text = resp.text().await?;
        let rpc_resp: JsonRpcResponse<T> = serde_json::from_str(&text)
            .map_err(|e| eyre::eyre!("failed to parse RPC response: {e}\nraw: {text}"))?;

        if let Some(err) = rpc_resp.error {
            return Err(eyre::eyre!("{err}"));
        }

        Ok(rpc_resp.result)
    }

    /// Returns the current block number.
    pub async fn block_number(&self) -> eyre::Result<u64> {
        let hex: String = self.call("eth_blockNumber", json!([])).await?;
        Ok(u64::from_str_radix(hex.trim_start_matches("0x"), 16)?)
    }

    /// Returns the balance of an address at the latest block.
    pub async fn get_balance(&self, address: Address) -> eyre::Result<U256> {
        let hex: String = self
            .call("eth_getBalance", json!([format!("{address:?}"), "latest"]))
            .await?;
        Ok(U256::from_str_radix(hex.trim_start_matches("0x"), 16)?)
    }

    /// Sends a raw signed transaction and returns the tx hash.
    pub async fn send_raw_transaction(&self, raw_tx: &[u8]) -> eyre::Result<B256> {
        let hex_tx = format!("0x{}", hex::encode(raw_tx));
        let hash: B256 = self.call("eth_sendRawTransaction", json!([hex_tx])).await?;
        Ok(hash)
    }

    /// Returns the transaction receipt for a given tx hash.
    pub async fn get_transaction_receipt(
        &self,
        tx_hash: B256,
    ) -> eyre::Result<Option<TransactionReceipt>> {
        self.call_optional("eth_getTransactionReceipt", json!([format!("{tx_hash:?}")]))
            .await
    }

    /// Waits for a transaction receipt with polling.
    pub async fn wait_for_receipt(
        &self,
        tx_hash: B256,
        timeout: std::time::Duration,
    ) -> eyre::Result<TransactionReceipt> {
        let start = tokio::time::Instant::now();
        let poll = std::time::Duration::from_millis(500);
        let mut last_rpc_error = None;

        loop {
            if start.elapsed() > timeout {
                if let Some(last_rpc_error) = last_rpc_error {
                    return Err(eyre::eyre!(
                        "timeout waiting for receipt of {tx_hash:?}; last_rpc_error={last_rpc_error}"
                    ));
                }
                return Err(eyre::eyre!("timeout waiting for receipt of {tx_hash:?}"));
            }
            match self.get_transaction_receipt(tx_hash).await {
                Ok(Some(receipt)) => return Ok(receipt),
                Ok(None) => {}
                Err(err) => last_rpc_error = Some(format!("{err:#}")),
            }
            tokio::time::sleep(poll).await;
        }
    }

    /// Returns the chain ID.
    #[allow(dead_code)]
    pub async fn chain_id(&self) -> eyre::Result<u64> {
        let hex: String = self.call("eth_chainId", json!([])).await?;
        Ok(u64::from_str_radix(hex.trim_start_matches("0x"), 16)?)
    }

    /// Returns the next usable nonce for an address, including transactions
    /// already accepted into the local pool.
    ///
    /// E2E senders resynchronize after high-volume phases while the pool may
    /// still contain unmined transactions. Querying `latest` there reuses the
    /// first pending nonce and produces a replacement-underpriced failure.
    pub async fn get_nonce(&self, address: Address) -> eyre::Result<u64> {
        let hex: String = self
            .call(
                "eth_getTransactionCount",
                json!([format!("{address:?}"), "pending"]),
            )
            .await?;
        Ok(u64::from_str_radix(hex.trim_start_matches("0x"), 16)?)
    }

    /// Returns the current gas price.
    pub async fn gas_price(&self) -> eyre::Result<u128> {
        let hex: String = self.call("eth_gasPrice", json!([])).await?;
        Ok(u128::from_str_radix(hex.trim_start_matches("0x"), 16)?)
    }

    /// Calls eth_call for read-only contract interaction.
    pub async fn eth_call(&self, to: Address, data: Vec<u8>) -> eyre::Result<Vec<u8>> {
        let hex_data = format!("0x{}", hex::encode(&data));
        let result: String = self
            .call(
                "eth_call",
                json!([{
                "to": format!("{to:?}"),
                "data": hex_data,
            }, "latest"]),
            )
            .await?;
        let bytes = hex::decode(result.trim_start_matches("0x"))?;
        Ok(bytes)
    }

    /// Queries n42_consensusStatus.
    pub async fn consensus_status(&self) -> eyre::Result<ConsensusStatus> {
        self.call("n42_consensusStatus", json!([])).await
    }

    /// Submits a BLS attestation via n42_submitAttestation.
    pub async fn submit_attestation(
        &self,
        pubkey: &str,
        signature: &str,
        block_hash: B256,
        slot: u64,
    ) -> eyre::Result<AttestationResponse> {
        self.call(
            "n42_submitAttestation",
            json!([pubkey, signature, format!("{block_hash:?}"), slot]),
        )
        .await
    }

    /// Returns a block by number.
    pub async fn get_block_by_number(&self, number: u64) -> eyre::Result<Value> {
        let hex_num = format!("0x{number:x}");
        self.call("eth_getBlockByNumber", json!([hex_num, false]))
            .await
    }

    /// Estimates gas for a transaction.
    #[allow(dead_code)]
    pub async fn estimate_gas(
        &self,
        from: Address,
        to: Option<Address>,
        data: Option<Vec<u8>>,
        value: Option<U256>,
    ) -> eyre::Result<u64> {
        let mut params = serde_json::Map::new();
        params.insert("from".to_string(), json!(format!("{from:?}")));
        if let Some(to) = to {
            params.insert("to".to_string(), json!(format!("{to:?}")));
        }
        if let Some(data) = data {
            params.insert(
                "data".to_string(),
                json!(format!("0x{}", hex::encode(data))),
            );
        }
        if let Some(value) = value {
            params.insert("value".to_string(), json!(format!("0x{value:x}")));
        }

        let hex: String = self.call("eth_estimateGas", json!([params])).await?;
        Ok(u64::from_str_radix(hex.trim_start_matches("0x"), 16)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn spawn_rpc_server(responses: Vec<String>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            for body in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = [0u8; 4096];
                let _ = socket.read(&mut buf).await.unwrap();
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });

        format!("http://{addr}")
    }

    fn receipt_response_body() -> String {
        format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{\"transactionHash\":\"0x{}\",\"status\":\"0x1\",\"blockNumber\":\"0x2\",\"blockHash\":\"0x{}\",\"gasUsed\":\"0x5208\",\"cumulativeGasUsed\":\"0x5208\",\"contractAddress\":null}}}}",
            "11".repeat(32),
            "22".repeat(32)
        )
    }

    #[tokio::test]
    async fn test_get_transaction_receipt_none_for_null_result() {
        let rpc = RpcClient::new(
            spawn_rpc_server(vec![
                "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":null}".to_string(),
            ])
            .await,
        );

        let receipt = rpc
            .get_transaction_receipt(B256::repeat_byte(0x11))
            .await
            .unwrap();
        assert!(receipt.is_none());
    }

    #[tokio::test]
    async fn test_get_transaction_receipt_propagates_rpc_error() {
        let rpc = RpcClient::new(
            spawn_rpc_server(vec![
                "{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32000,\"message\":\"boom\"}}"
                    .to_string(),
            ])
            .await,
        );

        let err = rpc
            .get_transaction_receipt(B256::repeat_byte(0x11))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("boom"));
    }

    #[tokio::test]
    async fn test_wait_for_receipt_retries_until_result_available() {
        let rpc = RpcClient::new(
            spawn_rpc_server(vec![
                "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":null}".to_string(),
                receipt_response_body(),
            ])
            .await,
        );

        let receipt = rpc
            .wait_for_receipt(B256::repeat_byte(0x11), std::time::Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(receipt.block_number, 2);
        assert_eq!(receipt.status, 1);
    }
}
