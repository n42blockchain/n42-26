use jsonrpsee::{
    MethodResponse,
    core::{
        middleware::{Batch, BatchEntry, Notification},
        server::{BatchResponseBuilder, ResponsePayload as MethodResponsePayload},
    },
    server::middleware::rpc::RpcServiceT,
    types::{Request, Response, ResponsePayload},
};
use serde_json::Value;
use std::{collections::HashSet, future::Future};
use tower::Layer;

/// Keeps gov5 H2's established JSON-RPC response shape while sharing reth's Ethereum RPC server.
///
/// Reth annotates mined transaction and log objects with the non-standard `blockTimestamp`
/// convenience field, while gov5 does not. Reth also serializes empty receipt log and topic lists
/// as `[]`, while gov5's established receipt shape uses `null`. Top-level `eth_getLogs` results
/// retain an intentionally different gov5 shape: empty data is `""`, indexes are JSON numbers, and
/// empty topics remain `[]`. Normalizing those response-only differences keeps cross-client
/// responses structurally identical without changing block, transaction, receipt, or state
/// commitments.
#[derive(Clone, Copy, Debug)]
pub struct Gov5RpcCompatLayer {
    enabled: bool,
}

impl Gov5RpcCompatLayer {
    pub const fn new(enabled: bool) -> Self {
        Self { enabled }
    }
}

impl<S> Layer<S> for Gov5RpcCompatLayer {
    type Service = Gov5RpcCompatService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Gov5RpcCompatService {
            inner,
            enabled: self.enabled,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Gov5RpcCompatService<S> {
    inner: S,
    enabled: bool,
}

impl<S> RpcServiceT for Gov5RpcCompatService<S>
where
    S: RpcServiceT<
            MethodResponse = MethodResponse,
            BatchResponse = MethodResponse,
            NotificationResponse = MethodResponse,
        > + Send
        + Sync
        + Clone
        + 'static,
{
    type MethodResponse = MethodResponse;
    type BatchResponse = MethodResponse;
    type NotificationResponse = MethodResponse;

    fn call<'a>(&self, req: Request<'a>) -> impl Future<Output = Self::MethodResponse> + Send + 'a {
        let rewrite = self.enabled && req.method.starts_with("eth_");
        let inner = self.inner.clone();
        async move { normalize_response(inner.call(req).await, rewrite) }
    }

    fn batch<'a>(&self, req: Batch<'a>) -> impl Future<Output = Self::BatchResponse> + Send + 'a {
        let rewrite_ids = if self.enabled {
            batch_eth_rewrite_ids(&req)
        } else {
            HashSet::new()
        };
        let inner = self.inner.clone();
        async move {
            let response = inner.batch(req).await;
            if rewrite_ids.is_empty() {
                response
            } else {
                normalize_batch_response(response, Some(&rewrite_ids))
            }
        }
    }

    fn notification<'a>(
        &self,
        notification: Notification<'a>,
    ) -> impl Future<Output = Self::NotificationResponse> + Send + 'a {
        self.inner.notification(notification)
    }
}

fn batch_eth_rewrite_ids(batch: &Batch<'_>) -> HashSet<jsonrpsee::types::Id<'static>> {
    let mut eth_ids = HashSet::new();
    let mut other_ids = HashSet::new();
    for entry in batch.iter().flatten() {
        let BatchEntry::Call(request) = entry else {
            continue;
        };
        let id = request.id().into_owned();
        if request.method.starts_with("eth_") {
            eth_ids.insert(id);
        } else {
            other_ids.insert(id);
        }
    }
    eth_ids.retain(|id| !other_ids.contains(id));
    eth_ids
}

fn normalize_gov5_metadata(value: &mut Value) -> bool {
    match value {
        Value::Object(fields) => {
            let mut changed = fields.remove("blockTimestamp").is_some();
            if fields
                .get("logs")
                .is_some_and(|logs| matches!(logs, Value::Array(entries) if entries.is_empty()))
            {
                fields.insert("logs".to_owned(), Value::Null);
                changed = true;
            }
            if fields
                .get("topics")
                .is_some_and(|topics| matches!(topics, Value::Array(entries) if entries.is_empty()))
            {
                fields.insert("topics".to_owned(), Value::Null);
                changed = true;
            }
            changed |= fields
                .values_mut()
                .fold(false, |found, value| normalize_gov5_metadata(value) | found);
            changed
        }
        Value::Array(values) => values
            .iter_mut()
            .fold(false, |found, value| normalize_gov5_metadata(value) | found),
        _ => false,
    }
}

fn normalize_log_quantity(fields: &mut serde_json::Map<String, Value>, name: &str) -> bool {
    let Some(quantity) = fields
        .get(name)
        .and_then(Value::as_str)
        .and_then(|value| value.strip_prefix("0x"))
        .and_then(|value| u64::from_str_radix(if value.is_empty() { "0" } else { value }, 16).ok())
    else {
        return false;
    };
    fields.insert(name.to_owned(), Value::Number(quantity.into()));
    true
}

fn normalize_top_level_logs(value: &mut Value) -> Option<bool> {
    let Value::Array(logs) = value else {
        return None;
    };
    if logs.is_empty()
        || !logs.iter().all(|log| {
            log.as_object().is_some_and(|fields| {
                fields.contains_key("data")
                    && fields.contains_key("logIndex")
                    && fields.contains_key("topics")
            })
        })
    {
        return None;
    }

    let mut changed = false;
    for log in logs {
        let Value::Object(fields) = log else {
            unreachable!("top-level log result shape was checked above");
        };
        changed |= fields.remove("blockTimestamp").is_some();
        if fields.get("data").and_then(Value::as_str) == Some("0x") {
            fields.insert("data".to_owned(), Value::String(String::new()));
            changed = true;
        }
        changed |= normalize_log_quantity(fields, "logIndex");
        changed |= normalize_log_quantity(fields, "transactionIndex");
    }
    Some(changed)
}

fn normalize_gov5_result(value: &mut Value) -> bool {
    normalize_top_level_logs(value).unwrap_or_else(|| normalize_gov5_metadata(value))
}

fn normalize_response(response: MethodResponse, enabled: bool) -> MethodResponse {
    if !enabled || !response.is_success() {
        return response;
    }
    if response.is_method_call() {
        normalize_method_response(response)
    } else if response.is_batch() {
        normalize_batch_response(response, None)
    } else {
        response
    }
}

fn normalize_method_response(response: MethodResponse) -> MethodResponse {
    let Ok(parsed) = serde_json::from_str::<Response<'_, Value>>(response.as_ref()) else {
        return response;
    };
    let ResponsePayload::Success(result) = parsed.payload else {
        return response;
    };
    let mut result = result.into_owned();
    if !normalize_gov5_result(&mut result) {
        return response;
    }

    let id = parsed.id.into_owned();
    let (_, _on_close, extensions) = response.into_parts();
    MethodResponse::response(id, MethodResponsePayload::success(result), usize::MAX)
        .with_extensions(extensions)
}

fn normalize_batch_response(
    response: MethodResponse,
    rewrite_ids: Option<&HashSet<jsonrpsee::types::Id<'static>>>,
) -> MethodResponse {
    let Ok(parsed) = serde_json::from_str::<Vec<Response<'_, Value>>>(response.as_ref()) else {
        return response;
    };
    let mut normalized = Vec::with_capacity(parsed.len());
    let mut changed = false;

    for item in parsed {
        let id = item.id.into_owned();
        let item = match item.payload {
            ResponsePayload::Success(result) => {
                let mut result = result.into_owned();
                if rewrite_ids.is_none_or(|ids| ids.contains(&id)) {
                    changed |= normalize_gov5_result(&mut result);
                }
                MethodResponse::response(id, MethodResponsePayload::success(result), usize::MAX)
            }
            ResponsePayload::Error(error) => MethodResponse::error(id, error.into_owned()),
        };
        normalized.push(item);
    }

    if !changed {
        return response;
    }

    let (_, _on_close, extensions) = response.into_parts();
    let mut batch = BatchResponseBuilder::new_with_limit(usize::MAX);
    for item in normalized {
        batch
            .append(item)
            .expect("unbounded compatibility response cannot exceed its limit");
    }
    MethodResponse::from_batch(batch.finish()).with_extensions(extensions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonrpsee::types::{ErrorObjectOwned, Id};
    use serde_json::json;

    fn success(result: Value) -> MethodResponse {
        success_with_id(Id::Number(7), result)
    }

    fn success_with_id(id: Id<'static>, result: Value) -> MethodResponse {
        MethodResponse::response(id, MethodResponsePayload::success(result), usize::MAX)
    }

    #[test]
    fn strips_nested_block_timestamps_from_method_response() {
        let response = success(json!({
            "hash": "0x01",
            "blockTimestamp": "0x02",
            "logs": [
                {"blockTimestamp": "0x02", "data": "0x01"},
                {"blockTimestamp": "0x02", "data": "0x02"}
            ]
        }));

        let normalized = normalize_response(response, true);
        let value: Value = serde_json::from_str(normalized.as_ref()).unwrap();
        assert_eq!(
            value["result"],
            json!({"hash": "0x01", "logs": [{"data": "0x01"}, {"data": "0x02"}]})
        );
    }

    #[test]
    fn normalizes_empty_receipt_logs_without_changing_top_level_log_results() {
        let receipt = success(json!({
            "transactionHash": "0x01",
            "logs": []
        }));
        let normalized = normalize_response(receipt, true);
        let value: Value = serde_json::from_str(normalized.as_ref()).unwrap();
        assert_eq!(
            value["result"],
            json!({"transactionHash": "0x01", "logs": null})
        );

        let logs = success(json!([]));
        let original = logs.as_ref().to_owned();
        assert_eq!(normalize_response(logs, true).as_ref(), original);
    }

    #[test]
    fn distinguishes_receipt_logs_from_top_level_log_results() {
        let log = json!({
            "address": "0x01",
            "blockTimestamp": "0x02",
            "data": "0x",
            "logIndex": "0x2a",
            "topics": [],
            "transactionIndex": "0x03"
        });

        let receipt = success(json!({
            "transactionHash": "0x04",
            "logs": [log.clone()]
        }));
        let normalized = normalize_response(receipt, true);
        let value: Value = serde_json::from_str(normalized.as_ref()).unwrap();
        assert_eq!(
            value["result"],
            json!({
                "transactionHash": "0x04",
                "logs": [{
                    "address": "0x01",
                    "data": "0x",
                    "logIndex": "0x2a",
                    "topics": null,
                    "transactionIndex": "0x03"
                }]
            })
        );

        let logs = success(json!([log]));
        let normalized = normalize_response(logs, true);
        let value: Value = serde_json::from_str(normalized.as_ref()).unwrap();
        assert_eq!(
            value["result"],
            json!([{
                "address": "0x01",
                "data": "",
                "logIndex": 42,
                "topics": [],
                "transactionIndex": 3
            }])
        );
    }

    #[test]
    fn leaves_standard_profile_response_unchanged() {
        let response = success(json!({"blockTimestamp": "0x02", "logs": []}));
        let original = response.as_ref().to_owned();
        assert_eq!(normalize_response(response, false).as_ref(), original);
    }

    #[test]
    fn strips_batch_successes_and_preserves_errors() {
        let mut batch = BatchResponseBuilder::new_with_limit(usize::MAX);
        batch
            .append(success(json!({"blockTimestamp": "0x02", "hash": "0x01"})))
            .unwrap();
        batch
            .append(MethodResponse::error(
                Id::Number(8),
                ErrorObjectOwned::owned(-32000, "unchanged", None::<()>),
            ))
            .unwrap();

        let normalized = normalize_response(MethodResponse::from_batch(batch.finish()), true);
        let value: Value = serde_json::from_str(normalized.as_ref()).unwrap();
        assert_eq!(value[0]["result"], json!({"hash": "0x01"}));
        assert_eq!(value[1]["error"]["code"], -32000);
        assert_eq!(value[1]["error"]["message"], "unchanged");
    }

    #[test]
    fn normalizes_only_eth_method_ids_in_mixed_batch() {
        let mut requests = Batch::new();
        requests.push(Request::borrowed(
            "eth_getTransactionReceipt",
            None,
            Id::Number(7),
        ));
        requests.push(Request::borrowed(
            "n42_getMobileReceipt",
            None,
            Id::Number(8),
        ));
        requests.push(Request::borrowed("debug_traceBlock", None, Id::Number(9)));
        let rewrite_ids = batch_eth_rewrite_ids(&requests);
        assert_eq!(rewrite_ids, HashSet::from([Id::Number(7)]));

        let mut batch = BatchResponseBuilder::new_with_limit(usize::MAX);
        batch
            .append(success_with_id(
                Id::Number(8),
                json!({"blockTimestamp": "0x02", "logs": [], "topics": []}),
            ))
            .unwrap();
        batch
            .append(success_with_id(
                Id::Number(7),
                json!({"blockTimestamp": "0x02", "logs": [], "topics": []}),
            ))
            .unwrap();
        batch
            .append(success_with_id(
                Id::Number(9),
                json!({"blockTimestamp": "0x02", "logs": [], "topics": []}),
            ))
            .unwrap();

        let normalized = normalize_batch_response(
            MethodResponse::from_batch(batch.finish()),
            Some(&rewrite_ids),
        );
        let value: Value = serde_json::from_str(normalized.as_ref()).unwrap();
        assert_eq!(
            value[0]["result"],
            json!({"blockTimestamp": "0x02", "logs": [], "topics": []})
        );
        assert_eq!(value[1]["result"], json!({"logs": null, "topics": null}));
        assert_eq!(
            value[2]["result"],
            json!({"blockTimestamp": "0x02", "logs": [], "topics": []})
        );
    }

    #[test]
    fn ambiguous_duplicate_id_is_not_rewritten() {
        let mut requests = Batch::new();
        requests.push(Request::borrowed("eth_getLogs", None, Id::Number(7)));
        requests.push(Request::borrowed(
            "n42_getMobileReceipt",
            None,
            Id::Number(7),
        ));
        assert!(batch_eth_rewrite_ids(&requests).is_empty());
    }
}
