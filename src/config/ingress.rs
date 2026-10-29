//! Origin-side request admission limits.

use ipnet::IpNet;

use crate::error::AppError;

use super::{
    defaults::*,
    env::EnvSource,
    parse::{invalid, positive_integer, present},
};

/// Origin-side request admission limits.
///
/// These controls complement (rather than replace) Cloudflare and reverse-proxy limits.  The
/// values are intentionally plain integers so they can be set safely in deployment manifests;
/// callers must not derive a value from request data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IngressConfig {
    /// Maximum parsed HTTP header fields per HTTP/1 request.
    pub max_headers: u64,
    /// Maximum aggregate request-header bytes, including names and values.
    pub max_header_bytes: u64,
    /// Maximum request target (path plus query) bytes.
    pub max_uri_bytes: u64,
    /// Deadline while receiving a request body. This is not a handler deadline.
    pub read_timeout_ms: u64,
    /// Handler deadline for read-only routes; durable mutations are never cancelled mid-flight.
    pub read_handler_timeout_ms: u64,
    /// Grace before process shutdown aborts remaining connection tasks for crash recovery.
    pub shutdown_grace_ms: u64,
    /// Whole-request admission permits.
    pub max_concurrent_requests: u64,
    /// Accepted TCP connections, including peers that are still sending headers.
    pub max_connections: u64,
    /// Additional permits for unsafe HTTP methods.
    pub max_concurrent_mutations: u64,
    /// Bounded pending preview-render jobs, excluding the active job.
    pub render_queue_jobs: u64,
    /// Total declared source bytes reserved by pending and active preview jobs.
    pub render_queue_bytes: u64,
    /// JSON nesting depth accepted by any JSON endpoint.
    pub json_max_depth: u64,
    /// Total JSON values accepted by any JSON endpoint.
    pub json_max_nodes: u64,
    /// Object fields or array entries accepted in a single JSON container.
    pub json_max_members: u64,
    /// JSON-RPC batch entries accepted by `/mcp`.
    pub json_max_batch: u64,
    /// Refill duration used for the bounded token buckets.
    pub rate_window_seconds: u64,
    /// Read requests per principal/source window.
    pub reads_per_window: u64,
    /// Unsafe requests per principal/source window.
    pub mutations_per_window: u64,
    /// MCP transport requests per source window, before a verified publisher is available.
    pub mcp_per_window: u64,
    /// Artifact content writes per verified publisher/source window.
    pub uploads_per_window: u64,
    /// Viewer feedback mutations per source window.
    pub feedback_per_window: u64,
    /// Viewer state mutations per source window.
    pub state_per_window: u64,
    /// Administrative requests per source window.
    pub admin_per_window: u64,
    /// Requests per verified Access viewer/source window.
    pub verified_viewers_per_window: u64,
    /// Public-share requests per token/source window.
    pub shares_per_window: u64,
    /// Candidate credential attempts per source window.
    pub auth_failures_per_window: u64,
    /// Trusted Cloudflare/proxy peers allowed to supply the single `CF-Connecting-IP` address.
    /// `X-Forwarded-For` is never position-guessed.
    pub trusted_proxy_cidrs: Vec<IpNet>,
}

impl Default for IngressConfig {
    fn default() -> Self {
        Self {
            max_headers: DEFAULT_INGRESS_MAX_HEADERS,
            max_header_bytes: DEFAULT_INGRESS_MAX_HEADER_BYTES,
            max_uri_bytes: DEFAULT_INGRESS_MAX_URI_BYTES,
            read_timeout_ms: DEFAULT_INGRESS_READ_TIMEOUT_MS,
            read_handler_timeout_ms: DEFAULT_INGRESS_READ_HANDLER_TIMEOUT_MS,
            shutdown_grace_ms: DEFAULT_INGRESS_SHUTDOWN_GRACE_MS,
            max_concurrent_requests: DEFAULT_INGRESS_MAX_CONCURRENT_REQUESTS,
            max_connections: DEFAULT_INGRESS_MAX_CONNECTIONS,
            max_concurrent_mutations: DEFAULT_INGRESS_MAX_CONCURRENT_MUTATIONS,
            render_queue_jobs: DEFAULT_INGRESS_RENDER_QUEUE_JOBS,
            render_queue_bytes: DEFAULT_INGRESS_RENDER_QUEUE_BYTES,
            json_max_depth: DEFAULT_INGRESS_JSON_MAX_DEPTH,
            json_max_nodes: DEFAULT_INGRESS_JSON_MAX_NODES,
            json_max_members: DEFAULT_INGRESS_JSON_MAX_MEMBERS,
            json_max_batch: DEFAULT_INGRESS_JSON_MAX_BATCH,
            rate_window_seconds: DEFAULT_INGRESS_RATE_WINDOW_SECONDS,
            reads_per_window: DEFAULT_INGRESS_READS_PER_WINDOW,
            mutations_per_window: DEFAULT_INGRESS_MUTATIONS_PER_WINDOW,
            mcp_per_window: DEFAULT_INGRESS_MCP_PER_WINDOW,
            uploads_per_window: DEFAULT_INGRESS_UPLOADS_PER_WINDOW,
            feedback_per_window: DEFAULT_INGRESS_FEEDBACK_PER_WINDOW,
            state_per_window: DEFAULT_INGRESS_STATE_PER_WINDOW,
            admin_per_window: DEFAULT_INGRESS_ADMIN_PER_WINDOW,
            verified_viewers_per_window: DEFAULT_INGRESS_VERIFIED_VIEWERS_PER_WINDOW,
            shares_per_window: DEFAULT_INGRESS_SHARES_PER_WINDOW,
            auth_failures_per_window: DEFAULT_INGRESS_AUTH_FAILURES_PER_WINDOW,
            trusted_proxy_cidrs: Vec::new(),
        }
    }
}

impl IngressConfig {
    pub(super) fn from_source(env: &dyn EnvSource) -> Result<Self, AppError> {
        let trusted_proxy_cidrs = trusted_proxy_cidrs(env)?;
        let max_header_bytes = max_header_bytes(env)?;
        let rates = std::cell::Cell::new(None);
        Ok(Self {
            max_headers: positive_integer(env, "INGRESS_MAX_HEADERS", DEFAULT_INGRESS_MAX_HEADERS)?,
            max_header_bytes,
            max_uri_bytes: positive_integer(
                env,
                "INGRESS_MAX_URI_BYTES",
                DEFAULT_INGRESS_MAX_URI_BYTES,
            )?,
            read_timeout_ms: positive_integer(
                env,
                "INGRESS_READ_TIMEOUT_MS",
                DEFAULT_INGRESS_READ_TIMEOUT_MS,
            )?,
            read_handler_timeout_ms: positive_integer(
                env,
                "INGRESS_READ_HANDLER_TIMEOUT_MS",
                DEFAULT_INGRESS_READ_HANDLER_TIMEOUT_MS,
            )?,
            shutdown_grace_ms: positive_integer(
                env,
                "INGRESS_SHUTDOWN_GRACE_MS",
                DEFAULT_INGRESS_SHUTDOWN_GRACE_MS,
            )?,
            max_concurrent_requests: positive_integer(
                env,
                "INGRESS_MAX_CONCURRENT_REQUESTS",
                DEFAULT_INGRESS_MAX_CONCURRENT_REQUESTS,
            )?,
            max_connections: positive_integer(
                env,
                "INGRESS_MAX_CONNECTIONS",
                DEFAULT_INGRESS_MAX_CONNECTIONS,
            )?,
            max_concurrent_mutations: positive_integer(
                env,
                "INGRESS_MAX_CONCURRENT_MUTATIONS",
                DEFAULT_INGRESS_MAX_CONCURRENT_MUTATIONS,
            )?,
            render_queue_jobs: positive_integer(
                env,
                "INGRESS_RENDER_QUEUE_JOBS",
                DEFAULT_INGRESS_RENDER_QUEUE_JOBS,
            )?,
            render_queue_bytes: positive_integer(
                env,
                "INGRESS_RENDER_QUEUE_BYTES",
                DEFAULT_INGRESS_RENDER_QUEUE_BYTES,
            )?,
            json_max_depth: positive_integer(
                env,
                "INGRESS_JSON_MAX_DEPTH",
                DEFAULT_INGRESS_JSON_MAX_DEPTH,
            )?,
            json_max_nodes: positive_integer(
                env,
                "INGRESS_JSON_MAX_NODES",
                DEFAULT_INGRESS_JSON_MAX_NODES,
            )?,
            json_max_members: positive_integer(
                env,
                "INGRESS_JSON_MAX_MEMBERS",
                DEFAULT_INGRESS_JSON_MAX_MEMBERS,
            )?,
            json_max_batch: positive_integer(
                env,
                "INGRESS_JSON_MAX_BATCH",
                DEFAULT_INGRESS_JSON_MAX_BATCH,
            )?,
            rate_window_seconds: {
                rates.set(Some(RateLimits::from_source(env)?));
                rates.get().ok_or(AppError::Internal)?.rate_window_seconds
            },
            reads_per_window: rates.get().ok_or(AppError::Internal)?.reads_per_window,
            mutations_per_window: rates.get().ok_or(AppError::Internal)?.mutations_per_window,
            mcp_per_window: rates.get().ok_or(AppError::Internal)?.mcp_per_window,
            uploads_per_window: rates.get().ok_or(AppError::Internal)?.uploads_per_window,
            feedback_per_window: rates.get().ok_or(AppError::Internal)?.feedback_per_window,
            state_per_window: rates.get().ok_or(AppError::Internal)?.state_per_window,
            admin_per_window: rates.get().ok_or(AppError::Internal)?.admin_per_window,
            verified_viewers_per_window: rates
                .get()
                .ok_or(AppError::Internal)?
                .verified_viewers_per_window,
            shares_per_window: rates.get().ok_or(AppError::Internal)?.shares_per_window,
            auth_failures_per_window: rates
                .get()
                .ok_or(AppError::Internal)?
                .auth_failures_per_window,
            trusted_proxy_cidrs,
        })
    }
}

fn trusted_proxy_cidrs(env: &dyn EnvSource) -> Result<Vec<IpNet>, AppError> {
    present(env, "TRUSTED_PROXY_CIDRS")
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| {
                    value.parse::<IpNet>().map_err(|_| {
                        invalid("TRUSTED_PROXY_CIDRS", value, "a comma-separated CIDR list")
                    })
                })
                .collect()
        })
        .transpose()
        .map(|cidrs| cidrs.unwrap_or_default())
}

fn max_header_bytes(env: &dyn EnvSource) -> Result<u64, AppError> {
    let value = positive_integer(
        env,
        "INGRESS_MAX_HEADER_BYTES",
        DEFAULT_INGRESS_MAX_HEADER_BYTES,
    )?;
    if value < 8 * 1024 {
        return Err(invalid(
            "INGRESS_MAX_HEADER_BYTES",
            &value.to_string(),
            "at least 8192 bytes for Hyper HTTP/1 parsing",
        ));
    }
    Ok(value)
}

#[derive(Clone, Copy)]
struct RateLimits {
    rate_window_seconds: u64,
    reads_per_window: u64,
    mutations_per_window: u64,
    mcp_per_window: u64,
    uploads_per_window: u64,
    feedback_per_window: u64,
    state_per_window: u64,
    admin_per_window: u64,
    verified_viewers_per_window: u64,
    shares_per_window: u64,
    auth_failures_per_window: u64,
}

impl RateLimits {
    fn from_source(env: &dyn EnvSource) -> Result<Self, AppError> {
        Ok(Self {
            rate_window_seconds: positive_integer(
                env,
                "INGRESS_RATE_WINDOW_SECONDS",
                DEFAULT_INGRESS_RATE_WINDOW_SECONDS,
            )?,
            reads_per_window: positive_integer(
                env,
                "INGRESS_READS_PER_WINDOW",
                DEFAULT_INGRESS_READS_PER_WINDOW,
            )?,
            mutations_per_window: positive_integer(
                env,
                "INGRESS_MUTATIONS_PER_WINDOW",
                DEFAULT_INGRESS_MUTATIONS_PER_WINDOW,
            )?,
            mcp_per_window: positive_integer(
                env,
                "INGRESS_MCP_PER_WINDOW",
                DEFAULT_INGRESS_MCP_PER_WINDOW,
            )?,
            uploads_per_window: positive_integer(
                env,
                "INGRESS_UPLOADS_PER_WINDOW",
                DEFAULT_INGRESS_UPLOADS_PER_WINDOW,
            )?,
            feedback_per_window: positive_integer(
                env,
                "INGRESS_FEEDBACK_PER_WINDOW",
                DEFAULT_INGRESS_FEEDBACK_PER_WINDOW,
            )?,
            state_per_window: positive_integer(
                env,
                "INGRESS_STATE_PER_WINDOW",
                DEFAULT_INGRESS_STATE_PER_WINDOW,
            )?,
            admin_per_window: positive_integer(
                env,
                "INGRESS_ADMIN_PER_WINDOW",
                DEFAULT_INGRESS_ADMIN_PER_WINDOW,
            )?,
            verified_viewers_per_window: positive_integer(
                env,
                "INGRESS_VERIFIED_VIEWERS_PER_WINDOW",
                DEFAULT_INGRESS_VERIFIED_VIEWERS_PER_WINDOW,
            )?,
            shares_per_window: positive_integer(
                env,
                "INGRESS_SHARES_PER_WINDOW",
                DEFAULT_INGRESS_SHARES_PER_WINDOW,
            )?,
            auth_failures_per_window: positive_integer(
                env,
                "INGRESS_AUTH_FAILURES_PER_WINDOW",
                DEFAULT_INGRESS_AUTH_FAILURES_PER_WINDOW,
            )?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::env::MapEnv;

    #[test]
    fn ingress_configuration_reports_trusted_proxy_cidrs_before_later_limits() {
        let env = MapEnv::empty()
            .with("TRUSTED_PROXY_CIDRS", "not-a-cidr")
            .with("INGRESS_MAX_HEADER_BYTES", "1");

        let error = IngressConfig::from_source(&env).expect_err("invalid configuration");

        assert!(error.to_string().contains("TRUSTED_PROXY_CIDRS"));
    }

    #[test]
    fn ingress_configuration_reports_header_floor_before_later_limits() {
        let env = MapEnv::empty()
            .with("INGRESS_MAX_HEADER_BYTES", "1")
            .with("INGRESS_MAX_HEADERS", "not-a-number");

        let error = IngressConfig::from_source(&env).expect_err("invalid configuration");

        assert!(error.to_string().contains("INGRESS_MAX_HEADER_BYTES"));
    }
}
