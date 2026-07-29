//! Owned by U01 (sol) — frozen application composition seam.

use std::sync::Arc;

use axum::Router;

use crate::{
    config::AppConfig,
    http,
    mcp::tasks::PreviewTaskStore,
    observability::McpTelemetry,
    ports::{
        AdminService, ArtifactService, EngagementService, HealthProbe, NotificationSink,
        PageRenderer, PreviewService, PublisherAuthenticator, ShareService, ViewerIdentity,
    },
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
    pub engagement: Arc<dyn EngagementService>,
    pub shares: Arc<dyn ShareService>,
    pub pages: Arc<dyn PageRenderer>,
    pub previews: Arc<dyn PreviewService>,
    pub notifications: Arc<dyn NotificationSink>,
    pub health: Arc<dyn HealthProbe>,
    pub preview_tasks: Arc<PreviewTaskStore>,
    pub mcp_telemetry: McpTelemetry,
    pub config: Arc<AppConfig>,
}

/// Build the application without binding a listener.
pub fn build_router(deps: AppDeps) -> Router {
    http::router().with_state(deps)
}
