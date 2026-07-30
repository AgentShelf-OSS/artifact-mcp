//! Owned by U01 (sol) — frozen domain, HTTP, and JSON-RPC error mappings.

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use thiserror::Error;

/// Application failures translated by HTTP routes.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AppError {
    #[error("{0}")]
    Validation(String),
    #[error("{0}")]
    Unauthorized(String),
    #[error("{0}")]
    Forbidden(String),
    #[error("{0}")]
    NotFound(String),
    #[error("Not found")]
    ConcealedNotFound,
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Gone(String),
    #[error("payload too large")]
    PayloadTooLarge,
    #[error("too many requests")]
    RateLimited,
    #[error("{0}")]
    Unavailable(String),
    #[error("internal error")]
    Internal,
}

impl AppError {
    /// Stable HTTP mapping shared by route groups.
    #[must_use]
    pub const fn http_status(&self) -> StatusCode {
        match self {
            Self::Validation(_) => StatusCode::BAD_REQUEST,
            Self::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            Self::Forbidden(_) => StatusCode::FORBIDDEN,
            Self::NotFound(_) | Self::ConcealedNotFound => StatusCode::NOT_FOUND,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::Gone(_) => StatusCode::GONE,
            Self::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            Self::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = self.http_status();
        let mut response = (
            status,
            Json(ErrorBody {
                error: self.to_string(),
            }),
        )
            .into_response();
        // Express's `res.json` emits `application/json; charset=utf-8`, while axum's `Json`
        // emits a bare `application/json`. The conformance oracle compares headers exactly, and
        // the concealed-404 body is the most-compared response in the suite (the 57-step
        // invariant-3 matrix), so the charset parameter has to match the oracle.
        response.headers_mut().insert(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("application/json; charset=utf-8"),
        );
        response
    }
}

/// Protocol-level JSON-RPC failures. Tool execution failures are intentionally separate.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum JsonRpcError {
    #[error("{0}")]
    Parse(String),
    #[error("Invalid Request")]
    InvalidRequest,
    #[error("Method not found: {0}")]
    MethodNotFound(String),
    #[error("{0}")]
    InvalidParams(String),
    #[error("unauthorized")]
    Unauthorized,
    #[error("Header mismatch: {0}")]
    HeaderMismatch(String),
    #[error("Unsupported protocol version")]
    UnsupportedProtocolVersion { requested: String },
    #[error("{0}")]
    Internal(String),
}

impl JsonRpcError {
    /// Frozen JSON-RPC code mapping.
    #[must_use]
    pub const fn code(&self) -> i32 {
        match self {
            Self::Parse(_) => -32_700,
            Self::InvalidRequest => -32_600,
            Self::MethodNotFound(_) => -32_601,
            Self::InvalidParams(_) => -32_602,
            Self::Unauthorized => -32_001,
            Self::HeaderMismatch(_) => -32_020,
            Self::UnsupportedProtocolVersion { .. } => -32_022,
            Self::Internal(_) => -32_603,
        }
    }

    /// HTTP envelope mapping used by the direct streamable-HTTP transport.
    #[must_use]
    pub const fn http_status(&self) -> StatusCode {
        match self {
            Self::Parse(_) | Self::HeaderMismatch(_) | Self::UnsupportedProtocolVersion { .. } => {
                StatusCode::BAD_REQUEST
            }
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::InvalidRequest
            | Self::MethodNotFound(_)
            | Self::InvalidParams(_)
            | Self::Internal(_) => StatusCode::OK,
        }
    }
}

/// MCP dispatch failures preserve the JSON-RPC protocol/tool-result boundary.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum McpError {
    #[error(transparent)]
    Protocol(#[from] JsonRpcError),
    #[error(transparent)]
    Tool(#[from] AppError),
}

impl McpError {
    /// A tool failure has no top-level JSON-RPC error code; it becomes `result.isError`.
    #[must_use]
    pub const fn json_rpc_code(&self) -> Option<i32> {
        match self {
            Self::Protocol(error) => Some(error.code()),
            Self::Tool(_) => None,
        }
    }

    #[must_use]
    pub const fn is_tool_error(&self) -> bool {
        matches!(self, Self::Tool(_))
    }

    #[must_use]
    pub const fn http_status(&self) -> StatusCode {
        match self {
            Self::Protocol(error) => error.http_status(),
            Self::Tool(_) => StatusCode::OK,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_domain_errors_to_http_statuses() {
        let cases = [
            (AppError::Validation("bad".into()), StatusCode::BAD_REQUEST),
            (
                AppError::Unauthorized("no".into()),
                StatusCode::UNAUTHORIZED,
            ),
            (AppError::Forbidden("no".into()), StatusCode::FORBIDDEN),
            (AppError::NotFound("missing".into()), StatusCode::NOT_FOUND),
            (AppError::ConcealedNotFound, StatusCode::NOT_FOUND),
            (AppError::Conflict("changed".into()), StatusCode::CONFLICT),
            (AppError::Gone("gone".into()), StatusCode::GONE),
            (AppError::PayloadTooLarge, StatusCode::PAYLOAD_TOO_LARGE),
            (
                AppError::Unavailable("down".into()),
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (AppError::Internal, StatusCode::INTERNAL_SERVER_ERROR),
        ];

        for (error, expected) in cases {
            assert_eq!(error.http_status(), expected);
        }
    }

    #[test]
    fn maps_frozen_json_rpc_codes_and_http_envelopes() {
        let cases = [
            (
                JsonRpcError::Parse("parse".into()),
                -32_700,
                StatusCode::BAD_REQUEST,
            ),
            (JsonRpcError::InvalidRequest, -32_600, StatusCode::OK),
            (
                JsonRpcError::MethodNotFound("x".into()),
                -32_601,
                StatusCode::OK,
            ),
            (
                JsonRpcError::InvalidParams("bad".into()),
                -32_602,
                StatusCode::OK,
            ),
            (
                JsonRpcError::Unauthorized,
                -32_001,
                StatusCode::UNAUTHORIZED,
            ),
            (
                JsonRpcError::Internal("bug".into()),
                -32_603,
                StatusCode::OK,
            ),
        ];

        for (error, code, status) in cases {
            assert_eq!(error.code(), code);
            assert_eq!(error.http_status(), status);
        }
    }

    #[test]
    fn tool_failures_stay_is_error_results() {
        let error = McpError::Tool(AppError::NotFound("Unknown artifact: x".into()));
        assert!(error.is_tool_error());
        assert_eq!(error.json_rpc_code(), None);
        assert_eq!(error.http_status(), StatusCode::OK);

        let protocol = McpError::Protocol(JsonRpcError::InvalidParams("bad".into()));
        assert!(!protocol.is_tool_error());
        assert_eq!(protocol.json_rpc_code(), Some(-32_602));
    }
}
