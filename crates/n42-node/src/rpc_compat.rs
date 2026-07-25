use jsonrpsee::{
    MethodResponse,
    core::{
        middleware::{Batch, Notification},
        server::{BatchResponseBuilder, ResponsePayload as MethodResponsePayload},
    },
    server::middleware::rpc::RpcServiceT,
    types::{Request, Response, ResponsePayload},
};
use serde_json::Value;
use std::future::Future;
use tower::Layer;

/// Keeps gov5 H2's established JSON-RPC response shape while sharing reth's Ethereum RPC server.
///
/// Reth annotates mined transaction and log objects with the non-standard `blockTimestamp`
/// convenience field. The preserved gov5 API does not. Removing only that metadata field at the
/// response boundary keeps cross-client responses structurally identical without changing block,
/// transaction, receipt, or state commitments.
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
        let rewrite = self.enabled;
        let inner = self.inner.clone();
        async move { normalize_response(inner.batch(req).await, rewrite) }
    }

    fn notification<'a>(
        &self,
        notification: Notification<'a>,
    ) -> impl Future<Output = Self::NotificationResponse> + Send + 'a {
        self.inner.notification(notification)
    }
}

fn remove_block_timestamp(value: &mut Value) -> bool {
    match value {
        Value::Object(fields) => {
            let mut removed = fields.remove("blockTimestamp").is_some();
            removed |= fields
                .values_mut()
                .fold(false, |found, value| remove_block_timestamp(value) | found);
            removed
        }
        Value::Array(values) => values
            .iter_mut()
            .fold(false, |found, value| remove_block_timestamp(value) | found),
        _ => false,
    }
}

fn normalize_response(response: MethodResponse, enabled: bool) -> MethodResponse {
    if !enabled || !response.is_success() {
        return response;
    }
    if response.is_method_call() {
        normalize_method_response(response)
    } else if response.is_batch() {
        normalize_batch_response(response)
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
    if !remove_block_timestamp(&mut result) {
        return response;
    }

    let id = parsed.id.into_owned();
    let (_, _on_close, extensions) = response.into_parts();
    MethodResponse::response(id, MethodResponsePayload::success(result), usize::MAX)
        .with_extensions(extensions)
}

fn normalize_batch_response(response: MethodResponse) -> MethodResponse {
    let Ok(parsed) = serde_json::from_str::<Vec<Response<'_, Value>>>(response.as_ref()) else {
        return response;
    };
    let mut normalized = Vec::with_capacity(parsed.len());
    let mut removed = false;

    for item in parsed {
        let id = item.id.into_owned();
        let item = match item.payload {
            ResponsePayload::Success(result) => {
                let mut result = result.into_owned();
                removed |= remove_block_timestamp(&mut result);
                MethodResponse::response(id, MethodResponsePayload::success(result), usize::MAX)
            }
            ResponsePayload::Error(error) => MethodResponse::error(id, error.into_owned()),
        };
        normalized.push(item);
    }

    if !removed {
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
        MethodResponse::response(
            Id::Number(7),
            MethodResponsePayload::success(result),
            usize::MAX,
        )
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
    fn leaves_standard_profile_response_unchanged() {
        let response = success(json!({"blockTimestamp": "0x02"}));
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
}
