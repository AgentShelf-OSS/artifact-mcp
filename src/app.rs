//! Owned by U01 (sol) — frozen application composition seam.

use std::sync::Arc;

use axum::{Router, middleware};

use crate::{
    config::AppConfig,
    http,
    integrations::delivery_runtime::{DeliveryTelemetry, DeliveryWakeSignal},
    mcp::tasks::PreviewTaskStore,
    observability::McpTelemetry,
    ports::{
        AdminService, ArtifactService, DiscussionService, EngagementService, HealthProbe,
        NotificationSink, PageRenderer, PreviewService, PublisherAuthenticator, ShareService,
        ViewerIdentity,
    },
    security::audit::AuditAccess,
};

/// Runtime dependencies shared by every route.
///
/// Trait objects are intentional: production and deterministic test adapters share one
/// concrete Axum state type, while individual capabilities remain replaceable.
#[derive(Clone)]
pub struct AppDeps {
    pub publisher_auth: Arc<dyn PublisherAuthenticator>,
    pub viewer_identity: Arc<dyn ViewerIdentity>,
    pub artifacts: Arc<dyn ArtifactService>,
    pub admin: Arc<dyn AdminService>,
    /// Required discussion boundary. Tests use the inert adapter when discussion behaviour is
    /// outside their scope so routes never silently disappear at runtime.
    pub discussions: Arc<dyn DiscussionService>,
    pub engagement: Arc<dyn EngagementService>,
    pub shares: Arc<dyn ShareService>,
    pub pages: Arc<dyn PageRenderer>,
    pub previews: Arc<dyn PreviewService>,
    pub notifications: Arc<dyn NotificationSink>,
    pub health: Arc<dyn HealthProbe>,
    /// Listener-wide origin admission controls shared by every route.
    pub ingress: Arc<http::ingress::IngressState>,
    pub preview_tasks: Arc<PreviewTaskStore>,
    pub mcp_telemetry: McpTelemetry,
    /// Aggregate durable-delivery health, with no webhook or tenant dimensions.
    pub delivery_telemetry: DeliveryTelemetry,
    /// A lossy, post-commit worker wake hint. Polling remains the correctness fallback.
    pub delivery_wake: DeliveryWakeSignal,
    pub audit_access: Option<Arc<AuditAccess>>,
    pub config: Arc<AppConfig>,
}

/// Build the application without binding a listener.
pub fn build_router(deps: AppDeps) -> Router {
    // The request-authenticity gate is deliberately outside the route units: every human
    // mutation receives the same policy, while the middleware itself explicitly leaves `/mcp`
    // alone because that surface uses bearer credentials rather than ambient viewer cookies.
    let csrf_state = Arc::new(http::middleware::RequestAuthenticityState::from_config(
        &deps.config,
    ));
    let ingress = Arc::clone(&deps.ingress);
    http::router()
        .with_state(deps)
        .layer(middleware::from_fn_with_state(
            csrf_state,
            http::middleware::same_origin_gate,
        ))
        .layer(middleware::from_fn_with_state(
            ingress,
            http::ingress::admit,
        ))
}
