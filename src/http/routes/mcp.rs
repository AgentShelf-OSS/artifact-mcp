//! Authenticated POST/OPTIONS `/mcp` transport.

use std::{future::poll_fn, pin::Pin};

use axum::{
    Json, Router,
    body::{Body, HttpBody},
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Serialize;
use serde_json::{Value, json};

use crate::{
    AppDeps,
    mcp::{
        dispatch::{
            MODERN_PROTOCOL_VERSION, PROTOCOL_VERSION, ProtocolEra, SUPPORTED_PROTOCOL_VERSIONS,
        },
        protocol::{self, OrderedJson},
    },
    observability::{McpMetricLabels, McpOutcome, labels_for},
    security::oauth::{SUPPORTED_SCOPES, required_scope},
};

pub(crate) fn router() -> Router<AppDeps> {
    Router::new()
        .route("/mcp", post(mcp).options(mcp_options))
        .route("/metrics", get(metrics))
        .route(
            "/.well-known/oauth-protected-resource",
            get(oauth_protected_resource),
        )
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(oauth_protected_resource),
        )
}

async fn mcp(State(deps): State<AppDeps>, request: Request) -> Response {
    let mut observation = deps.mcp_telemetry.begin();
    let request_id = observation.request_id().to_owned();
    let (mut response, labels, outcome) = mcp_inner(&deps, request).await;
    observation.set_labels(labels);
    let result_bytes = response
        .body()
        .size_hint()
        .exact()
        .and_then(|size| usize::try_from(size).ok())
        .unwrap_or_default();
    if let Ok(value) = HeaderValue::from_str(&request_id) {
        response.headers_mut().insert("x-request-id", value);
    }
    observation.finish(outcome, result_bytes);
    response
}

async fn mcp_inner(deps: &AppDeps, request: Request) -> (Response, McpMetricLabels, McpOutcome) {
    let transport_headers = request.headers().clone();
    let initial_labels = labels_for(protocol_dimension(&transport_headers), None, None);
    // This await happens before the body is read or bounded. `/mcp` is Access-bypassed, so the
    // publisher key is the only gate protecting the multi-megabyte JSON parser.
    let auth = match deps.publisher_auth.authenticate(request.headers()).await {
        Ok(auth) => auth,
        Err(_) => {
            // Authentication deliberately precedes body reading, but the request body still has to
            // be drained before the 401 is written. Returning immediately drops the body unread,
            // so hyper closes the connection while the client is mid-upload and the client sees
            // `write EPIPE` instead of the status. Node's HTTP layer discards the remainder for
            // us; axum does not. Retained bytes stay bounded by the configured limit, so an
            // unauthenticated caller still cannot make us buffer an unbounded body.
            let limit = usize::try_from(deps.config.body.mcp_json).unwrap_or(usize::MAX);
            let _ = read_body_and_drain(request.into_body(), limit).await;
            return observed(
                unauthorized_response(&deps.config),
                initial_labels,
                McpOutcome::AuthenticationFailure,
            );
        }
    };
    let limit = usize::try_from(deps.config.body.mcp_json).unwrap_or(usize::MAX);
    let bytes = match read_body_and_drain(request.into_body(), limit).await {
        Ok(BufferedRequestBody::Complete(bytes)) => bytes,
        Ok(BufferedRequestBody::TooLarge) | Err(_) => {
            return observed(
                json_error(StatusCode::PAYLOAD_TOO_LARGE, "payload too large"),
                initial_labels,
                McpOutcome::ValidationFailure,
            );
        }
    };
    let payload = match serde_json::from_slice::<protocol::OrderedJson>(&bytes) {
        Ok(payload) => payload,
        Err(_) => {
            return observed(
                json_error(StatusCode::BAD_REQUEST, "invalid JSON"),
                initial_labels,
                McpOutcome::ValidationFailure,
            );
        }
    };
    let era = match validate_transport(&payload, &transport_headers) {
        Ok(era) => era,
        Err(error) => {
            let labels = labels_for(
                protocol_dimension(&transport_headers),
                payload.get("method").and_then(OrderedJson::as_str),
                request_name(&payload),
            );
            return observed(
                (
                    StatusCode::BAD_REQUEST,
                    Json(protocol::rpc_error_with_data(
                        request_id(&payload),
                        error.code,
                        &error.message,
                        error.data,
                    )),
                )
                    .into_response(),
                labels,
                McpOutcome::ValidationFailure,
            );
        }
    };
    let method = payload
        .get("method")
        .and_then(OrderedJson::as_str)
        .unwrap_or_default();
    let name = request_name(&payload);
    let labels = labels_for(
        match era {
            ProtocolEra::Legacy => PROTOCOL_VERSION,
            ProtocolEra::Modern => MODERN_PROTOCOL_VERSION,
        },
        Some(method),
        name,
    );
    if let Some(scope) = required_scope(method, name)
        && !auth.has_scope(scope)
    {
        return observed(
            insufficient_scope_response(&deps.config, request_id(&payload), scope),
            labels,
            McpOutcome::AuthorizationFailure,
        );
    }
    match protocol::handle_mcp_for_era(payload, &auth, deps, era).await {
        Some(response) => {
            let outcome = response_outcome(&response);
            let status = if era == ProtocolEra::Modern
                && response
                    .get("error")
                    .and_then(|error| error.get("code"))
                    .and_then(Value::as_i64)
                    == Some(-32_601)
            {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::OK
            };
            observed((status, Json(response)).into_response(), labels, outcome)
        }
        None => observed(
            StatusCode::ACCEPTED.into_response(),
            labels,
            McpOutcome::Success,
        ),
    }
}

fn observed(
    response: Response,
    labels: McpMetricLabels,
    outcome: McpOutcome,
) -> (Response, McpMetricLabels, McpOutcome) {
    (response, labels, outcome)
}

fn protocol_dimension(headers: &HeaderMap) -> &'static str {
    match headers
        .get("mcp-protocol-version")
        .and_then(|value| value.to_str().ok())
    {
        Some(MODERN_PROTOCOL_VERSION) => MODERN_PROTOCOL_VERSION,
        Some(PROTOCOL_VERSION) | None => PROTOCOL_VERSION,
        Some(_) => "unsupported",
    }
}

fn request_name(payload: &OrderedJson) -> Option<&str> {
    let method = payload.get("method").and_then(OrderedJson::as_str);
    payload
        .get("params")
        .and_then(|params| {
            if method == Some("resources/read") {
                params.get("uri")
            } else if method.is_some_and(|method| method.starts_with("tasks/")) {
                params.get("taskId")
            } else {
                params.get("name")
            }
        })
        .and_then(OrderedJson::as_str)
}

fn response_outcome(response: &Value) -> McpOutcome {
    let Some(error) = response.get("error") else {
        return McpOutcome::Success;
    };
    let code = error.get("code").and_then(Value::as_i64);
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match code {
        Some(-32_603)
            if message.contains("output failed validation")
                || message.contains("output schema")
                || message.contains("structured content") =>
        {
            McpOutcome::OutputValidationFailure
        }
        Some(-32_603) => McpOutcome::ServerFailure,
        Some(-32_602) | Some(-32_600) | Some(-32_700) => McpOutcome::ValidationFailure,
        _ => McpOutcome::ProtocolError,
    }
}

async fn metrics(State(deps): State<AppDeps>) -> Response {
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
        deps.mcp_telemetry.render_prometheus(),
    )
        .into_response()
}

async fn oauth_protected_resource(State(deps): State<AppDeps>) -> Response {
    if !deps.config.oauth.enabled() {
        return StatusCode::NOT_FOUND.into_response();
    }
    Json(json!({
        "resource": deps.config.mcp_resource_uri(),
        "authorization_servers": [deps.config.oauth.issuer],
        "bearer_methods_supported": ["header"],
        "scopes_supported": SUPPORTED_SCOPES
    }))
    .into_response()
}

async fn mcp_options() -> Response {
    let mut response = StatusCode::NO_CONTENT.into_response();
    response.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    response.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static(
            "authorization, content-type, accept, mcp-protocol-version, mcp-method, mcp-name",
        ),
    );
    response
}

#[derive(Debug, PartialEq)]
struct TransportValidationError {
    code: i32,
    message: String,
    data: Option<Value>,
}

impl TransportValidationError {
    fn header(message: impl Into<String>) -> Self {
        Self {
            code: -32_020,
            message: format!("Header mismatch: {}", message.into()),
            data: None,
        }
    }

    fn invalid_params(message: impl Into<String>) -> Self {
        Self {
            code: -32_602,
            message: message.into(),
            data: None,
        }
    }

    fn unsupported(requested: &str) -> Self {
        Self {
            code: -32_022,
            message: "Unsupported protocol version".to_owned(),
            data: Some(json!({
                "supported": SUPPORTED_PROTOCOL_VERSIONS,
                "requested": requested
            })),
        }
    }

    fn invalid_request(message: impl Into<String>) -> Self {
        Self {
            code: -32_600,
            message: message.into(),
            data: None,
        }
    }
}

fn validate_transport(
    payload: &OrderedJson,
    headers: &HeaderMap,
) -> Result<ProtocolEra, TransportValidationError> {
    let header_version = optional_header(headers, "mcp-protocol-version")?;
    let has_modern_metadata = contains_request_meta(payload);
    let method = payload.get("method").and_then(OrderedJson::as_str);
    let modern_intent = header_version
        .as_deref()
        .is_some_and(|version| version != PROTOCOL_VERSION)
        || has_modern_metadata
        || method == Some("server/discover");

    if !modern_intent {
        return Ok(ProtocolEra::Legacy);
    }
    if payload.as_array().is_some() {
        return Err(TransportValidationError::invalid_request(
            "Batch requests are not supported by MCP 2026-07-28",
        ));
    }
    let method = method.ok_or_else(|| {
        TransportValidationError::invalid_request("Invalid Request: method must be a string")
    })?;
    let header_version = header_version.ok_or_else(|| {
        TransportValidationError::header("required MCP-Protocol-Version header is missing")
    })?;
    let body_version = request_meta(payload)
        .and_then(|meta| meta.get("io.modelcontextprotocol/protocolVersion"))
        .and_then(OrderedJson::as_str)
        .ok_or_else(|| {
            TransportValidationError::invalid_params(
                "Missing required request metadata: io.modelcontextprotocol/protocolVersion",
            )
        })?;
    if header_version != body_version {
        return Err(TransportValidationError::header(format!(
            "MCP-Protocol-Version header value '{header_version}' does not match body value '{body_version}'"
        )));
    }
    if body_version != MODERN_PROTOCOL_VERSION {
        return Err(TransportValidationError::unsupported(body_version));
    }
    if request_meta(payload)
        .and_then(|meta| meta.get("io.modelcontextprotocol/clientCapabilities"))
        .and_then(OrderedJson::as_object)
        .is_none()
    {
        return Err(TransportValidationError::invalid_params(
            "Missing required request metadata: io.modelcontextprotocol/clientCapabilities",
        ));
    }

    let header_method = optional_header(headers, "mcp-method")?
        .ok_or_else(|| TransportValidationError::header("required Mcp-Method header is missing"))?;
    if header_method != method {
        return Err(TransportValidationError::header(format!(
            "Mcp-Method header value '{header_method}' does not match body value '{method}'"
        )));
    }

    if matches!(
        method,
        "tools/call"
            | "resources/read"
            | "prompts/get"
            | "tasks/get"
            | "tasks/update"
            | "tasks/cancel"
    ) {
        let body_name = payload
            .get("params")
            .and_then(|params| {
                if method == "resources/read" {
                    params.get("uri")
                } else if method.starts_with("tasks/") {
                    params.get("taskId")
                } else {
                    params.get("name")
                }
            })
            .and_then(OrderedJson::as_str)
            .ok_or_else(|| {
                TransportValidationError::invalid_params(format!(
                    "{method} requires a string params.{}",
                    if method == "resources/read" {
                        "uri"
                    } else if method.starts_with("tasks/") {
                        "taskId"
                    } else {
                        "name"
                    }
                ))
            })?;
        let raw_header_name = optional_header(headers, "mcp-name")?.ok_or_else(|| {
            TransportValidationError::header("required Mcp-Name header is missing")
        })?;
        let header_name = decode_header_value(&raw_header_name)
            .ok_or_else(|| TransportValidationError::header("Mcp-Name header is malformed"))?;
        if header_name != body_name {
            return Err(TransportValidationError::header(format!(
                "Mcp-Name header value '{header_name}' does not match body value '{body_name}'"
            )));
        }
    }

    Ok(ProtocolEra::Modern)
}

fn optional_header(
    headers: &HeaderMap,
    name: &'static str,
) -> Result<Option<String>, TransportValidationError> {
    headers
        .get(name)
        .map(|value| {
            value.to_str().map(str::to_owned).map_err(|_| {
                TransportValidationError::header(format!("{name} header is malformed"))
            })
        })
        .transpose()
}

fn contains_request_meta(payload: &OrderedJson) -> bool {
    match payload {
        OrderedJson::Array(messages) => messages.iter().any(contains_request_meta),
        _ => payload
            .get("params")
            .is_some_and(|params| params.contains_key("_meta")),
    }
}

fn request_meta(payload: &OrderedJson) -> Option<&OrderedJson> {
    payload.get("params").and_then(|params| params.get("_meta"))
}

fn decode_header_value(value: &str) -> Option<String> {
    let Some(encoded) = value
        .strip_prefix("=?base64?")
        .and_then(|value| value.strip_suffix("?="))
    else {
        return Some(value.to_owned());
    };
    let decoded = STANDARD.decode(encoded).ok()?;
    String::from_utf8(decoded).ok()
}

fn request_id(payload: &OrderedJson) -> Value {
    payload
        .get("id")
        .cloned()
        .map_or(Value::Null, OrderedJson::into_value)
}

fn unauthorized_response(config: &crate::config::AppConfig) -> Response {
    let mut response = (
        StatusCode::UNAUTHORIZED,
        Json(protocol::rpc_error(
            serde_json::Value::Null,
            -32_001,
            "unauthorized",
        )),
    )
        .into_response();
    if config.oauth.enabled()
        && let Ok(challenge) = HeaderValue::from_str(&format!(
            "Bearer resource_metadata=\"{}\", scope=\"{}\"",
            config.oauth_resource_metadata_uri(),
            SUPPORTED_SCOPES.join(" ")
        ))
    {
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, challenge);
    }
    response
}

fn insufficient_scope_response(
    config: &crate::config::AppConfig,
    id: Value,
    scope: &str,
) -> Response {
    let mut response = (
        StatusCode::FORBIDDEN,
        Json(protocol::rpc_error_with_data(
            id,
            -32_003,
            "insufficient_scope",
            Some(json!({ "requiredScope": scope })),
        )),
    )
        .into_response();
    if let Ok(challenge) = HeaderValue::from_str(&format!(
        "Bearer error=\"insufficient_scope\", scope=\"{scope}\", resource_metadata=\"{}\"",
        config.oauth_resource_metadata_uri()
    )) {
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, challenge);
    }
    response
}

#[derive(Serialize)]
struct HttpError<'a> {
    error: &'a str,
}

fn json_error(status: StatusCode, message: &str) -> Response {
    (status, Json(HttpError { error: message })).into_response()
}

#[derive(Debug, PartialEq, Eq)]
enum BufferedRequestBody {
    Complete(Vec<u8>),
    TooLarge,
}

/// Buffer at most `limit` bytes while continuing to poll an oversized body to end-of-stream.
///
/// Returning the 413 only after the body has been drained keeps Hyper from closing the request
/// side while the client is still writing, which otherwise surfaces as `EPIPE` instead of the
/// response. The caller deliberately authenticates before entering this function.
async fn read_body_and_drain(
    mut body: Body,
    limit: usize,
) -> Result<BufferedRequestBody, axum::Error> {
    let hinted_size = body
        .size_hint()
        .upper()
        .and_then(|size| usize::try_from(size).ok());
    let mut too_large = hinted_size.is_some_and(|size| size > limit);
    let capacity = if too_large {
        0
    } else {
        hinted_size.unwrap_or_default().min(limit)
    };
    let mut content = Vec::with_capacity(capacity);

    loop {
        let frame = poll_fn(|context| Pin::new(&mut body).poll_frame(context)).await;
        let Some(frame) = frame else {
            break;
        };
        let frame = frame?;
        let Ok(data) = frame.into_data() else {
            continue;
        };
        if too_large {
            continue;
        }
        if data.len() > limit.saturating_sub(content.len()) {
            too_large = true;
            content.clear();
            content.shrink_to_fit();
        } else {
            content.extend_from_slice(&data);
        }
    }

    if too_large {
        Ok(BufferedRequestBody::TooLarge)
    } else {
        Ok(BufferedRequestBody::Complete(content))
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;

    use super::{BufferedRequestBody, read_body_and_drain};

    #[tokio::test]
    async fn request_body_limit_accepts_the_exact_boundary() {
        let body = read_body_and_drain(Body::from("1234"), 4)
            .await
            .expect("read request body");
        assert_eq!(body, BufferedRequestBody::Complete(b"1234".to_vec()));
    }

    #[tokio::test]
    async fn oversized_request_body_is_drained_and_reported_after_the_boundary() {
        let body = read_body_and_drain(Body::from("12345"), 4)
            .await
            .expect("drain request body");
        assert_eq!(body, BufferedRequestBody::TooLarge);
    }
}
