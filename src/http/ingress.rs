//! Origin-side request admission controls.
//!
//! Reverse proxies are useful defence in depth, but this module is deliberately inside the
//! application boundary: direct-origin traffic and proxy drift must not turn into unbounded work.
//! It performs only cheap, request-local work before routing. Authentication and authorization
//! remain in their existing handlers; admission keys use opaque hashes and are never emitted as
//! labels or logs.

use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use axum::{
    Json,
    body::{Body, HttpBody},
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use serde_json::json;
use sha2::{Digest as _, Sha256};
use tokio::{sync::Semaphore, time::timeout};

use crate::{
    config::{AppConfig, BodyLimits, IngressConfig},
    mcp::protocol::OrderedJson,
    model::PublisherIdentity,
};

const MAX_BUCKETS: usize = 8_192;
const VERIFIED_SOURCE_HEADER: HeaderName = HeaderName::from_static("x-artifact-ingress-source");

/// Shared ingress controller installed outside every application route.
#[derive(Clone)]
pub struct IngressState {
    config: Arc<IngressConfig>,
    body: BodyLimits,
    requests: Arc<Semaphore>,
    mutations: Arc<Semaphore>,
    buckets: Arc<Mutex<HashMap<String, Bucket>>>,
    metrics: IngressMetrics,
}

/// Ownership of the listener-wide request permit while a parsed MCP read is executing.
///
/// The outer middleware cannot apply a generic handler timeout to `/mcp`: it does not know
/// whether the parsed operation is a read or a durable mutation.  It therefore transfers this
/// guard to the MCP route, which gives reads a client-facing deadline without releasing
/// admission capacity while their background operation is still running.
#[derive(Clone)]
pub struct McpRequestPermit(Arc<Mutex<Option<tokio::sync::OwnedSemaphorePermit>>>);

impl McpRequestPermit {
    fn new(permit: tokio::sync::OwnedSemaphorePermit) -> Self {
        Self(Arc::new(Mutex::new(Some(permit))))
    }

    /// Take the permit exactly once for the background read task. If a route rejects before it
    /// takes the guard, dropping the request extension releases capacity normally.
    pub(crate) fn take(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }
}

/// A verified MCP operation class. These are deliberately coarse: the operation name is not
/// emitted as a metric label or retained as a raw limiter key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpCost {
    Upload,
    Mutation,
    Read,
}

/// Cost class applied only after the server has resolved a human identity from Access.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewerCost {
    Read,
    Mutation,
    Admin,
}

impl ViewerCost {
    const fn label(self) -> &'static str {
        match self {
            Self::Read => "viewer_read",
            Self::Mutation => "viewer_mutation",
            Self::Admin => "viewer_admin",
        }
    }

    const fn limit(self, config: &IngressConfig) -> u64 {
        let _ = self;
        config.verified_viewers_per_window
    }
}

impl McpCost {
    const fn label(self) -> &'static str {
        match self {
            Self::Upload => "mcp_upload",
            Self::Mutation => "mcp_mutation",
            Self::Read => "mcp_read",
        }
    }

    const fn limit(self, config: &IngressConfig) -> u64 {
        match self {
            Self::Upload => config.uploads_per_window,
            Self::Mutation => config.mutations_per_window,
            Self::Read => config.reads_per_window,
        }
    }
}

#[derive(Clone, Copy)]
struct Bucket {
    last_refill: Instant,
    tokens: f64,
}

/// A low-cardinality counter set. It intentionally has no identity, URI, user, tenant, or key
/// dimension; structured request logs may correlate only through the existing request id.
#[derive(Clone, Debug, Default)]
pub struct IngressMetrics {
    counters: Arc<Mutex<HashMap<&'static str, u64>>>,
    classes: Arc<Mutex<HashMap<&'static str, u64>>>,
    latency: Arc<Mutex<HashMap<&'static str, (u64, u64)>>>,
    render_queue_rejections: Arc<AtomicU64>,
    render_queue_pending: Arc<AtomicU64>,
    render_queue_running: Arc<AtomicU64>,
    render_queue_reserved_bytes: Arc<AtomicU64>,
}

/// Low-cardinality live preview-queue gauges updated by the production queue itself.
#[derive(Clone, Debug, Default)]
pub struct RenderQueuePressure {
    pending: Arc<AtomicU64>,
    running: Arc<AtomicU64>,
    reserved_bytes: Arc<AtomicU64>,
}

impl RenderQueuePressure {
    pub fn update(&self, pending: usize, running: bool, reserved_bytes: u64) {
        self.pending.store(pending as u64, Ordering::Relaxed);
        self.running.store(u64::from(running), Ordering::Relaxed);
        self.reserved_bytes.store(reserved_bytes, Ordering::Relaxed);
    }
}

impl IngressMetrics {
    fn record(&self, reason: &'static str) {
        let mut counters = self
            .counters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let value = counters.entry(reason).or_default();
        *value = value.saturating_add(1);
    }

    fn record_class(&self, class: &'static str) {
        let mut classes = self
            .classes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *classes.entry(class).or_default() += 1;
    }

    fn record_latency(&self, class: &'static str, elapsed: Duration) {
        let mut latency = self
            .latency
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let row = latency.entry(class).or_default();
        row.0 = row.0.saturating_add(1);
        row.1 = row.1.saturating_add(elapsed.as_millis() as u64);
    }

    /// Prometheus exposition appended to the existing `/metrics` endpoint.
    #[must_use]
    pub fn render_prometheus(&self) -> String {
        let counters = self
            .counters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut rows = counters.iter().collect::<Vec<_>>();
        rows.sort_by_key(|(reason, _)| **reason);
        let mut output = String::from(
            "# HELP artifact_mcp_ingress_rejections_total Origin admission rejections by bounded reason.\n\\
             # TYPE artifact_mcp_ingress_rejections_total counter\n",
        );
        for (reason, count) in rows {
            output.push_str(&format!(
                "artifact_mcp_ingress_rejections_total{{reason=\"{reason}\"}} {count}\n"
            ));
        }
        let classes = self
            .classes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        output.push_str("# HELP artifact_mcp_ingress_rejections_by_class_total Rejections by fixed principal/request class.\n# TYPE artifact_mcp_ingress_rejections_by_class_total counter\n");
        for (class, count) in classes.iter() {
            output.push_str(&format!(
                "artifact_mcp_ingress_rejections_by_class_total{{class=\"{class}\"}} {count}\n"
            ));
        }
        let latency = self
            .latency
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        output.push_str("# HELP artifact_mcp_ingress_handler_duration_milliseconds Handler duration by fixed class.\n# TYPE artifact_mcp_ingress_handler_duration_milliseconds summary\n");
        for (class, (count, sum)) in latency.iter() {
            output.push_str(&format!("artifact_mcp_ingress_handler_duration_milliseconds_count{{class=\"{class}\"}} {count}\nartifact_mcp_ingress_handler_duration_milliseconds_sum{{class=\"{class}\"}} {sum}\n"));
        }
        output.push_str("# HELP artifact_mcp_render_queue_rejections_total Preview jobs rejected by the bounded queue.\n\
             # TYPE artifact_mcp_render_queue_rejections_total counter\n");
        output.push_str(&format!(
            "artifact_mcp_render_queue_rejections_total {}\n",
            self.render_queue_rejections.load(Ordering::Relaxed)
        ));
        output.push_str("# HELP artifact_mcp_render_queue_pending Current queued preview jobs.\n# TYPE artifact_mcp_render_queue_pending gauge\n");
        output.push_str(&format!(
            "artifact_mcp_render_queue_pending {}\n",
            self.render_queue_pending.load(Ordering::Relaxed)
        ));
        output.push_str("# HELP artifact_mcp_render_queue_running Whether the preview lane is active.\n# TYPE artifact_mcp_render_queue_running gauge\n");
        output.push_str(&format!(
            "artifact_mcp_render_queue_running {}\n",
            self.render_queue_running.load(Ordering::Relaxed)
        ));
        output.push_str("# HELP artifact_mcp_render_queue_reserved_bytes Declared bytes reserved by active and queued previews.\n# TYPE artifact_mcp_render_queue_reserved_bytes gauge\n");
        output.push_str(&format!(
            "artifact_mcp_render_queue_reserved_bytes {}\n",
            self.render_queue_reserved_bytes.load(Ordering::Relaxed)
        ));
        output
    }
}

impl IngressState {
    #[must_use]
    pub fn from_config(config: &AppConfig) -> Self {
        let limits = Arc::new(config.ingress.clone());
        Self {
            requests: Arc::new(Semaphore::new(as_usize(limits.max_concurrent_requests))),
            mutations: Arc::new(Semaphore::new(as_usize(limits.max_concurrent_mutations))),
            config: limits,
            body: config.body,
            buckets: Arc::new(Mutex::new(HashMap::new())),
            metrics: IngressMetrics::default(),
        }
    }

    #[must_use]
    pub const fn metrics(&self) -> &IngressMetrics {
        &self.metrics
    }

    /// Origin admission metrics with only fixed low-cardinality dimensions and pool gauges.
    #[must_use]
    pub fn render_prometheus(&self) -> String {
        let mut output = self.metrics.render_prometheus();
        output.push_str("# HELP artifact_mcp_ingress_request_permits_available Available whole-request admission permits.\n# TYPE artifact_mcp_ingress_request_permits_available gauge\n");
        output.push_str(&format!(
            "artifact_mcp_ingress_request_permits_available {}\n",
            self.requests.available_permits()
        ));
        output.push_str("# HELP artifact_mcp_ingress_mutation_permits_available Available mutation admission permits.\n# TYPE artifact_mcp_ingress_mutation_permits_available gauge\n");
        output.push_str(&format!(
            "artifact_mcp_ingress_mutation_permits_available {}\n",
            self.mutations.available_permits()
        ));
        output
    }

    /// Counter passed to the concrete preview queue at bootstrap. It contains no request-derived
    /// label and is deliberately shared instead of making the renderer depend on HTTP state.
    #[must_use]
    pub fn preview_queue_rejection_counter(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.metrics.render_queue_rejections)
    }

    #[must_use]
    pub fn preview_queue_pressure(&self) -> RenderQueuePressure {
        RenderQueuePressure {
            pending: Arc::clone(&self.metrics.render_queue_pending),
            running: Arc::clone(&self.metrics.render_queue_running),
            reserved_bytes: Arc::clone(&self.metrics.render_queue_reserved_bytes),
        }
    }

    /// Record an invalid credential outcome after the route authenticator has checked it.
    ///
    /// This is deliberately source-only. An unverified candidate credential must never become a
    /// limiter key: rotating bad keys would otherwise bypass the budget and fill the bucket map.
    pub fn allow_auth_failure(&self, headers: &HeaderMap, peer: Option<SocketAddr>) -> bool {
        let source = trusted_source(headers, peer, &self.config);
        self.allow(
            "auth_failure",
            self.config.auth_failures_per_window,
            source_fingerprint(&source),
        )
    }

    /// Preserve the listener-derived peer address while `/mcp` still owns the request body.
    pub fn allow_auth_failure_request(&self, request: &Request) -> bool {
        self.allow_auth_failure(request.headers(), peer_addr(request))
    }

    /// A separate per-publisher budget installed only after the authenticator has returned a
    /// verified identity. Raw bearer values never become map keys or metric labels.
    pub fn allow_verified_publisher_request(
        &self,
        request: &Request,
        publisher: &PublisherIdentity,
    ) -> bool {
        let source = trusted_source(request.headers(), peer_addr(request), &self.config);
        let allowed = self.allow(
            "verified_publisher",
            self.config.mutations_per_window,
            combined_fingerprint(
                &source,
                &format!("{}:{}", publisher.org.0, publisher.client_id.0),
            ),
        );
        if !allowed {
            self.metrics.record_class("publisher");
            self.metrics.record("rate_limited");
        }
        allowed
    }

    /// Apply the more restrictive operation budget after JSON-RPC parsing has revealed the
    /// requested tool. This remains safe because parsing is already bounded by the transport
    /// body and complexity limits above.
    pub fn allow_verified_publisher_operation(
        &self,
        headers: &HeaderMap,
        peer: Option<SocketAddr>,
        publisher: &PublisherIdentity,
        cost: McpCost,
    ) -> bool {
        let source = trusted_source(headers, peer, &self.config);
        let allowed = self.allow(
            cost.label(),
            cost.limit(&self.config),
            combined_fingerprint(
                &source,
                &format!("{}:{}", publisher.org.0, publisher.client_id.0),
            ),
        );
        if !allowed {
            self.metrics.record_class("publisher_operation");
            self.metrics.record("rate_limited");
        }
        allowed
    }

    pub fn allow_verified_publisher_operation_weight(
        &self,
        headers: &HeaderMap,
        peer: Option<SocketAddr>,
        publisher: &PublisherIdentity,
        cost: McpCost,
        weight: u64,
    ) -> bool {
        let source = trusted_source(headers, peer, &self.config);
        let allowed = self.allow_weight(
            cost.label(),
            cost.limit(&self.config),
            weight,
            combined_fingerprint(
                &source,
                &format!("{}:{}", publisher.org.0, publisher.client_id.0),
            ),
        );
        if !allowed {
            self.metrics.record_class("publisher_operation");
            self.metrics.record("rate_limited");
        }
        allowed
    }

    pub fn try_acquire_mutation(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        self.mutations.clone().try_acquire_owned().ok()
    }

    /// Apply a post-Access budget using only the resolved tenant/email identity plus the source
    /// fingerprint installed by this middleware. Caller-controlled cookies and headers are never
    /// limiter keys.
    pub fn allow_verified_viewer(
        &self,
        headers: &HeaderMap,
        viewer: &crate::model::Viewer,
        cost: ViewerCost,
    ) -> bool {
        let Some(email) = viewer.email.as_ref().filter(|email| !email.0.is_empty()) else {
            return true;
        };
        let source = verified_source(headers);
        let tenant = viewer.org.as_ref().map_or("", |org| org.0.as_str());
        let allowed = self.allow(
            cost.label(),
            cost.limit(&self.config),
            combined_fingerprint(
                &source,
                &format!(
                    "{}:{}",
                    tenant.to_ascii_lowercase(),
                    email.0.to_ascii_lowercase()
                ),
            ),
        );
        if !allowed {
            self.metrics.record_class("viewer");
            self.metrics.record("rate_limited");
        }
        allowed
    }

    /// A public share becomes a limiter principal only after resolution and authorization prove
    /// that it exists and is live. Invalid candidates remain in the source-only pre-auth bucket.
    pub fn allow_verified_share(
        &self,
        headers: &HeaderMap,
        token: &crate::model::ShareToken,
    ) -> bool {
        let allowed = self.allow(
            "verified_share",
            self.config.shares_per_window,
            combined_fingerprint(&verified_source(headers), &token.0),
        );
        if !allowed {
            self.metrics.record_class("share");
            self.metrics.record("rate_limited");
        }
        allowed
    }

    /// The same non-oracular response used by ordinary admission rate limits.
    pub fn rate_limited_response(&self) -> Response {
        self.reject(
            "auth_failure_rate_limited",
            StatusCode::TOO_MANY_REQUESTS,
            Some(self.config.rate_window_seconds),
        )
    }

    /// JSON-RPC transport variant for `/mcp`.
    pub fn mcp_rate_limited_response(&self) -> Response {
        self.metrics.record("auth_failure_rate_limited");
        mcp_admission_response(
            StatusCode::TOO_MANY_REQUESTS,
            Some(self.config.rate_window_seconds),
        )
    }

    /// JSON-RPC overload response for a verified operation budget. The response deliberately
    /// remains indistinguishable from any other transport quota to callers.
    pub fn mcp_operation_rate_limited_response(&self) -> Response {
        self.metrics.record("mcp_operation_rate_limited");
        mcp_admission_response(
            StatusCode::TOO_MANY_REQUESTS,
            Some(self.config.rate_window_seconds),
        )
    }

    fn allow(&self, class: &'static str, limit: u64, identity: String) -> bool {
        self.allow_weight(class, limit, 1, identity)
    }

    fn allow_weight(&self, class: &'static str, limit: u64, weight: u64, identity: String) -> bool {
        let now = Instant::now();
        let window = Duration::from_secs(self.config.rate_window_seconds);
        let key = format!("{class}:{identity}");
        let mut buckets = self
            .buckets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if buckets.len() >= MAX_BUCKETS && !buckets.contains_key(&key) {
            // Evict one opaque entry in O(1); scanning all buckets under the request mutex would
            // itself become an attack multiplier, while refusing every new source is global DoS.
            if let Some(evicted) = buckets.keys().next().cloned() {
                buckets.remove(&evicted);
            }
        }
        let capacity = limit as f64;
        let bucket = buckets.entry(key).or_insert(Bucket {
            last_refill: now,
            tokens: capacity,
        });
        // A token bucket avoids the boundary burst inherent in a fixed window while retaining a
        // strict, configurable burst ceiling. `rate_window_seconds` is parsed as positive.
        let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
        let refill_per_second = capacity / window.as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * refill_per_second).min(capacity);
        bucket.last_refill = now;
        if bucket.tokens < weight as f64 {
            return false;
        }
        bucket.tokens -= weight as f64;
        true
    }
}

/// Reject malformed or expensive requests before route matching, identity resolution, database
/// checkout, filesystem staging, or renderer scheduling.
pub async fn admit(
    State(state): State<Arc<IngressState>>,
    mut request: Request,
    next: Next,
) -> Response {
    let started = Instant::now();
    let class = classify(request.method(), request.uri().path());
    if request
        .uri()
        .path_and_query()
        .is_some_and(|uri| uri.as_str().len() > as_usize(state.config.max_uri_bytes))
    {
        return state.reject_for_class(class, "uri_too_long", StatusCode::URI_TOO_LONG, None);
    }
    if request.headers().len() > as_usize(state.config.max_headers)
        || header_bytes(request.headers()) > as_usize(state.config.max_header_bytes)
    {
        return state.reject_for_class(
            class,
            "headers_too_large",
            StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
            None,
        );
    }
    if request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().starts_with("multipart/"))
    {
        return state.reject_for_class(
            class,
            "multipart_unsupported",
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            None,
        );
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
        return state.reject_for_class(
            class,
            "body_too_large",
            StatusCode::PAYLOAD_TOO_LARGE,
            None,
        );
    }

    let source = trusted_source(request.headers(), peer_addr(&request), &state.config);
    let rate_limit = rate_limit(&state.config, class);
    if !state.allow(class.label(), rate_limit, source_fingerprint(&source)) {
        // Audit reads are throttled here, before their route can authenticate a capability or
        // record route-local telemetry. Keep this signal fixed-name and route-specific so the
        // sustained-rate-limit alert covers the live audit surface without turning scope
        // denials (which do reach the route) into rate-limit events.
        if is_audit_route(request.uri().path()) {
            crate::observability::record_global_security_signal("rate_limit");
        }
        return state.reject_for_class(
            class,
            "rate_limited",
            StatusCode::TOO_MANY_REQUESTS,
            Some(state.config.rate_window_seconds),
        );
    }
    // This value is calculated from the socket peer/proxy policy and overwrites any client
    // attempt to supply the same internal header. Later route extractors only retain HeaderMap,
    // so this is the safe hand-off for post-auth principal budgets.
    if let Ok(value) = HeaderValue::from_str(&source_fingerprint(&source)) {
        request.headers_mut().insert(VERIFIED_SOURCE_HEADER, value);
    }

    let Some(request_permit) = state.requests.clone().try_acquire_owned().ok() else {
        return state.reject_for_class(
            class,
            "request_concurrency",
            StatusCode::SERVICE_UNAVAILABLE,
            Some(1),
        );
    };
    let mutation_permit = if class.is_mutation() {
        match state.mutations.clone().try_acquire_owned() {
            Ok(permit) => Some(permit),
            Err(_) => {
                return state.reject_for_class(
                    class,
                    "mutation_concurrency",
                    StatusCode::SERVICE_UNAVAILABLE,
                    Some(1),
                );
            }
        }
    } else {
        None
    };
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

    fn reject(
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
struct IngressError {
    error: &'static str,
    code: &'static str,
}

const fn status_message(status: StatusCode) -> &'static str {
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
enum RequestClass {
    Probe,
    Share,
    Read,
    Mutation,
    Feedback,
    Mcp,
    Admin,
}

impl RequestClass {
    const fn label(self) -> &'static str {
        match self {
            Self::Probe => "probe",
            Self::Share => "share",
            Self::Read => "read",
            Self::Mutation => "mutation",
            Self::Feedback => "feedback",
            Self::Mcp => "mcp",
            Self::Admin => "admin",
        }
    }
    const fn is_mutation(self) -> bool {
        matches!(self, Self::Mutation | Self::Feedback | Self::Admin)
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
    if path.contains("/feedback") {
        return RequestClass::Feedback;
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

fn is_audit_route(path: &str) -> bool {
    matches!(path, "/audit/events" | "/audit/export")
}

fn body_limit(config: &BodyLimits, class: RequestClass) -> u64 {
    match class {
        RequestClass::Mcp => config.mcp_json,
        RequestClass::Admin => config.key_json,
        RequestClass::Mutation | RequestClass::Feedback => config
            .category_json
            .max(config.feedback_json)
            .max(config.reaction_json),
        _ => 0,
    }
}

fn rate_limit(config: &IngressConfig, class: RequestClass) -> u64 {
    match class {
        RequestClass::Probe => config.reads_per_window.min(30),
        RequestClass::Share => config.shares_per_window,
        RequestClass::Mutation => config.mutations_per_window,
        RequestClass::Feedback => config.feedback_per_window,
        RequestClass::Mcp => config.mcp_per_window,
        RequestClass::Admin => config.admin_per_window,
        RequestClass::Read => config.reads_per_window,
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

fn trusted_source(headers: &HeaderMap, peer: Option<SocketAddr>, config: &IngressConfig) -> String {
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

fn source_fingerprint(source: &str) -> String {
    combined_fingerprint(source, "source")
}

fn verified_source(headers: &HeaderMap) -> String {
    headers
        .get(&VERIFIED_SOURCE_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|value| value.len() == 24 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .map_or_else(|| source_fingerprint("unknown"), str::to_owned)
}

fn combined_fingerprint(source: &str, identity: &str) -> String {
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

fn as_usize(value: u64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX).max(1)
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        body::Body,
        http::{Request, StatusCode},
        middleware,
        routing::get,
    };
    use std::time::Duration;
    use std::{
        collections::VecDeque,
        convert::Infallible,
        task::{Context, Poll},
    };
    use tokio::sync::Notify;

    struct ChunkedBody {
        chunks: VecDeque<axum::body::Bytes>,
    }

    impl http_body::Body for ChunkedBody {
        type Data = axum::body::Bytes;
        type Error = Infallible;

        fn poll_frame(
            mut self: std::pin::Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
            Poll::Ready(
                self.chunks
                    .pop_front()
                    .map(|chunk| Ok(http_body::Frame::data(chunk))),
            )
        }
    }

    struct StalledBody;

    impl http_body::Body for StalledBody {
        type Data = axum::body::Bytes;
        type Error = Infallible;

        fn poll_frame(
            self: std::pin::Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
            let _ = self;
            Poll::Pending
        }
    }

    struct SlowDripBody {
        chunks: VecDeque<axum::body::Bytes>,
        delay: Duration,
        wait: Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
    }

    impl http_body::Body for SlowDripBody {
        type Data = axum::body::Bytes;
        type Error = Infallible;

        fn poll_frame(
            mut self: std::pin::Pin<&mut Self>,
            context: &mut Context<'_>,
        ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
            if self.chunks.is_empty() {
                return Poll::Ready(None);
            }
            if self.wait.is_none() {
                let delay = self.delay;
                self.wait = Some(Box::pin(tokio::time::sleep(delay)));
            }
            if self
                .wait
                .as_mut()
                .expect("delay initialized")
                .as_mut()
                .poll(context)
                .is_pending()
            {
                return Poll::Pending;
            }
            self.wait = None;
            Poll::Ready(
                self.chunks
                    .pop_front()
                    .map(|chunk| Ok(http_body::Frame::data(chunk))),
            )
        }
    }
    use tower::ServiceExt as _;

    fn router(state: Arc<IngressState>) -> Router {
        Router::new()
            .route("/ok", get(|| async { "ok" }))
            .layer(middleware::from_fn_with_state(state, admit))
    }

    #[tokio::test]
    async fn header_and_uri_limits_are_rejected_before_the_handler() {
        let mut config = AppConfig::default();
        config.ingress.max_headers = 1;
        config.ingress.max_uri_bytes = 2;
        let response = router(Arc::new(IngressState::from_config(&config)))
            .oneshot(
                Request::builder()
                    .uri("/ok")
                    .header("x-one", "1")
                    .header("x-two", "2")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::URI_TOO_LONG);
    }

    #[tokio::test]
    async fn share_brute_force_and_bad_key_buckets_are_bounded_without_identity_labels() {
        let mut config = AppConfig::default();
        config.ingress.shares_per_window = 1;
        config.ingress.auth_failures_per_window = 1;
        let state = Arc::new(IngressState::from_config(&config));
        let app = Router::new()
            .route("/s/{token}", get(|| async { "missing" }))
            .layer(middleware::from_fn_with_state(Arc::clone(&state), admit));
        let first = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/s/not-a-share")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let second = app
            .oneshot(
                Request::builder()
                    .uri("/s/not-a-share")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer candidate"),
        );
        assert!(state.allow_auth_failure(&headers, None));
        assert!(!state.allow_auth_failure(&headers, None));
        assert!(!state.metrics().render_prometheus().contains("candidate"));
    }

    #[test]
    fn rotating_bad_credentials_cannot_rotate_the_source_budget() {
        let mut config = AppConfig::default();
        config.ingress.auth_failures_per_window = 1;
        let state = IngressState::from_config(&config);
        let mut first = HeaderMap::new();
        first.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer first"),
        );
        let mut rotated = HeaderMap::new();
        rotated.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer second"),
        );
        let peer = Some("192.0.2.4:4444".parse().unwrap());
        assert!(state.allow_auth_failure(&first, peer));
        assert!(!state.allow_auth_failure(&rotated, peer));
    }

    #[test]
    fn weighted_mcp_reservations_are_atomic_when_a_batch_exceeds_remaining_budget() {
        let mut config = AppConfig::default();
        config.ingress.reads_per_window = 3;
        let state = IngressState::from_config(&config);
        let publisher = PublisherIdentity {
            client_id: crate::model::ClientId::from("publisher"),
            org: crate::model::OrgId::from("acme"),
            label: "Publisher".to_owned(),
            role: "writer".to_owned(),
            scopes: None,
        };
        let headers = HeaderMap::new();

        assert!(state.allow_verified_publisher_operation_weight(
            &headers,
            None,
            &publisher,
            McpCost::Read,
            2,
        ));
        assert!(
            !state.allow_verified_publisher_operation_weight(
                &headers,
                None,
                &publisher,
                McpCost::Read,
                2,
            ),
            "the second batch is rejected as one reservation"
        );
        assert!(
            state.allow_verified_publisher_operation(&headers, None, &publisher, McpCost::Read),
            "a rejected weight-two batch must not consume the one token still available"
        );
    }

    #[test]
    fn verified_upload_budget_is_scoped_to_the_verified_publisher_and_source() {
        let mut config = AppConfig::default();
        config.ingress.uploads_per_window = 1;
        let state = IngressState::from_config(&config);
        let publisher = PublisherIdentity {
            client_id: crate::model::ClientId("client-a".to_owned()),
            org: crate::model::OrgId("org-a".to_owned()),
            label: "test".to_owned(),
            role: "author".to_owned(),
            scopes: None,
        };
        let headers = HeaderMap::new();
        let peer = Some("192.0.2.4:4444".parse().unwrap());
        assert!(state.allow_verified_publisher_operation(
            &headers,
            peer,
            &publisher,
            McpCost::Upload,
        ));
        assert!(!state.allow_verified_publisher_operation(
            &headers,
            peer,
            &publisher,
            McpCost::Upload,
        ));
    }

    #[test]
    fn verified_viewers_share_a_nat_but_not_each_others_budget_or_raw_headers() {
        let mut config = AppConfig::default();
        config.ingress.verified_viewers_per_window = 1;
        let state = IngressState::from_config(&config);
        let mut first_headers = HeaderMap::new();
        first_headers.insert(
            VERIFIED_SOURCE_HEADER,
            HeaderValue::from_static("0123456789abcdef01234567"),
        );
        first_headers.insert(header::COOKIE, HeaderValue::from_static("session=one"));
        let mut alternate_headers = first_headers.clone();
        alternate_headers.insert(header::COOKIE, HeaderValue::from_static("session=rotated"));
        let alice = crate::model::Viewer {
            email: Some(crate::model::EmailAddress("Alice@Example.test".to_owned())),
            org: Some(crate::model::OrgId("acme".to_owned())),
            is_admin: false,
        };
        let bob = crate::model::Viewer {
            email: Some(crate::model::EmailAddress("bob@example.test".to_owned())),
            org: Some(crate::model::OrgId("acme".to_owned())),
            is_admin: false,
        };
        assert!(state.allow_verified_viewer(&first_headers, &alice, ViewerCost::Read));
        assert!(!state.allow_verified_viewer(&alternate_headers, &alice, ViewerCost::Read));
        assert!(state.allow_verified_viewer(&alternate_headers, &bob, ViewerCost::Read));
    }

    #[test]
    fn verified_share_budget_uses_the_canonical_resolved_token() {
        let mut config = AppConfig::default();
        config.ingress.shares_per_window = 1;
        let state = IngressState::from_config(&config);
        let mut headers = HeaderMap::new();
        headers.insert(
            VERIFIED_SOURCE_HEADER,
            HeaderValue::from_static("0123456789abcdef01234567"),
        );
        let token = crate::model::ShareToken("sharetoken".to_owned());
        assert!(state.allow_verified_share(&headers, &token));
        assert!(!state.allow_verified_share(&headers, &token));
    }

    #[test]
    fn high_cardinality_sources_evict_in_constant_bounded_state() {
        let config = AppConfig::default();
        let state = IngressState::from_config(&config);
        for index in 0..(MAX_BUCKETS + 128) {
            assert!(state.allow("load", 1, format!("source-{index}")));
        }
        assert!(
            state
                .buckets
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len()
                <= MAX_BUCKETS
        );
    }

    #[tokio::test]
    async fn mcp_admission_rejections_keep_the_frozen_transport_envelopes() {
        let response = mcp_admission_response(StatusCode::TOO_MANY_REQUESTS, Some(60));
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()[header::RETRY_AFTER], "60");
        let bytes = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["jsonrpc"], "2.0");
        assert!(json["id"].is_null());
        assert_eq!(json["error"]["data"]["reason"], "rate_limited");

        let oversized = mcp_admission_response(StatusCode::PAYLOAD_TOO_LARGE, None);
        let bytes = axum::body::to_bytes(oversized.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(bytes.as_ref(), br#"{"error":"payload too large"}"#);

        let unsupported = mcp_admission_response(StatusCode::UNSUPPORTED_MEDIA_TYPE, None);
        let bytes = axum::body::to_bytes(unsupported.into_body(), 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["error"]["data"]["reason"], "unsupported_media_type");

        let malformed = mcp_admission_response(StatusCode::BAD_REQUEST, None);
        let bytes = axum::body::to_bytes(malformed.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(bytes.as_ref(), br#"{"error":"invalid JSON"}"#);
    }

    #[tokio::test]
    async fn concurrency_rejects_pressure_and_recovers_after_a_request_completes() {
        let mut config = AppConfig::default();
        config.ingress.max_concurrent_requests = 1;
        let state = Arc::new(IngressState::from_config(&config));
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let first_call = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let app = Router::new()
            .route(
                "/slow",
                get({
                    let started = Arc::clone(&started);
                    let release = Arc::clone(&release);
                    let first_call = Arc::clone(&first_call);
                    move || {
                        let started = Arc::clone(&started);
                        let release = Arc::clone(&release);
                        let first_call = Arc::clone(&first_call);
                        async move {
                            if first_call.swap(false, std::sync::atomic::Ordering::SeqCst) {
                                started.notify_one();
                                release.notified().await;
                            }
                            "ok"
                        }
                    }
                }),
            )
            .layer(middleware::from_fn_with_state(state, admit));
        let task = tokio::spawn({
            let app = app.clone();
            async move {
                app.oneshot(Request::builder().uri("/slow").body(Body::empty()).unwrap())
                    .await
                    .unwrap()
            }
        });
        started.notified().await;
        let pressured = app
            .clone()
            .oneshot(Request::builder().uri("/slow").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(pressured.status(), StatusCode::SERVICE_UNAVAILABLE);
        release.notify_one();
        assert_eq!(task.await.unwrap().status(), StatusCode::OK);
        let recovered = app
            .oneshot(Request::builder().uri("/slow").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(recovered.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn chunked_body_limit_and_deadline_are_bounded() {
        let body = Body::new(ChunkedBody {
            chunks: VecDeque::from([
                axum::body::Bytes::from_static(b"12"),
                axum::body::Bytes::from_static(b"345"),
            ]),
        });
        assert_eq!(
            read_body_limited(body, 4, Duration::from_secs(1)).await,
            Err(BodyReadError::TooLarge)
        );
    }

    #[tokio::test]
    async fn discard_only_drain_consumes_oversized_frames_without_a_parser_buffer() {
        let body = Body::new(ChunkedBody {
            chunks: VecDeque::from([
                axum::body::Bytes::from(vec![b'x'; 8 * 1024]),
                axum::body::Bytes::from(vec![b'y'; 8 * 1024]),
            ]),
        });
        assert_eq!(
            discard_body_bounded(body, Duration::from_secs(1)).await,
            Ok(())
        );
    }

    #[tokio::test]
    async fn slow_body_is_timed_out_without_entering_a_handler() {
        let result = read_body_limited(Body::new(StalledBody), 16, Duration::from_millis(1)).await;
        assert_eq!(result, Err(BodyReadError::Timeout));
    }

    #[tokio::test]
    async fn slow_drip_cannot_extend_the_whole_body_deadline_per_frame() {
        let body = Body::new(SlowDripBody {
            chunks: VecDeque::from([
                axum::body::Bytes::from_static(b"a"),
                axum::body::Bytes::from_static(b"b"),
                axum::body::Bytes::from_static(b"c"),
            ]),
            delay: Duration::from_millis(20),
            wait: None,
        });
        assert_eq!(
            read_body_limited(body, 16, Duration::from_millis(35)).await,
            Err(BodyReadError::Timeout)
        );
    }

    #[test]
    fn deep_and_wide_json_are_rejected() {
        let mut config = AppConfig::default();
        config.ingress.json_max_depth = 2;
        config.ingress.json_max_members = 1;
        assert_eq!(
            validate_json_complexity(
                &OrderedJson::Array(vec![OrderedJson::Array(vec![OrderedJson::Null])]),
                &config.ingress
            ),
            Err("json_too_deep")
        );
        assert_eq!(
            validate_json_complexity(
                &OrderedJson::Array(vec![OrderedJson::Null, OrderedJson::Null]),
                &config.ingress
            ),
            Err("json_too_wide")
        );
    }

    #[test]
    fn untrusted_forwarded_headers_do_not_choose_the_source() {
        let config = IngressConfig::default();
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", HeaderValue::from_static("203.0.113.7"));
        assert_eq!(
            trusted_source(&headers, Some("127.0.0.1:1234".parse().unwrap()), &config),
            "127.0.0.1"
        );
    }

    #[test]
    fn only_a_configured_proxy_can_supply_cf_connecting_ip() {
        let config = IngressConfig {
            trusted_proxy_cidrs: vec!["127.0.0.0/8".parse().unwrap()],
            ..IngressConfig::default()
        };
        let mut headers = HeaderMap::new();
        headers.insert("cf-connecting-ip", HeaderValue::from_static("203.0.113.7"));
        headers.insert("x-forwarded-for", HeaderValue::from_static("198.51.100.8"));
        assert_eq!(
            trusted_source(&headers, Some("127.0.0.1:1234".parse().unwrap()), &config),
            "203.0.113.7"
        );
        assert_eq!(
            trusted_source(&headers, Some("192.0.2.9:1234".parse().unwrap()), &config),
            "192.0.2.9"
        );
    }
}
