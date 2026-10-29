use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use axum::{
    extract::Request,
    http::{HeaderMap, StatusCode},
    response::Response,
};
use tokio::sync::Semaphore;

use crate::{
    config::{AppConfig, BodyLimits, IngressConfig},
    model::PublisherIdentity,
};

use super::middleware::{
    RequestClass, as_usize, combined_fingerprint, mcp_admission_response, peer_addr,
    source_fingerprint, trusted_source, verified_source,
};

pub(super) const MAX_BUCKETS: usize = 8_192;

/// Shared ingress controller installed outside every application route.
#[derive(Clone)]
pub struct IngressState {
    pub(super) config: Arc<IngressConfig>,
    pub(super) body: BodyLimits,
    pub(super) requests: Arc<Semaphore>,
    pub(super) mutations: Arc<Semaphore>,
    pub(super) buckets: Arc<Mutex<HashMap<String, Bucket>>>,
    pub(super) metrics: IngressMetrics,
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
    pub(super) fn new(permit: tokio::sync::OwnedSemaphorePermit) -> Self {
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
pub(super) struct Bucket {
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
    pub(super) fn record(&self, reason: &'static str) {
        let mut counters = self
            .counters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let value = counters.entry(reason).or_default();
        *value = value.saturating_add(1);
    }

    pub(super) fn record_class(&self, class: &'static str) {
        let mut classes = self
            .classes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *classes.entry(class).or_default() += 1;
    }

    pub(super) fn record_latency(&self, class: &'static str, elapsed: Duration) {
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

    /// Spend one token of the viewer-state budget (`INGRESS_STATE_PER_WINDOW`) for the source this
    /// middleware verified. Shares the bucket that admission uses for `/{id}/state` routes.
    pub fn allow_state_request(&self, headers: &HeaderMap) -> bool {
        let allowed = self.allow(
            RequestClass::State.label(),
            self.config.state_per_window,
            verified_source(headers),
        );
        if !allowed {
            self.metrics.record_class(RequestClass::State.label());
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

    pub(super) fn allow(&self, class: &'static str, limit: u64, identity: String) -> bool {
        self.allow_weight(class, limit, 1, identity)
    }

    pub(super) fn allow_weight(
        &self,
        class: &'static str,
        limit: u64,
        weight: u64,
        identity: String,
    ) -> bool {
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
