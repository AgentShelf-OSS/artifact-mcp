use std::time::{Duration, Instant};

use axum::{
    Json,
    body::{Body, HttpBody},
    http::StatusCode,
    response::{IntoResponse, Response},
};

use crate::{config::IngressConfig, mcp::protocol::OrderedJson};
use tokio::time::timeout;

use super::middleware::{IngressError, as_usize};

/// Read one HTTP body with a deadline and hard streamed byte limit.
///
/// Once the limit is crossed, the remaining frames are discarded rather than retained, but are
/// still consumed through the same absolute deadline. That keeps HTTP/1 from closing a request
/// stream under an uploading client (which surfaces as `EPIPE`) without granting an oversized or
/// slow body unbounded memory or time.
pub async fn read_body_limited(
    mut body: Body,
    limit: usize,
    read_timeout: Duration,
) -> Result<Vec<u8>, BodyReadError> {
    let deadline = Instant::now()
        .checked_add(read_timeout)
        .unwrap_or_else(Instant::now);
    let hinted_size = body.size_hint().upper();
    let mut too_large = hinted_size.is_some_and(|size| size > limit as u64);
    let mut output = Vec::with_capacity(if too_large {
        0
    } else {
        hinted_size.unwrap_or_default().min(limit as u64) as usize
    });
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(if too_large {
                BodyReadError::TooLarge
            } else {
                BodyReadError::Timeout
            });
        }
        let frame = timeout(
            remaining,
            std::future::poll_fn(|context| std::pin::Pin::new(&mut body).poll_frame(context)),
        )
        .await
        .map_err(|_| {
            if too_large {
                BodyReadError::TooLarge
            } else {
                BodyReadError::Timeout
            }
        })?;
        let Some(frame) = frame else {
            break;
        };
        let frame = frame.map_err(|_| {
            if too_large {
                BodyReadError::TooLarge
            } else {
                BodyReadError::Invalid
            }
        })?;
        let Ok(data) = frame.into_data() else {
            continue;
        };
        if too_large {
            continue;
        }
        if data.len() > limit.saturating_sub(output.len()) {
            too_large = true;
            output.clear();
            output.shrink_to_fit();
            continue;
        }
        output.extend_from_slice(&data);
    }
    if too_large {
        Err(BodyReadError::TooLarge)
    } else {
        Ok(output)
    }
}

/// Discard an HTTP body through one absolute deadline without retaining any payload bytes.
///
/// This is for request paths such as failed authentication where the server must not buffer an
/// untrusted body, but must still let an active HTTP/1 upload finish so the client receives the
/// intended response rather than a reset. The deadline bounds the worker time spent draining.
pub async fn discard_body_bounded(
    mut body: Body,
    read_timeout: Duration,
) -> Result<(), BodyReadError> {
    let deadline = Instant::now()
        .checked_add(read_timeout)
        .unwrap_or_else(Instant::now);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(BodyReadError::Timeout);
        }
        let frame = timeout(
            remaining,
            std::future::poll_fn(|context| std::pin::Pin::new(&mut body).poll_frame(context)),
        )
        .await
        .map_err(|_| BodyReadError::Timeout)?;
        let Some(frame) = frame else {
            return Ok(());
        };
        // Deliberately do not call `into_data`: consuming the frame drops its payload without
        // allocating a copy, regardless of whether this is a data or trailer frame.
        let _ = frame.map_err(|_| BodyReadError::Invalid)?;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodyReadError {
    Timeout,
    TooLarge,
    Invalid,
}

/// Validate depth, total node count, and collection width after JSON parsing. Parsing still uses
/// the existing insertion-order-preserving AST, preserving route semantics for valid inputs.
pub fn validate_json_complexity(
    value: &OrderedJson,
    config: &IngressConfig,
) -> Result<(), &'static str> {
    fn walk(
        value: &OrderedJson,
        depth: u64,
        nodes: &mut u64,
        config: &IngressConfig,
    ) -> Result<(), &'static str> {
        if depth > config.json_max_depth {
            return Err("json_too_deep");
        }
        *nodes = nodes.saturating_add(1);
        if *nodes > config.json_max_nodes {
            return Err("json_too_complex");
        }
        let children: &[OrderedJson] = match value {
            OrderedJson::Array(values) => values,
            OrderedJson::Object(entries) => {
                if entries.len() > as_usize(config.json_max_members) {
                    return Err("json_too_wide");
                }
                for (_, child) in entries {
                    walk(child, depth.saturating_add(1), nodes, config)?;
                }
                return Ok(());
            }
            _ => return Ok(()),
        };
        if children.len() > as_usize(config.json_max_members) {
            return Err("json_too_wide");
        }
        for child in children {
            walk(child, depth.saturating_add(1), nodes, config)?;
        }
        Ok(())
    }
    let mut nodes = 0;
    walk(value, 1, &mut nodes, config)
}

pub fn complexity_response(reason: &'static str) -> Response {
    let status = StatusCode::PAYLOAD_TOO_LARGE;
    (
        status,
        Json(IngressError {
            error: "JSON request exceeds complexity limits",
            code: reason,
        }),
    )
        .into_response()
}

pub fn body_error_response(error: BodyReadError) -> Response {
    match error {
        BodyReadError::Timeout => (
            StatusCode::REQUEST_TIMEOUT,
            Json(IngressError {
                error: "request timeout",
                code: "request_timeout",
            }),
        )
            .into_response(),
        BodyReadError::TooLarge => (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(IngressError {
                error: "payload too large",
                code: "payload_too_large",
            }),
        )
            .into_response(),
        BodyReadError::Invalid => (
            StatusCode::BAD_REQUEST,
            Json(IngressError {
                error: "invalid request body",
                code: "invalid_body",
            }),
        )
            .into_response(),
    }
}
