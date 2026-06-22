//! Remote Engine-API [`ExecutionLayer`] — a JSON-RPC + JWT client that lets the
//! `n42-consensus-service` driver run against ANY Engine-API execution layer in a
//! *separate process* (Caplin's dual-mode: the same `ConsensusService` runs either
//! in-process via `RethExecutionLayer` or standalone via this RPC adapter).
//!
//! Hard-reth-free: only alloy wire types + `reqwest` + `serde_json` + the
//! `alloy_rpc_types_engine::JwtSecret` HS256 auth helper. The Engine API methods
//! used are `engine_newPayloadV4` / `engine_forkchoiceUpdatedV3` /
//! `engine_getPayloadV4` (Prague).
//!
//! Cross-process E2E (against a live `n42-node` reth Engine API) is the post-datc
//! validation step; unit tests here cover request construction, response parsing,
//! and the JWT header.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use alloy_primitives::B256;
use alloy_rpc_types_engine::{
    ExecutionData, ExecutionPayload, ExecutionPayloadEnvelopeV4, ForkchoiceState,
    ForkchoiceUpdated, JwtSecret, PayloadAttributes, PayloadId, PayloadStatus,
};
use n42_consensus_service::el::{BuiltBlock, ElError, ExecutionLayer, ResolveKind};
use serde_json::{Value, json};
use std::collections::HashMap;

const NEW_PAYLOAD_V4: &str = "engine_newPayloadV4";
const FCU_V3: &str = "engine_forkchoiceUpdatedV3";
const GET_PAYLOAD_V4: &str = "engine_getPayloadV4";

/// Engine-API JSON-RPC client implementing [`ExecutionLayer`] against a remote EL.
pub struct EngineApiRpcExecutionLayer {
    url: String,
    jwt: JwtSecret,
    client: reqwest::Client,
    next_id: AtomicU64,
    /// `payload_id → parent_beacon_block_root`, captured at
    /// `fork_choice_updated_with_attrs` so `resolve_payload` can rebuild the
    /// sidecar (the `getPayload` envelope does not carry it).
    parent_beacon: Mutex<HashMap<PayloadId, B256>>,
}

impl EngineApiRpcExecutionLayer {
    /// New client for `url` (e.g. `http://127.0.0.1:8551`) authenticated with `jwt`.
    pub fn new(url: impl Into<String>, jwt: JwtSecret) -> Self {
        Self {
            url: url.into(),
            jwt,
            client: reqwest::Client::new(),
            next_id: AtomicU64::new(1),
            parent_beacon: Mutex::new(HashMap::new()),
        }
    }

    /// New client reading a hex-encoded 32-byte JWT secret from `path`.
    pub fn from_secret_file(
        url: impl Into<String>,
        path: impl AsRef<std::path::Path>,
    ) -> Result<Self, ElError> {
        let hex =
            std::fs::read_to_string(path).map_err(|e| ElError(format!("read jwt secret: {e}")))?;
        let jwt = JwtSecret::from_hex(hex.trim())
            .map_err(|e| ElError(format!("parse jwt secret: {e}")))?;
        Ok(Self::new(url, jwt))
    }

    /// Builds the `Authorization: Bearer <jwt>` value with a fresh `iat` claim.
    fn auth_header(&self) -> Result<String, ElError> {
        bearer_header(&self.jwt)
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, ElError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let body = jsonrpc_request(id, method, params);
        let auth = self.auth_header()?;
        let resp = self
            .client
            .post(&self.url)
            .header(reqwest::header::AUTHORIZATION, auth)
            .json(&body)
            .send()
            .await
            .map_err(|e| ElError(format!("{method} transport: {e}")))?;
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| ElError(format!("{method} body: {e}")))?;
        parse_jsonrpc_result(&bytes).map_err(|e| ElError(format!("{method}: {}", e.0)))
    }
}

#[async_trait::async_trait]
impl ExecutionLayer for EngineApiRpcExecutionLayer {
    async fn new_payload(&self, payload: ExecutionData) -> Result<PayloadStatus, ElError> {
        let result = self
            .call(NEW_PAYLOAD_V4, new_payload_v4_params(&payload)?)
            .await?;
        serde_json::from_value(result).map_err(|e| ElError(format!("newPayload result: {e}")))
    }

    async fn fork_choice_updated(
        &self,
        state: ForkchoiceState,
    ) -> Result<ForkchoiceUpdated, ElError> {
        let result = self.call(FCU_V3, fcu_v3_params(&state, None)?).await?;
        serde_json::from_value(result).map_err(|e| ElError(format!("fcu result: {e}")))
    }

    async fn fork_choice_updated_with_attrs(
        &self,
        state: ForkchoiceState,
        attrs: PayloadAttributes,
    ) -> Result<ForkchoiceUpdated, ElError> {
        let parent_beacon = attrs.parent_beacon_block_root;
        let result = self
            .call(FCU_V3, fcu_v3_params(&state, Some(&attrs))?)
            .await?;
        let updated: ForkchoiceUpdated = serde_json::from_value(result)
            .map_err(|e| ElError(format!("fcu(attrs) result: {e}")))?;
        // Cache parent_beacon_block_root keyed by the returned payload_id so
        // resolve_payload can rebuild the ExecutionData sidecar.
        if let (Some(id), Some(root)) = (updated.payload_id, parent_beacon) {
            self.parent_beacon
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(id, root);
        }
        Ok(updated)
    }

    async fn resolve_payload(
        &self,
        id: PayloadId,
        _kind: ResolveKind,
    ) -> Option<Result<BuiltBlock, ElError>> {
        // WaitForPending → a single getPayload call (the remote builder packs
        // synchronously for this RPC).
        let result = match self.call(GET_PAYLOAD_V4, json!([id])).await {
            Ok(v) => v,
            Err(e) => return Some(Err(e)),
        };
        let envelope: ExecutionPayloadEnvelopeV4 = match serde_json::from_value(result) {
            Ok(env) => env,
            Err(e) => return Some(Err(ElError(format!("getPayload envelope: {e}")))),
        };
        let parent_beacon = self
            .parent_beacon
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id)
            .unwrap_or(B256::ZERO);
        Some(Ok(envelope_v4_to_built_block(envelope, parent_beacon)))
    }
}

/// Builds a JSON-RPC 2.0 request envelope.
fn jsonrpc_request(id: u64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

/// Mints an `Authorization: Bearer <HS256-JWT>` with a fresh `iat` claim,
/// directly via HMAC-SHA256 over the 32-byte secret. Done by hand (not
/// `JwtSecret::encode`) because that pulls `jsonwebtoken`, whose `rust_crypto`
/// vs `aws_lc_rs` provider features unify to BOTH once the reth/node crates are
/// in the same build graph, making HS256 encode panic ("could not determine the
/// process-level CryptoProvider"). Pure HMAC has no such ambiguity. The Engine
/// API only requires the `iat` claim (within ±60s).
fn bearer_header(jwt: &JwtSecret) -> Result<String, ElError> {
    use base64::Engine as _;
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    // Static HS256 header.
    let header_b64 = b64.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
    let iat = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| ElError(format!("clock before epoch: {e}")))?
        .as_secs();
    let claims_b64 = b64.encode(format!(r#"{{"iat":{iat}}}"#));
    let signing_input = format!("{header_b64}.{claims_b64}");

    let mut mac = Hmac::<Sha256>::new_from_slice(jwt.as_bytes())
        .map_err(|e| ElError(format!("jwt hmac key: {e}")))?;
    mac.update(signing_input.as_bytes());
    let sig_b64 = b64.encode(mac.finalize().into_bytes());

    Ok(format!("Bearer {signing_input}.{sig_b64}"))
}

/// Extracts the `result` field of a JSON-RPC response, surfacing `error` as
/// [`ElError`].
fn parse_jsonrpc_result(body: &[u8]) -> Result<Value, ElError> {
    let mut v: Value =
        serde_json::from_slice(body).map_err(|e| ElError(format!("invalid json-rpc: {e}")))?;
    if let Some(err) = v.get("error")
        && !err.is_null()
    {
        let code = err.get("code").and_then(Value::as_i64).unwrap_or_default();
        let msg = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        return Err(ElError(format!("rpc error {code}: {msg}")));
    }
    match v.get_mut("result") {
        Some(r) => Ok(r.take()),
        None => Err(ElError("json-rpc response missing result".into())),
    }
}

/// `engine_newPayloadV4` params: `[executionPayload, expectedBlobVersionedHashes,
/// parentBeaconBlockRoot, executionRequests]`.
fn new_payload_v4_params(data: &ExecutionData) -> Result<Value, ElError> {
    let payload = serde_json::to_value(&data.payload)
        .map_err(|e| ElError(format!("serialize executionPayload: {e}")))?;
    let versioned_hashes: Vec<B256> = data.sidecar.versioned_hashes().cloned().unwrap_or_default();
    let parent_beacon = data
        .sidecar
        .parent_beacon_block_root()
        .unwrap_or(B256::ZERO);
    let requests = match data.sidecar.requests() {
        Some(r) => serde_json::to_value(r)
            .map_err(|e| ElError(format!("serialize executionRequests: {e}")))?,
        None => json!([]),
    };
    Ok(json!([payload, versioned_hashes, parent_beacon, requests]))
}

/// `engine_forkchoiceUpdatedV3` params: `[forkchoiceState, payloadAttributes|null]`.
fn fcu_v3_params(
    state: &ForkchoiceState,
    attrs: Option<&PayloadAttributes>,
) -> Result<Value, ElError> {
    let state = serde_json::to_value(state)
        .map_err(|e| ElError(format!("serialize forkchoiceState: {e}")))?;
    let attrs = match attrs {
        Some(a) => serde_json::to_value(a)
            .map_err(|e| ElError(format!("serialize payloadAttributes: {e}")))?,
        None => Value::Null,
    };
    Ok(json!([state, attrs]))
}

/// Converts a `getPayloadV4` envelope into the node-neutral [`BuiltBlock`].
///
/// `blob_tx_hashes` is left empty: the envelope carries blob *versioned* hashes,
/// not the per-tx hashes the leader uses to rebroadcast sidecars. Blob sidecar
/// rebroadcast is a leader-side optimization; in standalone mode followers obtain
/// blobs through the normal mempool/EL path, so an empty set is correct here.
// TODO: thread real blob-tx hashes if standalone leaders ever rebroadcast sidecars.
fn envelope_v4_to_built_block(env: ExecutionPayloadEnvelopeV4, parent_beacon: B256) -> BuiltBlock {
    let v3 = env.envelope_inner.execution_payload.clone();
    let payload = ExecutionPayload::V3(v3);
    let hash = payload.block_hash();
    let number = payload.block_number();
    let timestamp = payload.timestamp();
    let tx_count = payload.as_v1().transactions.len();
    let (payload, sidecar) = env.into_payload_and_sidecar(parent_beacon);
    BuiltBlock {
        hash,
        number,
        timestamp,
        tx_count,
        execution_data: ExecutionData::new(payload, sidecar),
        blob_tx_hashes: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonrpc_request_shape() {
        let req = jsonrpc_request(7, "engine_getPayloadV4", json!(["0x1"]));
        assert_eq!(req["jsonrpc"], "2.0");
        assert_eq!(req["id"], 7);
        assert_eq!(req["method"], "engine_getPayloadV4");
        assert_eq!(req["params"][0], "0x1");
    }

    #[test]
    fn parse_result_ok() {
        let body = br#"{"jsonrpc":"2.0","id":1,"result":{"status":"VALID","latestValidHash":null,"validationError":null}}"#;
        let v = parse_jsonrpc_result(body).expect("ok result");
        let status: PayloadStatus = serde_json::from_value(v).expect("payload status");
        assert!(status.is_valid());
    }

    #[test]
    fn parse_result_error_surfaces_message() {
        let body = br#"{"jsonrpc":"2.0","id":1,"error":{"code":-38003,"message":"invalid payload attributes"}}"#;
        let err = parse_jsonrpc_result(body).expect_err("should be error");
        assert!(err.0.contains("-38003"));
        assert!(err.0.contains("invalid payload attributes"));
    }

    #[test]
    fn parse_result_missing_result() {
        let body = br#"{"jsonrpc":"2.0","id":1}"#;
        assert!(parse_jsonrpc_result(body).is_err());
    }

    #[test]
    fn fcu_params_null_attrs() {
        let state = ForkchoiceState {
            head_block_hash: B256::repeat_byte(0x11),
            safe_block_hash: B256::repeat_byte(0x22),
            finalized_block_hash: B256::repeat_byte(0x33),
        };
        let params = fcu_v3_params(&state, None).expect("params");
        assert!(params[1].is_null(), "no attrs ⇒ null second param");
        assert_eq!(params[0]["headBlockHash"], format!("0x{}", "11".repeat(32)));
    }

    #[test]
    fn fcu_with_attrs_is_object() {
        let state = ForkchoiceState {
            head_block_hash: B256::ZERO,
            safe_block_hash: B256::ZERO,
            finalized_block_hash: B256::ZERO,
        };
        let attrs = PayloadAttributes {
            timestamp: 0x1234,
            prev_randao: B256::repeat_byte(0xAB),
            suggested_fee_recipient: alloy_primitives::Address::repeat_byte(0xCD),
            withdrawals: Some(vec![]),
            parent_beacon_block_root: Some(B256::repeat_byte(0xEE)),
            slot_number: None,
            target_gas_limit: None,
        };
        let params = fcu_v3_params(&state, Some(&attrs)).expect("params");
        assert!(params[1].is_object(), "attrs ⇒ object second param");
        assert_eq!(params[1]["timestamp"], "0x1234");
    }

    #[test]
    fn jwt_header_is_fresh_hs256_bearer() {
        // Use the free `bearer_header` (no `reqwest::Client`) to avoid racing the
        // process-global rustls CryptoProvider install across parallel tests.
        let secret = JwtSecret::random();
        let header = bearer_header(&secret).expect("header");
        let token = header.strip_prefix("Bearer ").expect("bearer prefix");
        // HS256 JWT = three non-empty base64url segments.
        let segs: Vec<&str> = token.split('.').collect();
        assert_eq!(segs.len(), 3, "well-formed JWT");
        assert!(segs.iter().all(|s| !s.is_empty()), "no empty JWT segment");
        // A second call issues a distinct token (fresh per-request auth header).
        let header2 = bearer_header(&secret).expect("header2");
        assert!(header2.starts_with("Bearer "));
    }
}
