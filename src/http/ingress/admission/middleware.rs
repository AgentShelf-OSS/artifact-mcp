use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    Json,
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use serde_json::json;
use sha2::{Digest as _, Sha256};
use tokio::time::timeout;

use crate::config::{BodyLimits, IngressConfig};

use super::state::{IngressState, McpRequestPermit};

pub(super) const VERIFIED_SOURCE_HEADER: HeaderName =
    HeaderName::from_static("x-artifact-ingress-source");

/// Reject malformed or expensive requests before route matching, identity resolution, database
/// checkout, filesystem staging, or renderer scheduling.
pub async fn admit(
    State(state): State<Arc<IngressState>>,
    mut request: Request,
    next: Next,
) -> Response {
    let started = Instant::now();
    let class = classify(request.method(), request.uri().path());
    if let Some(response) = reject_request_shape(&state, &request, class) {
        return response;
    }
    if let Some(response) = admit_source(&state, &mut request, class) {
        return response;
    }
    let Some(request_permit) = state.requests.clone().try_acquire_owned().ok() else {
        return state.reject_for_class(
            class,
            "request_concurrency",
            StatusCode::SERVICE_UNAVAILABLE,
            Some(1),
        );
    };
    let mutation_permit = acquire_mutation_permit(&state, class);
    let Some(mutation_permit) = mutation_permit else {
        return state.reject_for_class(
            class,
            "mutation_concurrency",
            StatusCode::SERVICE_UNAVAILABLE,
            Some(1),
        );
    };
    run_admitted_request(
        state,
        request,
        next,
        class,
        started,
        request_permit,
        mutation_permit,
    )
    .await
}

fn reject_request_shape(
    state: &IngressState,
    request: &Request,
    class: RequestClass,
) -> Option<Response> {
    if request
        .uri()
        .path_and_query()
        .is_some_and(|uri| uri.as_str().len() > as_usize(state.config.max_uri_bytes))
    {
        return Some(state.reject_for_class(class, "uri_too_long", StatusCode::URI_TOO_LONG, None));
    }
    if request.headers().len() > as_usize(state.config.max_headers)
        || header_bytes(request.headers()) > as_usize(state.config.max_header_bytes)
    {
        return Some(state.reject_for_class(
            class,
            "headers_too_large",
            StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
            None,
        ));
    }
    if request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().starts_with("multipart/"))
    {
        return Some(state.reject_for_class(
            class,
            "multipart_unsupported",
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            None,
        ));
    }
    let body_limit = body_limit(&state.body, class);
    // `/mcp` authenticates before it buffers or rejects a body: an unauthenticated oversized
    // request must remain a 401, matching the established Node wire contract.  Its route then
    // drains under the same absolute deadline before returning either the 401 or a 413.  Every
    // other bounded JSON endpoint can safely reject its declared length here.
    if class != RequestClass::Mcp
        && request
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .is_some_and(|length| length > body_limit)
    {
        return Some(state.reject_for_class(
            class,
            "body_too_large",
            StatusCode::PAYLOAD_TOO_LARGE,
            None,
        ));
    }
    None
}

fn admit_source(
    state: &IngressState,
    request: &mut Request,
    class: RequestClass,
) -> Option<Response> {
    let source = trusted_source(request.headers(), peer_addr(request), &state.config);
    let rate_limit = rate_limit(&state.config, class);
    if class != RequestClass::PushMutation
        && !state.allow(class.label(), rate_limit, source_fingerprint(&source))
    {
        // Audit reads are throttled here, before their route can authenticate a capability or
        // record route-local telemetry. Keep this signal fixed-name and route-specific so the
        // sustained-rate-limit alert covers the live audit surface without turning scope
        // denials (which do reach the route) into rate-limit events.
        if is_audit_route(request.uri().path()) {
            crate::observability::record_global_security_signal("rate_limit");
        }
        return Some(state.reject_for_class(
            class,
            "rate_limited",
            StatusCode::TOO_MANY_REQUESTS,
            Some(state.config.rate_window_seconds),
        ));
    }
    // This value is calculated from the socket peer/proxy policy and overwrites any client
    // attempt to supply the same internal header. Later route extractors only retain HeaderMap,
    // so this is the safe hand-off for post-auth principal budgets.
    if let Ok(value) = HeaderValue::from_str(&source_fingerprint(&source)) {
        request.headers_mut().insert(VERIFIED_SOURCE_HEADER, value);
    }
    None
}

fn acquire_mutation_permit(
    state: &IngressState,
    class: RequestClass,
) -> Option<Option<tokio::sync::OwnedSemaphorePermit>> {
    Some(if class.is_mutation() {
        match state.mutations.clone().try_acquire_owned() {
            Ok(permit) => Some(permit),
            Err(_) => return None,
        }
    } else {
        None
    })
}

async fn run_admitted_request(
    state: Arc<IngressState>,
    mut request: Request,
    next: Next,
    class: RequestClass,
    started: Instant,
    request_permit: tokio::sync::OwnedSemaphorePermit,
    mutation_permit: Option<tokio::sync::OwnedSemaphorePermit>,
) -> Response {
    if class == RequestClass::Mcp {
        // Parsed MCP dispatch decides whether an operation is a cancellable read or a durable
        // mutation. Keep the whole-request permit attached to that route until dispatch really
        // completes; a read response timeout must not make more work admissible early.
        request
            .extensions_mut()
            .insert(McpRequestPermit::new(request_permit));
        let response = next.run(request).await;
        state
            .metrics
            .record_latency(class.label(), started.elapsed());
        drop(mutation_permit);
        return response;
    }
    if !class.is_mutation() && class != RequestClass::Mcp {
        // A read-only handler can time out at the client boundary. Its task retains the admission
        // permit until it actually resolves, so an uncancellable blocking dependency cannot turn
        // the timeout into extra concurrent work. Durable mutation routes never take this branch.
        let (send, receive) = tokio::sync::oneshot::channel();
        let running_state = Arc::clone(&state);
        tokio::spawn(async move {
            let response = next.run(request).await;
            running_state
                .metrics
                .record_latency(class.label(), started.elapsed());
            let _ = send.send(response);
            drop(request_permit);
        });
        return match timeout(
            Duration::from_millis(state.config.read_handler_timeout_ms),
            receive,
        )
        .await
        {
            Ok(Ok(response)) => response,
            Ok(Err(_)) => state.reject_for_class(
                class,
                "handler_failed",
                StatusCode::SERVICE_UNAVAILABLE,
                Some(1),
            ),
            Err(_) => state.reject_for_class(
                class,
                "read_handler_timeout",
                StatusCode::REQUEST_TIMEOUT,
                None,
            ),
        };
    }
    // Mutation permits remain held until the route itself resolves; an HTTP deadline cannot make
    // an already-started SQLite/filesystem operation un-happen.
    let response = next.run(request).await;
    state
        .metrics
        .record_latency(class.label(), started.elapsed());
    drop(mutation_permit);
    drop(request_permit);
    response
}

impl IngressState {
    fn reject_for_class(
        &self,
        class: RequestClass,
        reason: &'static str,
        status: StatusCode,
        retry_after: Option<u64>,
    ) -> Response {
        self.metrics.record(reason);
        self.metrics.record_class(class.label());
        if class == RequestClass::Mcp {
            return mcp_admission_response(status, retry_after);
        }
        self.reject_unrecorded(status, retry_after)
    }

    pub(super) fn reject(
        &self,
        reason: &'static str,
        status: StatusCode,
        retry_after: Option<u64>,
    ) -> Response {
        self.metrics.record(reason);
        self.reject_unrecorded(status, retry_after)
    }

    fn reject_unrecorded(&self, status: StatusCode, retry_after: Option<u64>) -> Response {
        let code = match status {
            StatusCode::REQUEST_TIMEOUT => "request_timeout",
            StatusCode::PAYLOAD_TOO_LARGE => "payload_too_large",
            StatusCode::URI_TOO_LONG => "uri_too_long",
            StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE => "headers_too_large",
            StatusCode::TOO_MANY_REQUESTS => "rate_limited",
            StatusCode::SERVICE_UNAVAILABLE => "busy",
            StatusCode::UNSUPPORTED_MEDIA_TYPE => "unsupported_media_type",
            _ => "request_rejected",
        };
        let mut response = (
            status,
            Json(IngressError {
                error: status_message(status),
                code,
            }),
        )
            .into_response();
        if let Some(seconds) = retry_after
            && let Ok(value) = HeaderValue::from_str(&seconds.clamp(1, 3_600).to_string())
        {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        response
    }
}

/// JSON-RPC's direct HTTP transport uses a JSON-RPC error envelope for listener/admission
/// failures. The frozen Node HTTP contract keeps malformed and oversized request bodies as REST
/// envelopes, however, rather than JSON-RPC transport errors.
pub fn mcp_admission_response(status: StatusCode, retry_after: Option<u64>) -> Response {
    if matches!(
        status,
        StatusCode::BAD_REQUEST | StatusCode::PAYLOAD_TOO_LARGE
    ) {
        let error = if status == StatusCode::BAD_REQUEST {
            "invalid JSON"
        } else {
            "payload too large"
        };
        return (status, Json(json!({ "error": error }))).into_response();
    }
    let (code, message, reason) = match status {
        StatusCode::REQUEST_TIMEOUT => (-32_000, "Request timed out", "request_timeout"),
        StatusCode::PAYLOAD_TOO_LARGE => {
            (-32_000, "Request payload too large", "payload_too_large")
        }
        StatusCode::URI_TOO_LONG => (-32_000, "Request URI too long", "uri_too_long"),
        StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE => {
            (-32_000, "Request headers too large", "headers_too_large")
        }
        StatusCode::UNSUPPORTED_MEDIA_TYPE => {
            (-32_000, "Unsupported media type", "unsupported_media_type")
        }
        StatusCode::TOO_MANY_REQUESTS => (-32_029, "Too many requests", "rate_limited"),
        _ => (-32_003, "Service busy", "busy"),
    };
    let mut response = (
        status,
        Json(json!({
            "jsonrpc": "2.0",
            "id": null,
            "error": { "code": code, "message": message, "data": { "reason": reason } }
        })),
    )
        .into_response();
    if let Some(seconds) = retry_after
        && let Ok(value) = HeaderValue::from_str(&seconds.clamp(1, 3_600).to_string())
    {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response
}

#[derive(Serialize)]
pub(super) struct IngressError {
    pub(super) error: &'static str,
    pub(super) code: &'static str,
}

pub(super) const fn status_message(status: StatusCode) -> &'static str {
    match status {
        StatusCode::REQUEST_TIMEOUT => "request timeout",
        StatusCode::PAYLOAD_TOO_LARGE => "payload too large",
        StatusCode::URI_TOO_LONG => "URI too long",
        StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE => "request headers too large",
        StatusCode::TOO_MANY_REQUESTS => "too many requests",
        StatusCode::SERVICE_UNAVAILABLE => "service busy",
        StatusCode::UNSUPPORTED_MEDIA_TYPE => "unsupported media type",
        _ => "request rejected",
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RequestClass {
    Probe,
    Share,
    Read,
    Data,
    Mutation,
    Feedback,
    State,
    /// ADR-0012 push/reminder mutations. Admitted like a mutation, but the state budget is spent
    /// by the handler after concealment and the disabled check (Node order).
    PushMutation,
    Mcp,
    Admin,
}

impl RequestClass {
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Probe => "probe",
            Self::Share => "share",
            Self::Read => "read",
            Self::Data => "data_read",
            Self::Mutation => "mutation",
            Self::Feedback => "feedback",
            Self::State => "state",
            Self::PushMutation => "push_mutation",
            Self::Mcp => "mcp",
            Self::Admin => "admin",
        }
    }
    pub(super) const fn is_mutation(self) -> bool {
        matches!(
            self,
            Self::Mutation | Self::Feedback | Self::State | Self::PushMutation | Self::Admin
        )
    }
}

fn classify(method: &Method, path: &str) -> RequestClass {
    if path == "/health" {
        return RequestClass::Probe;
    }
    if path.starts_with("/s/") {
        return RequestClass::Share;
    }
    if path.starts_with("/settings") || path.starts_with("/admin") {
        return if matches!(
            *method,
            Method::POST | Method::PUT | Method::PATCH | Method::DELETE
        ) {
            RequestClass::Admin
        } else {
            RequestClass::Read
        };
    }
    if path == "/mcp" {
        return RequestClass::Mcp;
    }
    if *method == Method::POST
        && path.split('/').count() == 4
        && (path.ends_with("/data/query") || path.ends_with("/data/stream"))
    {
        return RequestClass::Data;
    }
    if path.contains("/feedback") {
        return RequestClass::Feedback;
    }
    if path.split('/').nth(2) == Some("state") && matches!(path.split('/').count(), 3 | 4) {
        return RequestClass::State;
    }
    // ADR-0012: push and reminder mutations spend the viewer-state budget in their handlers.
    if matches!(*method, Method::PUT | Method::DELETE) && is_push_mutation_path(path) {
        return RequestClass::PushMutation;
    }
    if matches!(
        *method,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    ) {
        RequestClass::Mutation
    } else {
        RequestClass::Read
    }
}

fn is_push_mutation_path(path: &str) -> bool {
    if path == "/push/subscriptions" {
        return true;
    }
    let parts: Vec<&str> = path.split('/').collect();
    matches!(
        parts.as_slice(),
        ["", _, "push", "optin"] | ["", _, "reminders", _]
    )
}

fn is_audit_route(path: &str) -> bool {
    matches!(path, "/audit/events" | "/audit/export")
}

fn body_limit(config: &BodyLimits, class: RequestClass) -> u64 {
    match class {
        RequestClass::Data => 64 * 1024,
        RequestClass::Mcp => config.mcp_json,
        RequestClass::Admin => config.key_json,
        RequestClass::Mutation | RequestClass::Feedback => config
            .category_json
            .max(config.feedback_json)
            .max(config.reaction_json),
        RequestClass::State | RequestClass::PushMutation => config.state_json,
        _ => 0,
    }
}

fn rate_limit(config: &IngressConfig, class: RequestClass) -> u64 {
    match class {
        RequestClass::Probe => config.reads_per_window.min(30),
        RequestClass::Share => config.shares_per_window,
        RequestClass::Mutation => config.mutations_per_window,
        RequestClass::Feedback => config.feedback_per_window,
        RequestClass::State => config.state_per_window,
        // Not consulted at admission; see `IngressState::allow_state_request`.
        RequestClass::PushMutation => config.state_per_window,
        RequestClass::Mcp => config.mcp_per_window,
        RequestClass::Admin => config.admin_per_window,
        RequestClass::Read | RequestClass::Data => config.reads_per_window,
    }
}

fn header_bytes(headers: &HeaderMap) -> usize {
    headers.iter().fold(0_usize, |total, (name, value)| {
        total
            .saturating_add(name.as_str().len())
            .saturating_add(value.as_bytes().len())
            .saturating_add(4)
    })
}

pub fn peer_addr(request: &Request) -> Option<SocketAddr> {
    request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|value| value.0)
}

pub(super) fn trusted_source(
    headers: &HeaderMap,
    peer: Option<SocketAddr>,
    config: &IngressConfig,
) -> String {
    let peer_ip = peer.map(|address| address.ip());
    let trusted = peer_ip.is_some_and(|address| {
        config
            .trusted_proxy_cidrs
            .iter()
            .any(|cidr| cidr.contains(&address))
    });
    // Never infer a position from X-Forwarded-For: its left-most value is often supplied by the
    // client. A deployment may opt in to the single Cloudflare address header only by listing the
    // Cloudflare/proxy peer CIDR in `TRUSTED_PROXY_CIDRS`.
    let forwarded = trusted
        .then(|| {
            headers
                .get("cf-connecting-ip")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.trim().parse::<IpAddr>().ok())
        })
        .flatten();
    forwarded
        .or(peer_ip)
        .map_or_else(|| "unknown".to_owned(), |address| address.to_string())
}

pub(super) fn source_fingerprint(source: &str) -> String {
    combined_fingerprint(source, "source")
}

pub(super) fn verified_source(headers: &HeaderMap) -> String {
    headers
        .get(&VERIFIED_SOURCE_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|value| value.len() == 24 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .map_or_else(|| source_fingerprint("unknown"), str::to_owned)
}

pub(super) fn combined_fingerprint(source: &str, identity: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(source.as_bytes());
    hasher.update([0]);
    hasher.update(identity.as_bytes());
    let digest = hasher.finalize();
    digest[..12]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(super) fn as_usize(value: u64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX).max(1)
}
