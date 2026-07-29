//! Owned by U01 (sol) — listener-free health vertical slice.

use axum::{Json, Router, extract::State, http::StatusCode, routing::get};

use crate::{AppDeps, ports::integrations::HealthReport};

pub(crate) fn router() -> Router<AppDeps> {
    Router::new().route("/health", get(health))
}

async fn health(State(deps): State<AppDeps>) -> (StatusCode, Json<HealthReport>) {
    match deps.health.check().await {
        Ok(report) => (StatusCode::OK, Json(report)),
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, Json(HealthReport::error())),
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, sync::Arc};

    use axum::{
        body::{Body, to_bytes},
        http::{HeaderMap, Request, StatusCode},
    };
    use tower::ServiceExt;

    use super::*;
    use crate::{
        config::AppConfig,
        error::AppError,
        model::*,
        ports::{
            AdminService, ArtifactService, BoxFuture, EngagementService, HealthProbe,
            NotificationSink, PageRenderer, PreviewService, PublisherAuthenticator, ShareService,
            ViewerIdentity, integrations::PreviewPriority,
        },
        render::view_models::{GalleryView, SettingsView, ShellView},
        security::access::{AuthorizedArtifact, OwnedArtifact},
    };

    #[derive(Clone, Copy)]
    struct DeterministicFake;

    fn unavailable<'a, T>() -> BoxFuture<'a, Result<T, AppError>> {
        Box::pin(async { Err(AppError::Unavailable("not used by health test".into())) })
    }

    impl PublisherAuthenticator for DeterministicFake {
        fn authenticate<'a>(
            &'a self,
            _headers: &'a HeaderMap,
        ) -> BoxFuture<'a, Result<PublisherIdentity, AppError>> {
            unavailable()
        }
    }

    impl ViewerIdentity for DeterministicFake {
        fn resolve<'a>(
            &'a self,
            _headers: &'a HeaderMap,
        ) -> BoxFuture<'a, Result<Viewer, AppError>> {
            unavailable()
        }
    }

    impl ArtifactService for DeterministicFake {
        fn find_meta<'a>(
            &'a self,
            _id: &'a ArtifactId,
        ) -> BoxFuture<'a, Result<Option<ArtifactMeta>, AppError>> {
            unavailable()
        }

        fn publish(
            &self,
            _request: PublishArtifact,
        ) -> BoxFuture<'_, Result<PublishedArtifact, AppError>> {
            unavailable()
        }

        fn list_for_publisher<'a>(
            &'a self,
            _publisher: &'a PublisherIdentity,
        ) -> BoxFuture<'a, Result<Vec<ArtifactMeta>, AppError>> {
            unavailable()
        }

        fn list_org_artifacts<'a>(
            &'a self,
            _org: &'a OrgId,
            _include_hidden: bool,
        ) -> BoxFuture<'a, Result<Vec<ArtifactMeta>, AppError>> {
            unavailable()
        }

        fn list_all_grouped_by_org(
            &self,
            _include_hidden: bool,
        ) -> BoxFuture<'_, Result<Vec<OrgArtifacts>, AppError>> {
            unavailable()
        }

        fn list_org_ids<'a>(
            &'a self,
            _org: &'a OrgId,
            _include_hidden: bool,
        ) -> BoxFuture<'a, Result<Vec<ArtifactId>, AppError>> {
            unavailable()
        }

        fn read_body<'a>(
            &'a self,
            _artifact: &'a AuthorizedArtifact,
        ) -> BoxFuture<'a, Result<Option<ArtifactFile>, AppError>> {
            unavailable()
        }

        fn read_bundle_file<'a>(
            &'a self,
            _artifact: &'a AuthorizedArtifact,
            _relative_path: &'a str,
        ) -> BoxFuture<'a, Result<Option<ArtifactFile>, AppError>> {
            unavailable()
        }

        fn read_revision_body<'a>(
            &'a self,
            _artifact: &'a AuthorizedArtifact,
            _revision: u64,
            _relative_path: Option<&'a str>,
        ) -> BoxFuture<'a, Result<Option<ArtifactFile>, AppError>> {
            unavailable()
        }

        fn list_bundle_files<'a>(
            &'a self,
            _artifact: &'a AuthorizedArtifact,
            _revision: Option<u64>,
        ) -> BoxFuture<'a, Result<Option<Vec<(String, u64)>>, AppError>> {
            unavailable()
        }

        fn list_revisions<'a>(
            &'a self,
            _artifact: &'a AuthorizedArtifact,
        ) -> BoxFuture<'a, Result<RevisionHistory, AppError>> {
            unavailable()
        }

        fn update(
            &self,
            _artifact: AuthorizedArtifact,
            _update: ArtifactUpdate,
        ) -> BoxFuture<'_, Result<UpdateArtifactResult, AppError>> {
            unavailable()
        }

        fn restore(
            &self,
            _artifact: AuthorizedArtifact,
            _revision: u64,
            _acting_client_id: Option<ClientId>,
        ) -> BoxFuture<'_, Result<RestoreArtifactResult, AppError>> {
            unavailable()
        }

        fn delete(&self, _artifact: AuthorizedArtifact) -> BoxFuture<'_, Result<bool, AppError>> {
            unavailable()
        }

        fn set_category(
            &self,
            _artifact: AuthorizedArtifact,
            _category: String,
        ) -> BoxFuture<'_, Result<ArtifactMeta, AppError>> {
            unavailable()
        }

        fn set_hidden(
            &self,
            _artifact: AuthorizedArtifact,
            _hidden: bool,
        ) -> BoxFuture<'_, Result<ArtifactMeta, AppError>> {
            unavailable()
        }

        fn move_to_org(
            &self,
            _artifact: AuthorizedArtifact,
            _target_org: OrgId,
            _category: Option<String>,
        ) -> BoxFuture<'_, Result<ArtifactMeta, AppError>> {
            unavailable()
        }

        fn audit_storage(
            &self,
            _clean_transient: bool,
        ) -> BoxFuture<'_, Result<StorageAuditReport, AppError>> {
            unavailable()
        }

        fn backfill_body_digests(&self) -> BoxFuture<'_, Result<DigestBackfillReport, AppError>> {
            unavailable()
        }
    }

    impl AdminService for DeterministicFake {
        fn list_keys(&self) -> BoxFuture<'_, Result<Vec<PublisherKeySummary>, AppError>> {
            unavailable()
        }

        fn create_key(
            &self,
            _request: CreatePublisherKey,
        ) -> BoxFuture<'_, Result<CreatedPublisherKey, AppError>> {
            unavailable()
        }

        fn revoke_key<'a>(
            &'a self,
            _client_id: &'a ClientId,
        ) -> BoxFuture<'a, Result<bool, AppError>> {
            unavailable()
        }

        fn org_exists<'a>(&'a self, _org: &'a OrgId) -> BoxFuture<'a, Result<bool, AppError>> {
            unavailable()
        }

        fn org_for_domain<'a>(
            &'a self,
            _domain: &'a str,
        ) -> BoxFuture<'a, Result<Option<OrgId>, AppError>> {
            unavailable()
        }

        fn org_for_email<'a>(
            &'a self,
            _email: &'a EmailAddress,
        ) -> BoxFuture<'a, Result<Option<OrgId>, AppError>> {
            unavailable()
        }

        fn org_names(&self) -> BoxFuture<'_, Result<Vec<OrgId>, AppError>> {
            unavailable()
        }

        fn list_orgs(&self) -> BoxFuture<'_, Result<Vec<Organization>, AppError>> {
            unavailable()
        }

        fn create_org(
            &self,
            _request: CreateOrganization,
        ) -> BoxFuture<'_, Result<Organization, AppError>> {
            unavailable()
        }

        fn delete_org<'a>(&'a self, _org: &'a OrgId) -> BoxFuture<'a, Result<bool, AppError>> {
            unavailable()
        }

        fn add_domain<'a>(
            &'a self,
            _org: &'a OrgId,
            _domain: &'a str,
        ) -> BoxFuture<'a, Result<String, AppError>> {
            unavailable()
        }

        fn remove_domain<'a>(
            &'a self,
            _org: &'a OrgId,
            _domain: &'a str,
        ) -> BoxFuture<'a, Result<bool, AppError>> {
            unavailable()
        }

        fn add_email_member<'a>(
            &'a self,
            _org: &'a OrgId,
            _email: &'a EmailAddress,
        ) -> BoxFuture<'a, Result<EmailAddress, AppError>> {
            unavailable()
        }

        fn remove_email_member<'a>(
            &'a self,
            _org: &'a OrgId,
            _email: &'a EmailAddress,
        ) -> BoxFuture<'a, Result<bool, AppError>> {
            unavailable()
        }

        fn categories<'a>(
            &'a self,
            _org: &'a OrgId,
        ) -> BoxFuture<'a, Result<Vec<String>, AppError>> {
            unavailable()
        }

        fn add_category<'a>(
            &'a self,
            _org: &'a OrgId,
            _name: &'a str,
        ) -> BoxFuture<'a, Result<String, AppError>> {
            unavailable()
        }

        fn remove_category<'a>(
            &'a self,
            _org: &'a OrgId,
            _name: &'a str,
        ) -> BoxFuture<'a, Result<bool, AppError>> {
            unavailable()
        }

        fn color_map(&self) -> BoxFuture<'_, Result<BTreeMap<OrgId, Option<String>>, AppError>> {
            unavailable()
        }

        fn set_color<'a>(
            &'a self,
            _org: &'a OrgId,
            _color: Option<&'a str>,
        ) -> BoxFuture<'a, Result<Option<String>, AppError>> {
            unavailable()
        }

        fn list_webhooks<'a>(
            &'a self,
            _org: &'a OrgId,
        ) -> BoxFuture<'a, Result<Vec<WebhookSummary>, AppError>> {
            unavailable()
        }

        fn create_webhook(
            &self,
            _request: CreateWebhook,
        ) -> BoxFuture<'_, Result<WebhookSummary, AppError>> {
            unavailable()
        }

        fn remove_webhook<'a>(
            &'a self,
            _org: &'a OrgId,
            _id: &'a WebhookId,
        ) -> BoxFuture<'a, Result<bool, AppError>> {
            unavailable()
        }

        fn set_webhook_events<'a>(
            &'a self,
            _org: &'a OrgId,
            _id: &'a WebhookId,
            _events: &'a [WebhookEvent],
        ) -> BoxFuture<'a, Result<Option<WebhookSummary>, AppError>> {
            unavailable()
        }

        fn webhook_delivery<'a>(
            &'a self,
            _id: &'a WebhookId,
        ) -> BoxFuture<'a, Result<Option<WebhookDelivery>, AppError>> {
            unavailable()
        }
    }

    impl EngagementService for DeterministicFake {
        fn reaction<'a>(
            &'a self,
            _artifact: &'a AuthorizedArtifact,
            _viewer: &'a Viewer,
        ) -> BoxFuture<'a, Result<Reaction, AppError>> {
            unavailable()
        }

        fn set_reaction(
            &self,
            _artifact: AuthorizedArtifact,
            _viewer: Viewer,
            _update: ReactionUpdate,
        ) -> BoxFuture<'_, Result<Reaction, AppError>> {
            unavailable()
        }

        fn reactions_for_viewer<'a>(
            &'a self,
            _viewer: &'a Viewer,
        ) -> BoxFuture<'a, Result<BTreeMap<ArtifactId, Reaction>, AppError>> {
            unavailable()
        }

        fn sentiment(&self) -> BoxFuture<'_, Result<BTreeMap<ArtifactId, Sentiment>, AppError>> {
            unavailable()
        }

        fn record_view<'a>(
            &'a self,
            _artifact: &'a AuthorizedArtifact,
            _viewer: &'a Viewer,
        ) -> BoxFuture<'a, Result<(), AppError>> {
            unavailable()
        }

        fn view_counts<'a>(
            &'a self,
            _artifact: &'a AuthorizedArtifact,
        ) -> BoxFuture<'a, Result<ViewCounts, AppError>> {
            unavailable()
        }

        fn view_counts_for_org<'a>(
            &'a self,
            _org: &'a OrgId,
        ) -> BoxFuture<'a, Result<BTreeMap<ArtifactId, ViewCounts>, AppError>> {
            unavailable()
        }

        fn viewers<'a>(
            &'a self,
            _artifact: &'a AuthorizedArtifact,
        ) -> BoxFuture<'a, Result<Vec<ViewerView>, AppError>> {
            unavailable()
        }

        fn top_for_org<'a>(
            &'a self,
            _org: &'a OrgId,
            _limit: usize,
        ) -> BoxFuture<'a, Result<Vec<TopViewedArtifact>, AppError>> {
            unavailable()
        }

        fn feedback_ref<'a>(
            &'a self,
            _id: &'a FeedbackId,
        ) -> BoxFuture<'a, Result<Option<FeedbackRef>, AppError>> {
            unavailable()
        }

        fn list_feedback<'a>(
            &'a self,
            _artifact: &'a AuthorizedArtifact,
        ) -> BoxFuture<'a, Result<Vec<Feedback>, AppError>> {
            unavailable()
        }

        fn submit_feedback(
            &self,
            _artifact: AuthorizedArtifact,
            _submission: SubmitFeedback,
        ) -> BoxFuture<'_, Result<Feedback, AppError>> {
            unavailable()
        }

        fn delete_feedback(
            &self,
            _artifact: AuthorizedArtifact,
            _viewer: Viewer,
            _id: FeedbackId,
        ) -> BoxFuture<'_, Result<FeedbackMutation, AppError>> {
            unavailable()
        }

        fn resolve_feedback_as_viewer(
            &self,
            _artifact: AuthorizedArtifact,
            _viewer: Viewer,
            _id: FeedbackId,
        ) -> BoxFuture<'_, Result<FeedbackMutation, AppError>> {
            unavailable()
        }

        fn list_feedback_for_publisher<'a>(
            &'a self,
            _publisher: &'a PublisherIdentity,
            _artifact: Option<&'a OwnedArtifact>,
        ) -> BoxFuture<'a, Result<Vec<Feedback>, AppError>> {
            unavailable()
        }

        fn resolve_feedback_as_publisher(
            &self,
            _artifact: OwnedArtifact,
            _id: FeedbackId,
            _resolved_by: String,
        ) -> BoxFuture<'_, Result<bool, AppError>> {
            unavailable()
        }

        fn reopen_feedback_as_publisher(
            &self,
            _artifact: OwnedArtifact,
            _id: FeedbackId,
        ) -> BoxFuture<'_, Result<bool, AppError>> {
            unavailable()
        }

        fn recent_notifications<'a>(
            &'a self,
            _viewer: &'a Viewer,
            _limit: usize,
        ) -> BoxFuture<'a, Result<Vec<ViewerNotification>, AppError>> {
            unavailable()
        }

        fn unread_notifications<'a>(
            &'a self,
            _viewer: &'a Viewer,
        ) -> BoxFuture<'a, Result<u64, AppError>> {
            unavailable()
        }

        fn mark_notifications_seen<'a>(
            &'a self,
            _email: &'a EmailAddress,
        ) -> BoxFuture<'a, Result<(), AppError>> {
            unavailable()
        }
    }

    impl ShareService for DeterministicFake {
        fn resolve<'a>(
            &'a self,
            _token: &'a ShareToken,
        ) -> BoxFuture<'a, Result<Option<ShareGrant>, AppError>> {
            unavailable()
        }

        fn create(
            &self,
            _artifact: AuthorizedArtifact,
            _request: CreateShare,
        ) -> BoxFuture<'_, Result<PublicShare, AppError>> {
            unavailable()
        }

        fn list<'a>(
            &'a self,
            _artifact: &'a AuthorizedArtifact,
        ) -> BoxFuture<'a, Result<Vec<PublicShare>, AppError>> {
            unavailable()
        }

        fn revoke(
            &self,
            _artifact: AuthorizedArtifact,
            _token: ShareToken,
        ) -> BoxFuture<'_, Result<bool, AppError>> {
            unavailable()
        }
    }

    impl PageRenderer for DeterministicFake {
        fn gallery(&self, _view: &GalleryView) -> Result<String, AppError> {
            Err(AppError::Unavailable("not used by health test".into()))
        }

        fn shell(&self, _view: &ShellView) -> Result<String, AppError> {
            Err(AppError::Unavailable("not used by health test".into()))
        }

        fn settings(&self, _view: &SettingsView) -> Result<String, AppError> {
            Err(AppError::Unavailable("not used by health test".into()))
        }

        fn not_found(&self, _message: Option<&str>) -> Result<String, AppError> {
            Err(AppError::Unavailable("not used by health test".into()))
        }

        fn not_signed_in(&self) -> Result<String, AppError> {
            Err(AppError::Unavailable("not used by health test".into()))
        }

        fn access_retry(&self, _target: &str) -> Result<String, AppError> {
            Err(AppError::Unavailable("not used by health test".into()))
        }
    }

    impl PreviewService for DeterministicFake {
        fn enabled(&self) -> bool {
            false
        }

        fn read_thumbnail<'a>(
            &'a self,
            _artifact: &'a AuthorizedArtifact,
            _digest: &'a str,
        ) -> BoxFuture<'a, Result<Option<Vec<u8>>, AppError>> {
            unavailable()
        }

        fn placeholder(&self, _meta: &ArtifactMeta, _accent: Option<&str>) -> Vec<u8> {
            Vec::new()
        }

        fn ensure_thumbnail<'a>(
            &'a self,
            _meta: &'a ArtifactMeta,
            _html: &'a str,
            _priority: PreviewPriority,
        ) -> BoxFuture<'a, Result<Option<Vec<u8>>, AppError>> {
            unavailable()
        }

        fn remove_artifact<'a>(
            &'a self,
            _id: &'a ArtifactId,
        ) -> BoxFuture<'a, Result<(), AppError>> {
            unavailable()
        }
    }

    impl NotificationSink for DeterministicFake {
        fn emit(
            &self,
            _event: WebhookEvent,
            _org: OrgId,
            _payload: NotificationPayload,
        ) -> BoxFuture<'_, Result<(), AppError>> {
            unavailable()
        }

        fn test<'a>(
            &'a self,
            _webhook: &'a WebhookDelivery,
        ) -> BoxFuture<'a, Result<DeliveryResult, AppError>> {
            unavailable()
        }
    }

    impl HealthProbe for DeterministicFake {
        fn check(&self) -> BoxFuture<'_, Result<HealthReport, AppError>> {
            Box::pin(async { Ok(HealthReport::ok()) })
        }
    }

    fn test_deps() -> AppDeps {
        let fake = Arc::new(DeterministicFake);
        AppDeps {
            publisher_auth: fake.clone(),
            viewer_identity: fake.clone(),
            artifacts: fake.clone(),
            admin: fake.clone(),
            engagement: fake.clone(),
            shares: fake.clone(),
            pages: fake.clone(),
            previews: fake.clone(),
            notifications: fake.clone(),
            health: fake,
            preview_tasks: crate::mcp::tasks::PreviewTaskStore::new(
                std::env::temp_dir().join("artifact-mcp-health-test-tasks"),
            ),
            mcp_telemetry: crate::observability::McpTelemetry::default(),
            config: Arc::new(AppConfig::default()),
        }
    }

    #[tokio::test]
    async fn health_route_uses_fake_probe_without_a_listener() {
        let response = crate::build_router(test_deps())
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .expect("valid request"),
            )
            .await
            .expect("router response");

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 1024)
            .await
            .expect("health body");
        assert_eq!(body.as_ref(), br#"{"status":"ok"}"#);
    }
}
