use axum::{
    Json,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};

use crate::{
    mcp::{
        dispatch::{MODERN_PROTOCOL_VERSION, ProtocolEra, SUPPORTED_PROTOCOL_VERSIONS},
        protocol::{self, OrderedJson},
    },
    security::oauth::SUPPORTED_SCOPES,
};

#[derive(Debug, PartialEq)]
pub(super) struct TransportValidationError {
    pub(super) code: i32,
    pub(super) message: String,
    pub(super) data: Option<Value>,
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

pub(super) fn validate_transport(
    payload: &OrderedJson,
    headers: &HeaderMap,
) -> Result<ProtocolEra, TransportValidationError> {
    let header_version = optional_header(headers, "mcp-protocol-version")?;
    let method = payload.get("method").and_then(OrderedJson::as_str);
    let modern_intent = modern_transport(payload, headers);

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

fn contains_modern_request_metadata(payload: &OrderedJson) -> bool {
    match payload {
        OrderedJson::Array(messages) => messages.iter().any(contains_modern_request_metadata),
        _ => request_meta(payload).is_some_and(|meta| {
            meta.get("io.modelcontextprotocol/protocolVersion")
                .is_some()
        }),
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

pub(super) fn modern_transport(payload: &OrderedJson, headers: &HeaderMap) -> bool {
    headers
        .get("mcp-protocol-version")
        .and_then(|value| value.to_str().ok())
        == Some(MODERN_PROTOCOL_VERSION)
        || contains_modern_request_metadata(payload)
        || payload.get("method").and_then(OrderedJson::as_str) == Some("server/discover")
}

pub(super) fn request_id(payload: &OrderedJson) -> Value {
    protocol::request_id(payload).unwrap_or(Value::Null)
}

pub(super) fn unauthorized_response(config: &crate::config::AppConfig) -> Response {
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

pub(super) fn insufficient_scope_response(
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
