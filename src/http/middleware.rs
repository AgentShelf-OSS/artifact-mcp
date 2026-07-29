//! HTTP boundary middleware shared by route units.

use std::sync::Arc;

use axum::{
    body::{Body, to_bytes},
    extract::{DefaultBodyLimit, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};

use crate::{
    config::{AccessIdentityMode, AppConfig},
    ports::PageRenderer,
    security::access_retry::{ACCESS_RETRY_PARAM, access_retry_target},
};

/// Dependencies for the listener-level Cloudflare Access session retry response.
#[derive(Clone)]
pub struct AccessRetryState {
    mode: AccessIdentityMode,
    pages: Arc<dyn PageRenderer>,
}

impl AccessRetryState {
    #[must_use]
    pub const fn new(mode: AccessIdentityMode, pages: Arc<dyn PageRenderer>) -> Self {
        Self { mode, pages }
    }
}

/// Return Node's once-only Access session retry page before entering the Express-equivalent app.
///
/// This middleware must wrap [`express_etag`]: Node handles the retry with native `res.end`, so
/// the page receives the listener security/cache headers but no Express-generated ETag.
pub async fn access_session_retry(
    State(state): State<AccessRetryState>,
    request: Request,
    next: Next,
) -> Response {
    let Some(target) = access_retry_target(
        request.method(),
        request.uri(),
        request.headers(),
        state.mode,
        ACCESS_RETRY_PARAM,
    ) else {
        return next.run(request).await;
    };

    let body = match state.pages.access_retry(&target) {
        Ok(body) => body,
        Err(error) => return error.into_response(),
    };
    let mut response = Response::new(Body::from(body));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response.headers_mut().insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response
}

/// Add the same body-derived weak ETag as Express's default `res.send`/`res.json` path.
///
/// The current Node app bypasses `res.send` for redirects, bodyless MCP responses, listener-level
/// Access retry pages, and its final 404 handler. Within the Axum router those paths are exactly
/// the redirects carrying `Location` and responses without a `Content-Type`, so they remain
/// untagged. All normal HTML, JSON, raw artifact, public-share, thumbnail, and rendered error
/// responses pass through this middleware.
pub async fn express_etag(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let request_headers = request.headers().clone();
    let response = next.run(request).await;

    if !express_send_response(&response) {
        return response;
    }

    if let Some(etag) = response.headers().get(header::ETAG).cloned() {
        return apply_conditional_request(response, &method, &request_headers, &etag);
    }

    let (mut parts, body) = response.into_parts();
    let bytes = match to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::error!(error = %error, "could not buffer response for ETag generation");
            return Response::from_parts(parts, Body::empty());
        }
    };
    let etag = weak_etag(&bytes);
    let Ok(etag) = HeaderValue::from_str(&etag) else {
        return Response::from_parts(parts, Body::from(bytes));
    };
    parts.headers.insert(header::ETAG, etag.clone());

    let response = Response::from_parts(parts, Body::from(bytes));
    apply_conditional_request(response, &method, &request_headers, &etag)
}

fn express_send_response(response: &Response) -> bool {
    response.headers().contains_key(header::CONTENT_TYPE)
        && !(response.status().is_redirection()
            && response.headers().contains_key(header::LOCATION))
}

fn apply_conditional_request(
    mut response: Response,
    method: &Method,
    request_headers: &HeaderMap,
    etag: &HeaderValue,
) -> Response {
    if response_is_fresh(method, response.status(), request_headers, etag) {
        *response.status_mut() = StatusCode::NOT_MODIFIED;
        response.headers_mut().remove(header::CONTENT_TYPE);
        response.headers_mut().remove(header::CONTENT_LENGTH);
        response.headers_mut().remove(header::TRANSFER_ENCODING);
        // An empty stream has an unknown size hint, preventing Axum's outer route service from
        // reintroducing `content-length: 0` after Express deliberately removed that header.
        *response.body_mut() = Body::from_stream(Body::empty().into_data_stream());
    } else if response.status() == StatusCode::NO_CONTENT {
        response.headers_mut().remove(header::CONTENT_TYPE);
        response.headers_mut().remove(header::CONTENT_LENGTH);
        response.headers_mut().remove(header::TRANSFER_ENCODING);
        *response.body_mut() = Body::from_stream(Body::empty().into_data_stream());
    } else if response.status() == StatusCode::RESET_CONTENT {
        response
            .headers_mut()
            .insert(header::CONTENT_LENGTH, HeaderValue::from_static("0"));
        response.headers_mut().remove(header::TRANSFER_ENCODING);
        *response.body_mut() = Body::empty();
    }
    response
}

fn response_is_fresh(
    method: &Method,
    status: StatusCode,
    request_headers: &HeaderMap,
    etag: &HeaderValue,
) -> bool {
    if !matches!(*method, Method::GET | Method::HEAD)
        || !(status.is_success() || status == StatusCode::NOT_MODIFIED)
    {
        return false;
    }
    if request_headers
        .get(header::CACHE_CONTROL)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.split(',').any(|part| part.trim() == "no-cache"))
    {
        return false;
    }

    let Some(candidate) = request_headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    // No current Express `send` response carries Last-Modified. The `fresh` package therefore
    // treats any simultaneous If-Modified-Since condition as stale even when the ETag matches.
    if request_headers.contains_key(header::IF_MODIFIED_SINCE) {
        return false;
    }
    if candidate == "*" {
        return true;
    }
    let Ok(etag) = etag.to_str() else {
        return false;
    };
    candidate.split(',').any(|candidate| {
        let candidate = candidate.trim_matches(' ');
        candidate == etag
            || candidate
                .strip_prefix("W/")
                .is_some_and(|strong| strong == etag)
            || etag
                .strip_prefix("W/")
                .is_some_and(|strong| strong == candidate)
    })
}

/// Express/`etag` weak entity tag: lowercase hexadecimal byte length, a dash, and the first 27
/// Base64 characters of SHA-1 over the exact response bytes.
#[must_use]
pub fn weak_etag(content: &[u8]) -> String {
    let digest = sha1(content);
    let encoded = STANDARD.encode(digest);
    let hash = encoded.strip_suffix('=').unwrap_or(&encoded);
    format!("W/\"{:x}-{hash}\"", content.len())
}

fn sha1(content: &[u8]) -> [u8; 20] {
    let mut padded = Vec::with_capacity(content.len().saturating_add(72));
    padded.extend_from_slice(content);
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    let bit_len = u64::try_from(content.len()).map_or(u64::MAX, |length| length.wrapping_mul(8));
    padded.extend_from_slice(&bit_len.to_be_bytes());

    let mut state = [
        0x6745_2301_u32,
        0xefcd_ab89,
        0x98ba_dcfe,
        0x1032_5476,
        0xc3d2_e1f0,
    ];
    for chunk in padded.chunks_exact(64) {
        let mut words = [0_u32; 80];
        for (index, word) in words.iter_mut().take(16).enumerate() {
            let start = index * 4;
            *word = u32::from_be_bytes([
                chunk[start],
                chunk[start + 1],
                chunk[start + 2],
                chunk[start + 3],
            ]);
        }
        for index in 16..80 {
            words[index] =
                (words[index - 3] ^ words[index - 8] ^ words[index - 14] ^ words[index - 16])
                    .rotate_left(1);
        }

        let [mut a, mut b, mut c, mut d, mut e] = state;
        for (index, word) in words.iter().enumerate() {
            let (function, constant) = match index {
                0..=19 => ((b & c) | ((!b) & d), 0x5a82_7999),
                20..=39 => (b ^ c ^ d, 0x6ed9_eba1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8f1b_bcdc),
                _ => (b ^ c ^ d, 0xca62_c1d6),
            };
            let next = a
                .rotate_left(5)
                .wrapping_add(function)
                .wrapping_add(e)
                .wrapping_add(constant)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = next;
        }
        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
    }

    let mut digest = [0_u8; 20];
    for (index, word) in state.iter().enumerate() {
        digest[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    digest
}

/// Append `no-transform` to every response's cache policy.
///
/// This ports the `preventResponseTransforms` listener boundary in `server.js`. Cloudflare honors
/// the directive for Email Address Obfuscation, so bytes produced by artifact delivery and all
/// other routes remain unchanged in transit.
pub async fn prevent_response_transforms(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    append_no_transform(response.headers_mut());
    response
}

/// Apply the listener's `withNoTransform` behavior to an Axum header map.
pub fn append_no_transform(headers: &mut HeaderMap) {
    let current = headers
        .get(header::CACHE_CONTROL)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if current
        .split(',')
        .map(str::trim)
        .any(|directive| directive.eq_ignore_ascii_case("no-transform"))
    {
        return;
    }
    let next = if current.is_empty() {
        "no-transform".to_owned()
    } else {
        format!("{current}, no-transform")
    };
    if let Ok(value) = HeaderValue::from_str(&next) {
        headers.insert(header::CACHE_CONTROL, value);
    }
}

/// Explicit Axum body limit for `/mcp`.
///
/// Axum defaults body-buffering extractors to 2 MiB. The Node server instead accepts the
/// configured `MCP_JSON_LIMIT` (50,593,792 bytes by default for an 8 MiB bundle), so the MCP route
/// must attach this layer where it is registered.
pub fn mcp_body_limit(config: &AppConfig) -> DefaultBodyLimit {
    configured_body_limit(config.body.mcp_json)
}

/// Convert a validated configuration limit into Axum's platform-sized layer value.
pub fn configured_body_limit(limit: u64) -> DefaultBodyLimit {
    DefaultBodyLimit::max(usize::try_from(limit).unwrap_or(usize::MAX))
}
