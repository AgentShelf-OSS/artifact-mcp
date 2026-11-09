//! Direct streamable-HTTP JSON-RPC envelopes and batching.
//!
//! The request AST deliberately retains object insertion order. The frozen wire contract
//! depends on it for validation-error traversal order and selection of the first bundle
//! HTML file when no entry is supplied.

use std::future::Future;

use serde_json::{Map, Number, Value};

pub use crate::model::json::OrderedJson;

use crate::{AppDeps, error::McpError, model::PublisherIdentity};

use super::dispatch::{ProtocolEra, SERVER_NAME, SERVER_VERSION, SUPPORTED_PROTOCOL_VERSIONS};

/// Run one JSON-RPC payload, preserving batch request order and omitting notification responses.
pub async fn handle_mcp(
    payload: OrderedJson,
    auth: &PublisherIdentity,
    deps: &AppDeps,
) -> Option<Value> {
    handle_mcp_for_era(payload, auth, deps, ProtocolEra::Legacy).await
}

/// Run one payload using either the legacy handshake contract or modern per-request semantics.
pub async fn handle_mcp_for_era(
    payload: OrderedJson,
    auth: &PublisherIdentity,
    deps: &AppDeps,
    era: ProtocolEra,
) -> Option<Value> {
    let auth = auth.clone();
    let deps = deps.clone();
    handle_mcp_with_era(payload, era, move |message| {
        let auth = auth.clone();
        let deps = deps.clone();
        async move { crate::mcp::dispatch::dispatch_for_era(&message, &auth, &deps, era).await }
    })
    .await
}

/// Protocol engine with an injected method dispatcher.
pub async fn handle_mcp_with<F, Fut>(payload: OrderedJson, dispatch: F) -> Option<Value>
where
    F: Fn(OrderedJson) -> Fut,
    Fut: Future<Output = Result<Value, McpError>>,
{
    handle_mcp_with_era(payload, ProtocolEra::Legacy, dispatch).await
}

/// Protocol engine with explicit era selection and an injected method dispatcher.
pub async fn handle_mcp_with_era<F, Fut>(
    payload: OrderedJson,
    era: ProtocolEra,
    dispatch: F,
) -> Option<Value>
where
    F: Fn(OrderedJson) -> Fut,
    Fut: Future<Output = Result<Value, McpError>>,
{
    match payload {
        OrderedJson::Array(messages) => {
            if era == ProtocolEra::Modern {
                return Some(rpc_error_for_era(
                    Value::Null,
                    -32_600,
                    "Batch requests are not supported by MCP 2026-07-28",
                    era,
                ));
            }
            if messages.is_empty() {
                return Some(rpc_error(Value::Null, -32_600, "Invalid Request"));
            }
            let mut responses = Vec::new();
            for message in messages {
                if let Some(response) = handle_one(message, era, &dispatch).await {
                    responses.push(response);
                }
            }
            (!responses.is_empty()).then_some(Value::Array(responses))
        }
        message => handle_one(message, era, &dispatch).await,
    }
}

async fn handle_one<F, Fut>(message: OrderedJson, era: ProtocolEra, dispatch: &F) -> Option<Value>
where
    F: Fn(OrderedJson) -> Fut,
    Fut: Future<Output = Result<Value, McpError>>,
{
    if let Some(error) = invalid_request(&message, era) {
        return Some(error);
    }
    let expects_response = message.contains_key("id");
    let id = request_id(&message).unwrap_or(Value::Null);
    let dispatched = dispatch(message).await;
    if !expects_response {
        return None;
    }
    Some(match dispatched {
        Ok(result) => response_result(id, result_for_era(result, era)),
        Err(McpError::Protocol(error)) => protocol_error(id, &error),
        Err(McpError::Tool(error)) => tool_error(id, &error.to_string(), era),
    })
}

/// A readable MCP request ID is a string or an integer, including zero and negative integers.
#[must_use]
pub fn request_id(message: &OrderedJson) -> Option<Value> {
    message
        .get("id")
        .filter(|id| match id {
            OrderedJson::String(_) => true,
            OrderedJson::Number(number) => {
                number.is_i64()
                    || number.is_u64()
                    || number
                        .as_f64()
                        .is_some_and(|value| value.is_finite() && value.fract() == 0.0)
            }
            _ => false,
        })
        .cloned()
        .map(OrderedJson::into_value)
}

/// Validate the envelope before either authorization by method or method dispatch.
#[must_use]
pub fn invalid_request(message: &OrderedJson, era: ProtocolEra) -> Option<Value> {
    let id = request_id(message);
    let valid = message.as_object().is_some()
        && message.get("jsonrpc").and_then(OrderedJson::as_str) == Some("2.0")
        && message
            .get("method")
            .and_then(OrderedJson::as_str)
            .is_some()
        && (!message.contains_key("id") || id.is_some());
    (!valid).then(|| rpc_error_for_era(id.unwrap_or(Value::Null), -32_600, "Invalid Request", era))
}

fn result_for_era(mut result: Value, era: ProtocolEra) -> Value {
    if era != ProtocolEra::Modern {
        return result;
    }
    let Some(object) = result.as_object_mut() else {
        return result;
    };
    object
        .entry("resultType")
        .or_insert_with(|| Value::String("complete".to_owned()));
    let meta = object
        .entry("_meta")
        .or_insert_with(|| Value::Object(Map::new()));
    if !meta.is_object() {
        *meta = Value::Object(Map::new());
    }
    if let Some(meta) = meta.as_object_mut() {
        meta.insert(
            "io.modelcontextprotocol/serverInfo".to_owned(),
            serde_json::json!({ "name": SERVER_NAME, "version": SERVER_VERSION }),
        );
    }
    result
}

fn response_result(id: Value, result: Value) -> Value {
    let mut response = Map::new();
    response.insert("jsonrpc".to_owned(), Value::String("2.0".to_owned()));
    response.insert("id".to_owned(), id);
    response.insert("result".to_owned(), result);
    Value::Object(response)
}

#[must_use]
pub fn rpc_error(id: Value, code: i32, message: &str) -> Value {
    rpc_error_with_data(id, code, message, None)
}

#[must_use]
pub fn rpc_error_with_data(id: Value, code: i32, message: &str, data: Option<Value>) -> Value {
    rpc_error_with_data_for_era(id, code, message, data, ProtocolEra::Legacy)
}

#[must_use]
pub fn rpc_error_for_era(id: Value, code: i32, message: &str, era: ProtocolEra) -> Value {
    rpc_error_with_data_for_era(id, code, message, None, era)
}

/// Modern errors omit an unreadable ID; legacy JSON-RPC errors use null.
#[must_use]
pub fn rpc_error_with_data_for_era(
    id: Value,
    code: i32,
    message: &str,
    data: Option<Value>,
    era: ProtocolEra,
) -> Value {
    let mut error = Map::new();
    error.insert("code".to_owned(), Value::Number(Number::from(code)));
    error.insert("message".to_owned(), Value::String(message.to_owned()));
    if let Some(data) = data {
        error.insert("data".to_owned(), data);
    }
    let mut response = Map::new();
    response.insert("jsonrpc".to_owned(), Value::String("2.0".to_owned()));
    if !id.is_null() || era == ProtocolEra::Legacy {
        response.insert("id".to_owned(), id);
    }
    response.insert("error".to_owned(), Value::Object(error));
    Value::Object(response)
}

fn protocol_error(id: Value, error: &crate::error::JsonRpcError) -> Value {
    let data = match error {
        crate::error::JsonRpcError::UnsupportedProtocolVersion { requested } => {
            Some(serde_json::json!({
                "supported": SUPPORTED_PROTOCOL_VERSIONS,
                "requested": requested
            }))
        }
        _ => None,
    };
    rpc_error_with_data(id, error.code(), &error.to_string(), data)
}

fn tool_error(id: Value, message: &str, era: ProtocolEra) -> Value {
    let content = serde_json::json!([{ "type": "text", "text": message }]);
    response_result(
        id,
        result_for_era(
            serde_json::json!({ "content": content, "isError": true }),
            era,
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn invalid_ids_never_reach_an_injected_dispatcher() {
        let calls = std::cell::Cell::new(0);
        for era in [ProtocolEra::Legacy, ProtocolEra::Modern] {
            for id in [
                Value::Null,
                serde_json::json!(true),
                serde_json::json!([]),
                serde_json::json!({}),
                serde_json::json!(1.5),
            ] {
                let message = serde_json::from_value(serde_json::json!({
                    "jsonrpc": "2.0", "id": id, "method": "tools/call"
                }))
                .expect("request AST");
                let response = handle_mcp_with_era(message, era, |_| {
                    calls.set(calls.get() + 1);
                    std::future::ready(Ok(serde_json::json!({})))
                })
                .await
                .expect("invalid ID error");
                assert_eq!(response["error"]["code"], -32_600);
                assert_eq!(response.get("id").is_some(), era == ProtocolEra::Legacy);
            }
        }
        assert_eq!(calls.get(), 0);
    }
}
