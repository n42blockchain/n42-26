use alloy_primitives::{Address, B256, U256};
use serde::Deserialize;
use serde_json::{json, Value};
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
    #[allow(dead_code)]
    jsonrpc: String,
    #[allow(dead_code)]
    id: u64,
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

        let resp = self.client
            .post(&self.url)
            .json(&body)
            .send()
            .await?;

        let text = resp.text().await?;
        let rpc_resp: JsonRpcResponse<T> = serde_json::from_str(&text)
            .map_err(|e| eyre::eyre!("failed to parse RPC response: {e}\nraw: {text}"))?;

        if let Some(err) = rpc_resp.error {
            return Err(eyre::eyre!("{err}"));
        }

        rpc_resp.result.ok_or_else(|| eyre::eyre!("null result from {method}"))
    }

    /// Returns the current block number.
    pub async fn block_number(&self) -> eyre::Result<u64> {
        let hex: String = self.call("eth_blockNumber", json!([])).await?;
        Ok(u64::from_str_radix(hex.trim_start_matches("0x"), 16)?)
    }

    /// Returns the balance of an address at the latest block.
    pub async fn get_balance(&self, address: Address) -> eyre::Result<U256> {
        let hex: String = self.call(
            "eth_getBalance",
            json!([format!("{address:?}"), "latest"]),
        ).await?;
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
        let result: Option<TransactionReceipt> = self.call(
            "eth_getTransactionReceipt",
            json!([format!("{tx_hash:?}")]),
        ).await.ok();
        Ok(result)
    }

    /// Waits for a transaction receipt with polling.
    pub async fn wait_for_receipt(
        &self,
        tx_hash: B256,
        timeout: std::time::Duration,
    ) -> eyre::Result<TransactionReceipt> {
        let start = tokio::time::Instant::now();
        let poll = std::time::Duration::from_millis(500);

        loop {
            if start.elapsed() > timeout {
                return Err(eyre::eyre!("timeout waiting for receipt of {tx_hash:?}"));
            }
            if let Ok(Some(receipt)) = self.get_transaction_receipt(tx_hash).await {
                return Ok(receipt);
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

    /// Returns the current nonce for an address.
    pub async fn get_nonce(&self, address: Address) -> eyre::Result<u64> {
        let hex: String = self.call(
            "eth_getTransactionCount",
            json!([format!("{address:?}"), "latest"]),
        ).await?;
        Ok(u64::from_str_radix(hex.trim_start_matches("0x"), 16)?)
    }

    /// Returns the current gas price.
    pub async fn gas_price(&self) -> eyre::Result<u128> {
        let hex: String = self.call("eth_gasPrice", json!([])).await?;
        Ok(u128::from_str_radix(hex.trim_start_matches("0x"), 16)?)
    }

    /// Calls eth_call for read-only contract interaction.
    pub async fn eth_call(
        &self,
        to: Address,
        data: Vec<u8>,
    ) -> eyre::Result<Vec<u8>> {
        let hex_data = format!("0x{}", hex::encode(&data));
        let result: String = self.call(
            "eth_call",
            json!([{
                "to": format!("{to:?}"),
                "data": hex_data,
            }, "latest"]),
        ).await?;
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
        ).await
    }

    /// Returns a block by number.
    pub async fn get_block_by_number(&self, number: u64) -> eyre::Result<Value> {
        let hex_num = format!("0x{number:x}");
        self.call("eth_getBlockByNumber", json!([hex_num, false])).await
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
            params.insert("data".to_string(), json!(format!("0x{}", hex::encode(data))));
        }
        if let Some(value) = value {
            params.insert("value".to_string(), json!(format!("0x{value:x}")));
        }

        let hex: String = self.call("eth_estimateGas", json!([params])).await?;
        Ok(u64::from_str_radix(hex.trim_start_matches("0x"), 16)?)
    }
}
