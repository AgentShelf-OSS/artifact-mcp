use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use artifact_mcp::{
    AppDeps,
    config::{AppConfig, Secret},
    error::AppError,
    model::*,
    ports::{
        AdminService, ArtifactService, BoxFuture, EngagementService, HealthProbe, NotificationSink,
        PageRenderer, PreviewService, PublisherAuthenticator, ShareService, ViewerIdentity,
        integrations::{HealthReport, PreviewPriority},
    },
    render::view_models::{GalleryView, SettingsView, ShellView},
    security::access::{AuthorizedArtifact, OwnedArtifact},
};
use axum::{
    body::{Body, to_bytes},
    http::{HeaderMap, Request, StatusCode, header},
};
use serde_json::{Value, json};
use tower::ServiceExt;

static NEXT_AUDIT_ROUTE_TEMP: AtomicU64 = AtomicU64::new(0);
const AUDIT_KEY: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

struct AuditStartupObserver;

impl super::u20_runtime::runtime::StartupObserver for AuditStartupObserver {
    fn stage(&self, _stage: super::u20_runtime::runtime::StartupStage) {}
}

fn unavailable<'a, T>() -> BoxFuture<'a, Result<T, AppError>> {
    Box::pin(async {
        Err(AppError::Unavailable(
            "unused U18 test dependency".to_owned(),
        ))
    })
}

struct Harness {
    viewer: Mutex<Viewer>,
    keys: Mutex<Vec<PublisherKeySummary>>,
    organizations: Mutex<Vec<Organization>>,
    webhooks: Mutex<BTreeMap<OrgId, Vec<WebhookSummary>>>,
    rendered_settings: Mutex<Option<SettingsView>>,
    calls: Mutex<Vec<AdminCall>>,
    deliveries: Mutex<BTreeMap<WebhookId, WebhookDelivery>>,
    tested_webhooks: Mutex<Vec<WebhookDelivery>>,
    delivery_result: Mutex<DeliveryResult>,
    publisher: Mutex<Option<PublisherIdentity>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum AdminCall {
    OrgExists(OrgId),
    CreateKey(CreatePublisherKey),
    RevokeKey(ClientId),
    CreateOrg(CreateOrganization),
    DeleteOrg(OrgId),
    AddDomain(OrgId, String),
    RemoveDomain(OrgId, String),
    AddEmail(OrgId, EmailAddress),
    RemoveEmail(OrgId, EmailAddress),
    AddCategory(OrgId, String),
    RemoveCategory(OrgId, String),
    SetColor(OrgId, Option<String>),
    CreateWebhook(CreateWebhook),
    RemoveWebhook(OrgId, WebhookId),
    SetWebhookEvents(OrgId, WebhookId, Vec<WebhookEvent>),
    LookupWebhook(WebhookId),
    AuditWebhookTest(OrgId, WebhookId, Option<bool>),
}

impl Harness {
    fn admin() -> Arc<Self> {
        Arc::new(Self {
            viewer: Mutex::new(Viewer {
                email: Some(EmailAddress::from("admin@example.test")),
                org: Some(OrgId::from("admin")),
                is_admin: true,
            }),
            keys: Mutex::new(Vec::new()),
            organizations: Mutex::new(Vec::new()),
            webhooks: Mutex::new(BTreeMap::new()),
            rendered_settings: Mutex::new(None),
            calls: Mutex::new(Vec::new()),
            deliveries: Mutex::new(BTreeMap::new()),
            tested_webhooks: Mutex::new(Vec::new()),
            delivery_result: Mutex::new(DeliveryResult {
                ok: true,
                error: None,
            }),
            publisher: Mutex::new(None),
        })
    }

    fn deps(self: &Arc<Self>) -> AppDeps {
        self.deps_with_config(AppConfig::default())
    }

    fn deps_with_config(self: &Arc<Self>, config: AppConfig) -> AppDeps {
        let audit_access = config.audit_ledger_hmac_key.as_ref().map(|encoded| {
            let pool = artifact_mcp::persistence::db::Database::open(&config)
                .expect("open pooled audit route database");
            let key = artifact_mcp::security::audit::parse_hmac_key(encoded.expose())
                .expect("validated audit key");
            Arc::new(artifact_mcp::security::audit::AuditAccess::new(pool, key))
        });
        AppDeps {
            publisher_auth: self.clone(),
            viewer_identity: self.clone(),
            artifacts: self.clone(),
            admin: self.clone(),
            discussions: Arc::new(artifact_mcp::ports::InertDiscussionService),
            engagement: self.clone(),
            shares: self.clone(),
            pages: self.clone(),
            previews: self.clone(),
            notifications: self.clone(),
            health: self.clone(),
            ingress: Arc::new(artifact_mcp::http::ingress::IngressState::from_config(
                &config,
            )),
            preview_tasks: artifact_mcp::mcp::tasks::PreviewTaskStore::new(
                std::env::temp_dir().join(format!("artifact-mcp-u18-tasks-{}", std::process::id())),
            ),
            mcp_telemetry: artifact_mcp::observability::McpTelemetry::default(),
            delivery_telemetry:
                artifact_mcp::integrations::delivery_runtime::DeliveryTelemetry::default(),
            delivery_wake:
                artifact_mcp::integrations::delivery_runtime::DeliveryWakeSignal::default(),
            audit_access,
            config: Arc::new(config),
        }
    }

    fn set_publisher(&self, publisher: Option<PublisherIdentity>) {
        *self
            .publisher
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = publisher;
    }
}

impl PublisherAuthenticator for Harness {
    fn authenticate<'a>(
        &'a self,
        _headers: &'a HeaderMap,
    ) -> BoxFuture<'a, Result<PublisherIdentity, AppError>> {
        let publisher = self
            .publisher
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        Box::pin(async move {
            publisher.ok_or_else(|| {
                AppError::Unauthorized("publisher authentication required".to_owned())
            })
        })
    }
}

impl ViewerIdentity for Harness {
    fn resolve<'a>(&'a self, _headers: &'a HeaderMap) -> BoxFuture<'a, Result<Viewer, AppError>> {
        let viewer = self
            .viewer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        Box::pin(async move { Ok(viewer) })
    }
}

impl AdminService for Harness {
    fn list_keys(&self) -> BoxFuture<'_, Result<Vec<PublisherKeySummary>, AppError>> {
        let keys = self
            .keys
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        Box::pin(async move { Ok(keys) })
    }

    fn create_key(
        &self,
        request: CreatePublisherKey,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'_, Result<CreatedPublisherKey, AppError>> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(AdminCall::CreateKey(request.clone()));
        Box::pin(async move {
            Ok(CreatedPublisherKey {
                client_id: request.client_id,
                org: request.org,
                label: request.label,
                role: request.role,
                owner_email: request.owner_email,
                secret: "one-time-secret".to_owned(),
            })
        })
    }

    fn revoke_key<'a>(
        &'a self,
        client_id: &'a ClientId,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'a, Result<bool, AppError>> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(AdminCall::RevokeKey(client_id.clone()));
        let revoked = client_id.0 == "publisher-2";
        Box::pin(async move { Ok(revoked) })
    }

    fn org_exists<'a>(&'a self, org: &'a OrgId) -> BoxFuture<'a, Result<bool, AppError>> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(AdminCall::OrgExists(org.clone()));
        let exists = self
            .organizations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|organization| organization.name == *org);
        Box::pin(async move { Ok(exists) })
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
        let organizations = self
            .organizations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        Box::pin(async move { Ok(organizations) })
    }

    fn create_org(
        &self,
        request: CreateOrganization,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'_, Result<Organization, AppError>> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(AdminCall::CreateOrg(request.clone()));
        Box::pin(async move {
            Ok(Organization {
                name: request.name,
                label: request.label,
                color: None,
                created_at: None,
                domains: request.domain.into_iter().collect(),
                emails: Vec::new(),
                categories: Vec::new(),
                key_count: 0,
            })
        })
    }

    fn delete_org<'a>(
        &'a self,
        org: &'a OrgId,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'a, Result<bool, AppError>> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(AdminCall::DeleteOrg(org.clone()));
        Box::pin(async { Ok(true) })
    }

    fn add_domain<'a>(
        &'a self,
        org: &'a OrgId,
        domain: &'a str,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'a, Result<String, AppError>> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(AdminCall::AddDomain(org.clone(), domain.to_owned()));
        let domain = domain.to_lowercase();
        Box::pin(async move { Ok(domain) })
    }

    fn remove_domain<'a>(
        &'a self,
        org: &'a OrgId,
        domain: &'a str,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'a, Result<bool, AppError>> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(AdminCall::RemoveDomain(org.clone(), domain.to_owned()));
        Box::pin(async { Ok(true) })
    }

    fn add_email_member<'a>(
        &'a self,
        org: &'a OrgId,
        email: &'a EmailAddress,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'a, Result<EmailAddress, AppError>> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(AdminCall::AddEmail(org.clone(), email.clone()));
        let email = EmailAddress(email.0.trim().to_lowercase());
        Box::pin(async move { Ok(email) })
    }

    fn remove_email_member<'a>(
        &'a self,
        org: &'a OrgId,
        email: &'a EmailAddress,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'a, Result<bool, AppError>> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(AdminCall::RemoveEmail(org.clone(), email.clone()));
        Box::pin(async { Ok(true) })
    }

    fn categories<'a>(&'a self, _org: &'a OrgId) -> BoxFuture<'a, Result<Vec<String>, AppError>> {
        unavailable()
    }

    fn add_category<'a>(
        &'a self,
        org: &'a OrgId,
        name: &'a str,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'a, Result<String, AppError>> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(AdminCall::AddCategory(org.clone(), name.to_owned()));
        let name = name.to_owned();
        Box::pin(async move { Ok(name) })
    }

    fn remove_category<'a>(
        &'a self,
        org: &'a OrgId,
        name: &'a str,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'a, Result<bool, AppError>> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(AdminCall::RemoveCategory(org.clone(), name.to_owned()));
        Box::pin(async { Ok(true) })
    }

    fn color_map(&self) -> BoxFuture<'_, Result<BTreeMap<OrgId, Option<String>>, AppError>> {
        unavailable()
    }

    fn set_color<'a>(
        &'a self,
        org: &'a OrgId,
        color: Option<&'a str>,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'a, Result<Option<String>, AppError>> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(AdminCall::SetColor(org.clone(), color.map(str::to_owned)));
        let color = color.map(str::to_owned).filter(|value| !value.is_empty());
        Box::pin(async move { Ok(color) })
    }

    fn list_webhooks<'a>(
        &'a self,
        org: &'a OrgId,
    ) -> BoxFuture<'a, Result<Vec<WebhookSummary>, AppError>> {
        let rows = self
            .webhooks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(org)
            .cloned()
            .unwrap_or_default();
        Box::pin(async move { Ok(rows) })
    }

    fn create_webhook(
        &self,
        request: CreateWebhook,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'_, Result<WebhookSummary, AppError>> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(AdminCall::CreateWebhook(request.clone()));
        Box::pin(async move {
            Ok(WebhookSummary {
                id: WebhookId::from("wh0000000002"),
                label: request.label,
                events: request.events.unwrap_or_else(|| {
                    vec![
                        WebhookEvent::Published,
                        WebhookEvent::Updated,
                        WebhookEvent::Restored,
                        WebhookEvent::Deleted,
                        WebhookEvent::Feedback,
                        WebhookEvent::Resolved,
                    ]
                }),
                url: "https://discord.com…wxyz".to_owned(),
                last_ok_at: None,
                last_error: None,
            })
        })
    }

    fn remove_webhook<'a>(
        &'a self,
        org: &'a OrgId,
        id: &'a WebhookId,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'a, Result<bool, AppError>> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(AdminCall::RemoveWebhook(org.clone(), id.clone()));
        Box::pin(async { Ok(true) })
    }

    fn set_webhook_events<'a>(
        &'a self,
        org: &'a OrgId,
        id: &'a WebhookId,
        events: &'a [WebhookEvent],
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'a, Result<Option<WebhookSummary>, AppError>> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(AdminCall::SetWebhookEvents(
                org.clone(),
                id.clone(),
                events.to_vec(),
            ));
        let id = id.clone();
        let events = events.to_vec();
        Box::pin(async move {
            Ok(Some(WebhookSummary {
                id,
                label: "Ops".to_owned(),
                events,
                url: "https://discord.com…wxyz".to_owned(),
                last_ok_at: None,
                last_error: None,
            }))
        })
    }

    fn webhook_delivery<'a>(
        &'a self,
        id: &'a WebhookId,
    ) -> BoxFuture<'a, Result<Option<WebhookDelivery>, AppError>> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(AdminCall::LookupWebhook(id.clone()));
        let delivery = self
            .deliveries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .cloned();
        Box::pin(async move { Ok(delivery) })
    }

    fn audit_webhook_test<'a>(
        &'a self,
        org: &'a OrgId,
        id: &'a WebhookId,
        outcome: Option<bool>,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'a, Result<(), AppError>> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(AdminCall::AuditWebhookTest(
                org.clone(),
                id.clone(),
                outcome,
            ));
        Box::pin(async { Ok(()) })
    }
}

impl ArtifactService for Harness {
    fn find_meta<'a>(
        &'a self,
        _id: &'a ArtifactId,
    ) -> BoxFuture<'a, Result<Option<ArtifactMeta>, AppError>> {
        unavailable()
    }

    fn publish(
        &self,
        _request: PublishArtifact,
        _audit: artifact_mcp::security::audit::MutationAudit,
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
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'_, Result<UpdateArtifactResult, AppError>> {
        unavailable()
    }

    fn restore(
        &self,
        _artifact: AuthorizedArtifact,
        _revision: u64,
        _acting_client_id: Option<ClientId>,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'_, Result<RestoreArtifactResult, AppError>> {
        unavailable()
    }

    fn delete(
        &self,
        _artifact: AuthorizedArtifact,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'_, Result<bool, AppError>> {
        unavailable()
    }

    fn set_category(
        &self,
        _artifact: AuthorizedArtifact,
        _category: String,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'_, Result<ArtifactMeta, AppError>> {
        unavailable()
    }

    fn set_hidden(
        &self,
        _artifact: AuthorizedArtifact,
        _hidden: bool,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'_, Result<ArtifactMeta, AppError>> {
        unavailable()
    }

    fn move_to_org(
        &self,
        _artifact: AuthorizedArtifact,
        _target_org: OrgId,
        _category: Option<String>,
        _audit: artifact_mcp::security::audit::MutationAudit,
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

impl EngagementService for Harness {
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

impl ShareService for Harness {
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
        _audit: artifact_mcp::security::audit::MutationAudit,
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
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'_, Result<bool, AppError>> {
        unavailable()
    }
}

impl PageRenderer for Harness {
    fn gallery(&self, _view: &GalleryView) -> Result<String, AppError> {
        Err(AppError::Unavailable("unused U18 renderer".to_owned()))
    }

    fn shell(&self, _view: &ShellView) -> Result<String, AppError> {
        Err(AppError::Unavailable("unused U18 renderer".to_owned()))
    }

    fn settings(&self, view: &SettingsView) -> Result<String, AppError> {
        *self
            .rendered_settings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(view.clone());
        Ok("<main>settings fixture</main>".to_owned())
    }

    fn not_found(&self, _message: Option<&str>) -> Result<String, AppError> {
        Err(AppError::Unavailable("unused U18 renderer".to_owned()))
    }

    fn not_signed_in(&self) -> Result<String, AppError> {
        Err(AppError::Unavailable("unused U18 renderer".to_owned()))
    }

    fn access_retry(&self, _target: &str) -> Result<String, AppError> {
        Err(AppError::Unavailable("unused U18 renderer".to_owned()))
    }
}

impl PreviewService for Harness {
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

    fn remove_artifact<'a>(&'a self, _id: &'a ArtifactId) -> BoxFuture<'a, Result<(), AppError>> {
        unavailable()
    }
}

impl NotificationSink for Harness {
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
        webhook: &'a WebhookDelivery,
    ) -> BoxFuture<'a, Result<DeliveryResult, AppError>> {
        self.tested_webhooks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(webhook.clone());
        let result = self
            .delivery_result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        Box::pin(async move { Ok(result) })
    }
}

impl HealthProbe for Harness {
    fn check(&self) -> BoxFuture<'_, Result<HealthReport, AppError>> {
        unavailable()
    }
}

fn sample_key() -> PublisherKeySummary {
    PublisherKeySummary {
        client_id: ClientId::from("publisher-1"),
        org: OrgId::from("acme"),
        label: "Acme publisher".to_owned(),
        role: "author".to_owned(),
        owner_email: None,
        created_at: Timestamp("2026-01-01 00:00:00".to_owned()),
        revoked_at: None,
    }
}

fn sample_org() -> Organization {
    Organization {
        name: OrgId::from("acme"),
        label: "Acme".to_owned(),
        color: Some("#356B9F".to_owned()),
        created_at: Some(Timestamp("2026-01-01 00:00:00".to_owned())),
        domains: vec!["acme.test".to_owned()],
        emails: vec!["member@example.test".to_owned()],
        categories: vec!["Reports".to_owned()],
        key_count: 1,
    }
}

fn sample_webhook() -> WebhookSummary {
    WebhookSummary {
        id: WebhookId::from("wh0000000001"),
        label: "Ops".to_owned(),
        events: vec![WebhookEvent::Published],
        url: "https://discord.com…wxyz".to_owned(),
        last_ok_at: None,
        last_error: None,
    }
}

#[tokio::test]
async fn admin_settings_renders_keys_orgs_and_masked_webhooks_without_caching() {
    let harness = Harness::admin();
    harness
        .keys
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(sample_key());
    harness
        .organizations
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(sample_org());
    harness
        .webhooks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(OrgId::from("acme"), vec![sample_webhook()]);

    let response = artifact_mcp::build_router(harness.deps())
        .oneshot(
            Request::get("/settings")
                .body(Body::empty())
                .expect("valid request"),
        )
        .await
        .expect("route response");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/html; charset=utf-8"
    );
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let body = to_bytes(response.into_body(), 1024)
        .await
        .expect("settings body");
    assert_eq!(body.as_ref(), b"<main>settings fixture</main>");

    let rendered = harness
        .rendered_settings
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
        .expect("settings renderer was called");
    assert!(rendered.viewer.is_admin);
    assert_eq!(rendered.keys, vec![sample_key()]);
    assert_eq!(rendered.organizations.len(), 1);
    assert_eq!(rendered.organizations[0].organization, sample_org());
    assert_eq!(rendered.organizations[0].webhooks, vec![sample_webhook()]);
}

#[tokio::test]
async fn publisher_key_creation_checks_the_org_and_displays_the_secret_once() {
    let harness = Harness::admin();
    harness
        .organizations
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(sample_org());

    let response = artifact_mcp::build_router(harness.deps())
        .oneshot(
            Request::post("/settings/keys")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "clientId": "publisher-2",
                        "org": "acme",
                        "label": "Research publisher"
                    })
                    .to_string(),
                ))
                .expect("valid request"),
        )
        .await
        .expect("route response");

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 4096)
        .await
        .expect("key response body");
    let value: Value = serde_json::from_slice(&body).expect("key response JSON");
    assert_eq!(value["clientId"], "publisher-2");
    assert_eq!(value["org"], "acme");
    assert_eq!(value["label"], "Research publisher");
    assert_eq!(value["role"], "author");
    assert_eq!(value["secret"], "one-time-secret");
    let created_at = value["created_at"]
        .as_str()
        .expect("created_at is a string");
    assert!(created_at.ends_with('Z'));
    assert_eq!(created_at.len(), 24, "Node ISO timestamps have 3 ms digits");
    assert_eq!(
        harness
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_slice(),
        [
            AdminCall::OrgExists(OrgId::from("acme")),
            AdminCall::CreateKey(CreatePublisherKey {
                client_id: ClientId::from("publisher-2"),
                org: OrgId::from("acme"),
                label: "Research publisher".to_owned(),
                role: "author".to_owned(),
                owner_email: None,
            }),
        ]
    );
}

#[tokio::test]
async fn publisher_key_revoke_returns_the_service_result_for_the_raw_path_id() {
    let harness = Harness::admin();
    let response = artifact_mcp::build_router(harness.deps())
        .oneshot(
            Request::post("/settings/keys/publisher-2/revoke")
                .body(Body::empty())
                .expect("valid request"),
        )
        .await
        .expect("route response");

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 1024)
        .await
        .expect("revoke response body");
    assert_eq!(
        serde_json::from_slice::<Value>(&body).expect("revoke response JSON"),
        json!({ "id": "publisher-2", "revoked": true })
    );
    assert_eq!(
        harness
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_slice(),
        [AdminCall::RevokeKey(ClientId::from("publisher-2"))]
    );
}

#[tokio::test]
async fn organization_creation_passes_optional_fields_and_keeps_the_node_response_shape() {
    let harness = Harness::admin();
    let response = artifact_mcp::build_router(harness.deps())
        .oneshot(
            Request::post("/settings/orgs")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "name": "newco",
                        "domain": "newco.test"
                    })
                    .to_string(),
                ))
                .expect("valid request"),
        )
        .await
        .expect("route response");

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 4096)
        .await
        .expect("organization response body");
    assert_eq!(
        serde_json::from_slice::<Value>(&body).expect("organization response JSON"),
        json!({
            "name": "newco",
            "label": "",
            "domains": ["newco.test"],
            "emails": [],
            "categories": [],
            "keyCount": 0
        })
    );
    assert_eq!(
        harness
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_slice(),
        [AdminCall::CreateOrg(CreateOrganization {
            name: OrgId::from("newco"),
            label: String::new(),
            domain: Some("newco.test".to_owned()),
        })]
    );
}

#[tokio::test]
async fn organization_deletion_returns_the_path_name_and_service_result() {
    let harness = Harness::admin();
    let response = artifact_mcp::build_router(harness.deps())
        .oneshot(
            Request::delete("/settings/orgs/acme")
                .body(Body::empty())
                .expect("valid request"),
        )
        .await
        .expect("route response");

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 1024)
        .await
        .expect("organization deletion response");
    assert_eq!(
        serde_json::from_slice::<Value>(&body).expect("organization deletion JSON"),
        json!({ "name": "acme", "removed": true })
    );
    assert_eq!(
        harness
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_slice(),
        [AdminCall::DeleteOrg(OrgId::from("acme"))]
    );
}

#[tokio::test]
async fn domain_add_and_remove_preserve_the_node_response_values() {
    let harness = Harness::admin();
    let app = artifact_mcp::build_router(harness.deps());
    let added = app
        .clone()
        .oneshot(
            Request::post("/settings/orgs/acme/domains")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(json!({ "domain": "EXAMPLE.TEST" }).to_string()))
                .expect("valid request"),
        )
        .await
        .expect("add-domain response");
    assert_eq!(added.status(), StatusCode::OK);
    let added = to_bytes(added.into_body(), 1024)
        .await
        .expect("add-domain body");
    assert_eq!(
        serde_json::from_slice::<Value>(&added).expect("add-domain JSON"),
        json!({ "org": "acme", "domain": "example.test" })
    );

    let removed = app
        .oneshot(
            Request::delete("/settings/orgs/acme/domains/EXAMPLE.TEST")
                .body(Body::empty())
                .expect("valid request"),
        )
        .await
        .expect("remove-domain response");
    assert_eq!(removed.status(), StatusCode::OK);
    let removed = to_bytes(removed.into_body(), 1024)
        .await
        .expect("remove-domain body");
    assert_eq!(
        serde_json::from_slice::<Value>(&removed).expect("remove-domain JSON"),
        json!({ "org": "acme", "domain": "EXAMPLE.TEST", "removed": true })
    );
    assert_eq!(
        harness
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_slice(),
        [
            AdminCall::AddDomain(OrgId::from("acme"), "EXAMPLE.TEST".to_owned()),
            AdminCall::RemoveDomain(OrgId::from("acme"), "EXAMPLE.TEST".to_owned()),
        ]
    );
}

#[tokio::test]
async fn email_member_add_and_remove_match_node_normalization_boundaries() {
    let harness = Harness::admin();
    let app = artifact_mcp::build_router(harness.deps());
    let added = app
        .clone()
        .oneshot(
            Request::post("/settings/orgs/acme/emails")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({ "email": " Person@Example.com " }).to_string(),
                ))
                .expect("valid request"),
        )
        .await
        .expect("add-email response");
    assert_eq!(added.status(), StatusCode::OK);
    let added = to_bytes(added.into_body(), 1024)
        .await
        .expect("add-email body");
    assert_eq!(
        serde_json::from_slice::<Value>(&added).expect("add-email JSON"),
        json!({ "org": "acme", "email": "person@example.com" })
    );

    let removed = app
        .oneshot(
            Request::delete("/settings/orgs/acme/emails/Person%40Example.com")
                .body(Body::empty())
                .expect("valid request"),
        )
        .await
        .expect("remove-email response");
    assert_eq!(removed.status(), StatusCode::OK);
    let removed = to_bytes(removed.into_body(), 1024)
        .await
        .expect("remove-email body");
    assert_eq!(
        serde_json::from_slice::<Value>(&removed).expect("remove-email JSON"),
        json!({ "org": "acme", "email": "person@example.com", "removed": true })
    );
    assert_eq!(
        harness
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_slice(),
        [
            AdminCall::AddEmail(
                OrgId::from("acme"),
                EmailAddress::from(" Person@Example.com ")
            ),
            AdminCall::RemoveEmail(
                OrgId::from("acme"),
                EmailAddress::from("person@example.com")
            ),
        ]
    );
}

#[tokio::test]
async fn category_add_and_remove_delegate_and_keep_the_raw_remove_name() {
    let harness = Harness::admin();
    let app = artifact_mcp::build_router(harness.deps());
    let added = app
        .clone()
        .oneshot(
            Request::post("/settings/orgs/acme/categories")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(json!({ "name": "Reports" }).to_string()))
                .expect("valid request"),
        )
        .await
        .expect("add-category response");
    assert_eq!(added.status(), StatusCode::OK);
    let added = to_bytes(added.into_body(), 1024)
        .await
        .expect("add-category body");
    assert_eq!(
        serde_json::from_slice::<Value>(&added).expect("add-category JSON"),
        json!({ "org": "acme", "name": "Reports" })
    );

    let removed = app
        .oneshot(
            Request::delete("/settings/orgs/acme/categories")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(json!({ "name": "  Reports  " }).to_string()))
                .expect("valid request"),
        )
        .await
        .expect("remove-category response");
    assert_eq!(removed.status(), StatusCode::OK);
    let removed = to_bytes(removed.into_body(), 1024)
        .await
        .expect("remove-category body");
    assert_eq!(
        serde_json::from_slice::<Value>(&removed).expect("remove-category JSON"),
        json!({ "org": "acme", "name": "  Reports  ", "removed": true })
    );
    assert_eq!(
        harness
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_slice(),
        [
            AdminCall::AddCategory(OrgId::from("acme"), "Reports".to_owned()),
            AdminCall::RemoveCategory(OrgId::from("acme"), "  Reports  ".to_owned()),
        ]
    );
}

#[tokio::test]
async fn organization_color_returns_name_and_nullable_color() {
    let harness = Harness::admin();
    let response = artifact_mcp::build_router(harness.deps())
        .oneshot(
            Request::post("/settings/orgs/acme/color")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(json!({ "color": "#356B9F" }).to_string()))
                .expect("valid request"),
        )
        .await
        .expect("color response");

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 1024)
        .await
        .expect("color body");
    assert_eq!(
        serde_json::from_slice::<Value>(&body).expect("color JSON"),
        json!({ "name": "acme", "color": "#356B9F" })
    );
    assert_eq!(
        harness
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_slice(),
        [AdminCall::SetColor(
            OrgId::from("acme"),
            Some("#356B9F".to_owned())
        )]
    );
}

#[tokio::test]
async fn settings_role_denials_are_explicit_forbidden_responses() {
    for (viewer, expected) in [
        (
            Viewer {
                email: None,
                org: None,
                is_admin: false,
            },
            "Not signed in",
        ),
        (
            Viewer {
                email: Some(EmailAddress::from("member@acme.test")),
                org: Some(OrgId::from("acme")),
                is_admin: false,
            },
            "Admins only",
        ),
    ] {
        let harness = Harness::admin();
        *harness
            .viewer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = viewer;
        let app = artifact_mcp::build_router(harness.deps());

        let page = app
            .clone()
            .oneshot(
                Request::get("/settings")
                    .body(Body::empty())
                    .expect("valid page request"),
            )
            .await
            .expect("page response");
        assert_eq!(page.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            to_bytes(page.into_body(), 1024)
                .await
                .expect("page denial body")
                .as_ref(),
            expected.as_bytes()
        );

        let mutation = app
            .oneshot(
                Request::post("/settings/orgs")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .expect("valid mutation request"),
            )
            .await
            .expect("mutation response");
        assert_eq!(mutation.status(), StatusCode::FORBIDDEN);
        let mutation = to_bytes(mutation.into_body(), 1024)
            .await
            .expect("mutation denial body");
        assert_eq!(
            serde_json::from_slice::<Value>(&mutation).expect("mutation denial JSON"),
            json!({ "error": expected })
        );
        assert!(
            harness
                .calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty(),
            "a denied request must not reach AdminService"
        );
    }
}

#[tokio::test]
async fn settings_json_uses_the_exact_key_limit_and_parse_error_envelopes() {
    const LIMIT: usize = 64 * 1024;
    fn body_of_size(size: usize) -> String {
        let envelope = "{\"pad\":\"\"}";
        let pad = "x".repeat(size - envelope.len());
        format!("{{\"pad\":\"{pad}\"}}")
    }

    let malformed_harness = Harness::admin();
    let malformed = artifact_mcp::build_router(malformed_harness.deps())
        .oneshot(
            Request::post("/settings/orgs")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{"))
                .expect("malformed request"),
        )
        .await
        .expect("malformed response");
    assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);
    let malformed = to_bytes(malformed.into_body(), 1024)
        .await
        .expect("malformed body");
    assert_eq!(
        serde_json::from_slice::<Value>(&malformed).expect("malformed JSON envelope"),
        json!({ "error": "invalid JSON" })
    );

    let boundary_harness = Harness::admin();
    let boundary = artifact_mcp::build_router(boundary_harness.deps())
        .oneshot(
            Request::post("/settings/keys")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body_of_size(LIMIT)))
                .expect("boundary request"),
        )
        .await
        .expect("boundary response");
    assert_eq!(boundary.status(), StatusCode::BAD_REQUEST);
    let boundary = to_bytes(boundary.into_body(), 1024)
        .await
        .expect("boundary body");
    assert_eq!(
        serde_json::from_slice::<Value>(&boundary).expect("boundary JSON"),
        json!({
            "error": "Unknown organization \"\". Create it in the Organizations section first."
        })
    );

    let oversized_harness = Harness::admin();
    let oversized = artifact_mcp::build_router(oversized_harness.deps())
        .oneshot(
            Request::post("/settings/keys")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body_of_size(LIMIT + 1)))
                .expect("oversized request"),
        )
        .await
        .expect("oversized response");
    assert_eq!(oversized.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let oversized = to_bytes(oversized.into_body(), 1024)
        .await
        .expect("oversized body");
    assert_eq!(
        serde_json::from_slice::<Value>(&oversized).expect("oversized JSON"),
        json!({ "error": "payload too large" })
    );

    let registry_harness = Harness::admin();
    let category = artifact_mcp::build_router(registry_harness.deps())
        .oneshot(
            Request::post("/settings/orgs/acme/categories")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({ "name": "x".repeat(9 * 1024) }).to_string(),
                ))
                .expect("category request above category_json"),
        )
        .await
        .expect("category response");
    assert_eq!(
        category.status(),
        StatusCode::OK,
        "the settings category registry uses key_json; category_json belongs to /:id actions"
    );
}

#[tokio::test]
async fn settings_authorization_precedes_json_body_parsing() {
    const LIMIT: usize = 64 * 1024;
    let harness = Harness::admin();
    *harness
        .viewer
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Viewer {
        email: Some(EmailAddress::from("member@acme.test")),
        org: Some(OrgId::from("acme")),
        is_admin: false,
    };
    let app = artifact_mcp::build_router(harness.deps());

    for body in [
        "{".to_owned(),
        json!({ "pad": "x".repeat(LIMIT) }).to_string(),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::post("/settings/orgs")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body))
                    .expect("valid request"),
            )
            .await
            .expect("authorization response");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body = to_bytes(response.into_body(), 1024)
            .await
            .expect("authorization body");
        assert_eq!(
            serde_json::from_slice::<Value>(&body).expect("authorization JSON"),
            json!({ "error": "Admins only" })
        );
    }
    assert!(
        harness
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty(),
        "authorization failures must not reach AdminService or parse the body"
    );
}

#[tokio::test]
async fn webhook_creation_delegates_the_secret_but_returns_only_the_masked_summary() {
    const SECRET_URL: &str =
        "https://discord.com/api/webhooks/123456789012345678/ULTRA-SECRET-TOKEN-wxyz";
    let harness = Harness::admin();
    let response = artifact_mcp::build_router(harness.deps())
        .oneshot(
            Request::post("/settings/orgs/acme/webhooks")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "url": SECRET_URL,
                        "label": "Ops",
                        "events": ["resolved", "published"]
                    })
                    .to_string(),
                ))
                .expect("valid request"),
        )
        .await
        .expect("create-webhook response");

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 4096)
        .await
        .expect("create-webhook body");
    let body_text = String::from_utf8(body.to_vec()).expect("webhook response is utf8");
    assert!(!body_text.contains("ULTRA-SECRET-TOKEN"));
    assert_eq!(
        serde_json::from_str::<Value>(&body_text).expect("create-webhook JSON"),
        json!({
            "id": "wh0000000002",
            "label": "Ops",
            "events": ["resolved", "published"],
            "url": "https://discord.com…wxyz",
            "last_ok_at": null,
            "last_error": null
        })
    );
    assert_eq!(
        harness
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_slice(),
        [AdminCall::CreateWebhook(CreateWebhook {
            org: OrgId::from("acme"),
            url: SECRET_URL.to_owned(),
            label: "Ops".to_owned(),
            events: Some(vec![WebhookEvent::Resolved, WebhookEvent::Published]),
        })]
    );
}

#[tokio::test]
async fn webhook_event_update_and_delete_remain_org_scoped() {
    let harness = Harness::admin();
    let app = artifact_mcp::build_router(harness.deps());
    let updated = app
        .clone()
        .oneshot(
            Request::patch("/settings/orgs/acme/webhooks/wh0000000001")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({ "events": ["feedback", "resolved"] }).to_string(),
                ))
                .expect("valid request"),
        )
        .await
        .expect("update-webhook response");
    assert_eq!(updated.status(), StatusCode::OK);
    let updated = to_bytes(updated.into_body(), 4096)
        .await
        .expect("update-webhook body");
    assert_eq!(
        serde_json::from_slice::<Value>(&updated).expect("update-webhook JSON")["events"],
        json!(["feedback", "resolved"])
    );

    let removed = app
        .oneshot(
            Request::delete("/settings/orgs/acme/webhooks/wh0000000001")
                .body(Body::empty())
                .expect("valid request"),
        )
        .await
        .expect("remove-webhook response");
    assert_eq!(removed.status(), StatusCode::OK);
    let removed = to_bytes(removed.into_body(), 1024)
        .await
        .expect("remove-webhook body");
    assert_eq!(
        serde_json::from_slice::<Value>(&removed).expect("remove-webhook JSON"),
        json!({ "org": "acme", "id": "wh0000000001", "removed": true })
    );
    assert_eq!(
        harness
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_slice(),
        [
            AdminCall::SetWebhookEvents(
                OrgId::from("acme"),
                WebhookId::from("wh0000000001"),
                vec![WebhookEvent::Feedback, WebhookEvent::Resolved],
            ),
            AdminCall::RemoveWebhook(OrgId::from("acme"), WebhookId::from("wh0000000001")),
        ]
    );
}

#[tokio::test]
async fn webhook_test_checks_the_org_and_omits_a_null_error_on_success() {
    const SECRET_URL: &str = "https://discord.com/api/webhooks/1/secret-token";
    let harness = Harness::admin();
    let delivery = WebhookDelivery {
        id: WebhookId::from("wh0000000001"),
        org: OrgId::from("acme"),
        url: SECRET_URL.to_owned(),
        label: "Ops".to_owned(),
        events: vec![WebhookEvent::Published],
    };
    harness
        .deliveries
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(delivery.id.clone(), delivery.clone());
    let app = artifact_mcp::build_router(harness.deps());

    let tested = app
        .clone()
        .oneshot(
            Request::post("/settings/orgs/acme/webhooks/wh0000000001/test")
                .body(Body::empty())
                .expect("valid test request"),
        )
        .await
        .expect("test-webhook response");
    assert_eq!(tested.status(), StatusCode::OK);
    let tested = to_bytes(tested.into_body(), 1024)
        .await
        .expect("test-webhook body");
    assert_eq!(
        serde_json::from_slice::<Value>(&tested).expect("test-webhook JSON"),
        json!({ "ok": true })
    );
    assert_eq!(
        harness
            .tested_webhooks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_slice(),
        [delivery]
    );
    assert_eq!(
        harness
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_slice(),
        [
            AdminCall::LookupWebhook(WebhookId::from("wh0000000001")),
            AdminCall::AuditWebhookTest(OrgId::from("acme"), WebhookId::from("wh0000000001"), None,),
            AdminCall::AuditWebhookTest(
                OrgId::from("acme"),
                WebhookId::from("wh0000000001"),
                Some(true),
            ),
        ]
    );

    *harness
        .delivery_result
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = DeliveryResult {
        ok: false,
        error: Some("remote rejected test".to_owned()),
    };
    let failed = app
        .clone()
        .oneshot(
            Request::post("/settings/orgs/acme/webhooks/wh0000000001/test")
                .body(Body::empty())
                .expect("valid failed test request"),
        )
        .await
        .expect("failed test-webhook response");
    assert_eq!(failed.status(), StatusCode::OK);
    let failed = to_bytes(failed.into_body(), 1024)
        .await
        .expect("failed test-webhook body");
    assert_eq!(
        serde_json::from_slice::<Value>(&failed).expect("failed test-webhook JSON"),
        json!({ "ok": false, "error": "remote rejected test" })
    );
    assert_eq!(
        &harness
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_slice()[3..],
        [
            AdminCall::LookupWebhook(WebhookId::from("wh0000000001")),
            AdminCall::AuditWebhookTest(OrgId::from("acme"), WebhookId::from("wh0000000001"), None,),
            AdminCall::AuditWebhookTest(
                OrgId::from("acme"),
                WebhookId::from("wh0000000001"),
                Some(false),
            ),
        ]
    );

    let foreign = app
        .oneshot(
            Request::post("/settings/orgs/other/webhooks/wh0000000001/test")
                .body(Body::empty())
                .expect("valid foreign request"),
        )
        .await
        .expect("foreign test response");
    assert_eq!(foreign.status(), StatusCode::NOT_FOUND);
    let foreign = to_bytes(foreign.into_body(), 1024)
        .await
        .expect("foreign test body");
    assert_eq!(
        serde_json::from_slice::<Value>(&foreign).expect("foreign test JSON"),
        json!({ "error": "Webhook not found" })
    );
    assert_eq!(
        harness
            .tested_webhooks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len(),
        2,
        "a foreign org must not reach NotificationSink"
    );
    assert_eq!(
        harness
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|call| matches!(call, AdminCall::AuditWebhookTest(..)))
            .count(),
        4,
        "concealed foreign webhook probes must not reach the audit ledger"
    );
}

#[tokio::test]
async fn webhook_events_reject_non_arrays_and_unknown_names_with_node_messages() {
    let app = artifact_mcp::build_router(Harness::admin().deps());
    for (events, expected) in [
        (
            json!("published"),
            "Webhook events must be an array.".to_owned(),
        ),
        (
            json!(["published", "bogus"]),
            "Unknown webhook event: bogus".to_owned(),
        ),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::post("/settings/orgs/acme/webhooks")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({
                            "url": "https://discord.com/api/webhooks/1/test",
                            "events": events
                        })
                        .to_string(),
                    ))
                    .expect("valid request"),
            )
            .await
            .expect("validation response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(response.into_body(), 1024)
            .await
            .expect("validation body");
        assert_eq!(
            serde_json::from_slice::<Value>(&body).expect("validation JSON"),
            json!({ "error": expected })
        );
    }
}

const REQUIRE_NODE_REFERENCE: &str = "REQUIRE_NODE_REFERENCE";

const NODE_ROUTE_DRIVER: &str = r#"
import(process.argv[1]).then(async ({ createApp }) => {
  const input = JSON.parse(process.argv[2]);
  const webhook = {
    id: "wh0000000002",
    org: "acme",
    url: "https://discord.com/api/webhooks/1/secret-token",
    label: "Ops",
    events: ["published"]
  };
  const publicWebhook = (events = webhook.events) => ({
    id: webhook.id,
    label: webhook.label,
    events,
    url: "https://discord.com…wxyz",
    last_ok_at: null,
    last_error: null
  });
  const app = createApp({
    checkPublisherKey: () => ({ ok: false }),
    handleMcp: async () => null,
    resolveViewer: async () => ({ email: "admin@example.test", org: "admin", isAdmin: true }),
    artifacts: {},
    keys: {
      list: () => [],
      create: (request) => ({ ...request, role: request.role || "author", secret: "one-time-secret" }),
      revoke: (id) => id === "publisher-2"
    },
    orgs: {
      list: () => [],
      names: () => [],
      has: (name) => name === "acme",
      create: ({ name, label, domain }) => ({
        name,
        label,
        domains: domain ? [domain] : [],
        emails: [],
        categories: [],
        keyCount: 0
      }),
      remove: () => true,
      addDomain: (org, domain) => ({ org, domain: String(domain).toLowerCase() }),
      removeDomain: () => true,
      addEmailMember: (org, email) => ({ org, email: String(email).trim().toLowerCase() }),
      removeEmailMember: () => true,
      addCategory: (org, name) => ({ org, name }),
      removeCategory: () => true,
      setColor: (name, color) => ({ name, color }),
      colorMap: () => ({})
    },
    webhooks: {
      listForOrg: () => [],
      create: ({ events }) => publicWebhook(events ?? ["published", "updated", "restored", "deleted", "feedback", "resolved"]),
      remove: () => true,
      setEvents: (_org, _id, events) => publicWebhook(events ?? []),
      get: () => webhook
    },
    notify: { emit() {}, test: async () => ({ ok: true }) },
    reactions: {},
    feedback: {},
    pages: {
      settings: () => "<main>settings fixture</main>",
      notFound: () => "not found",
      notSignedIn: () => "not signed in"
    },
    logger: { info() {}, error() {} }
  });

  async function invoke(candidate) {
    const layer = app._router.stack.find((entry) =>
      entry.route?.path === candidate.route && entry.route.methods[candidate.method]
    );
    if (!layer) throw new Error(`missing route ${candidate.method} ${candidate.route}`);
    const handler = layer.route.stack.at(-1).handle;
    const result = { status: 200, body: null };
    const response = {
      status(code) { result.status = code; return this; },
      set() { return this; },
      send(value) { result.body = value; return this; },
      json(value) { result.body = value; return this; },
      end() { return this; }
    };
    await handler({ headers: {}, params: candidate.params, query: {}, body: candidate.body }, response);
    if (candidate.route === "/settings/keys" && result.body?.created_at) {
      result.body.created_at = "<iso-timestamp>";
    }
    return result;
  }

  const results = [];
  for (const candidate of input) results.push(await invoke(candidate));
  process.stdout.write(JSON.stringify(results));
}).catch((error) => { console.error(error); process.exit(1); });
"#;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn require_node_reference() -> bool {
    std::env::var(REQUIRE_NODE_REFERENCE).is_ok_and(|value| value == "1")
}

fn node_reference_available(root: &Path) -> bool {
    let unavailable = if !root.join("lib/app.js").is_file() {
        Some("lib/app.js is missing")
    } else if !root.join("node_modules/express/package.json").is_file() {
        Some("Node dependencies are missing")
    } else {
        match Command::new("node").arg("--version").output() {
            Ok(output) if output.status.success() => None,
            _ => Some("node is not on PATH"),
        }
    };

    match unavailable {
        None => true,
        Some(reason) => {
            assert!(
                !require_node_reference(),
                "{REQUIRE_NODE_REFERENCE}=1 but the Node app reference is unavailable ({reason})"
            );
            eprintln!("skipping U18 Node route parity proof: {reason}");
            false
        }
    }
}

fn parity_cases() -> Value {
    json!([
        { "method": "get", "route": "/settings", "uri": "/settings", "params": {} },
        {
            "method": "post", "route": "/settings/keys", "uri": "/settings/keys",
            "params": {}, "body": { "clientId": "publisher-2", "org": "acme", "label": "Agent" }
        },
        {
            "method": "post", "route": "/settings/keys/:id/revoke",
            "uri": "/settings/keys/publisher-2/revoke", "params": { "id": "publisher-2" }
        },
        {
            "method": "post", "route": "/settings/orgs", "uri": "/settings/orgs", "params": {},
            "body": { "name": "newco", "label": "New Co", "domain": "newco.test" }
        },
        {
            "method": "delete", "route": "/settings/orgs/:name", "uri": "/settings/orgs/acme",
            "params": { "name": "acme" }
        },
        {
            "method": "post", "route": "/settings/orgs/:name/domains",
            "uri": "/settings/orgs/acme/domains", "params": { "name": "acme" },
            "body": { "domain": "DOCS.ACME.TEST" }
        },
        {
            "method": "delete", "route": "/settings/orgs/:name/domains/:domain",
            "uri": "/settings/orgs/acme/domains/docs.acme.test",
            "params": { "name": "acme", "domain": "docs.acme.test" }
        },
        {
            "method": "post", "route": "/settings/orgs/:name/emails",
            "uri": "/settings/orgs/acme/emails", "params": { "name": "acme" },
            "body": { "email": " Person@Example.com " }
        },
        {
            "method": "delete", "route": "/settings/orgs/:name/emails/:email",
            "uri": "/settings/orgs/acme/emails/person%40example.com",
            "params": { "name": "acme", "email": "person@example.com" }
        },
        {
            "method": "post", "route": "/settings/orgs/:name/categories",
            "uri": "/settings/orgs/acme/categories", "params": { "name": "acme" },
            "body": { "name": "Reports" }
        },
        {
            "method": "delete", "route": "/settings/orgs/:name/categories",
            "uri": "/settings/orgs/acme/categories", "params": { "name": "acme" },
            "body": { "name": "Reports" }
        },
        {
            "method": "post", "route": "/settings/orgs/:name/color",
            "uri": "/settings/orgs/acme/color", "params": { "name": "acme" },
            "body": { "color": "#356B9F" }
        },
        {
            "method": "post", "route": "/settings/orgs/:name/webhooks",
            "uri": "/settings/orgs/acme/webhooks", "params": { "name": "acme" },
            "body": {
                "url": "https://discord.com/api/webhooks/1/secret-token",
                "label": "Ops", "events": ["published"]
            }
        },
        {
            "method": "patch", "route": "/settings/orgs/:name/webhooks/:id",
            "uri": "/settings/orgs/acme/webhooks/wh0000000002",
            "params": { "name": "acme", "id": "wh0000000002" },
            "body": { "events": ["feedback", "resolved"] }
        },
        {
            "method": "delete", "route": "/settings/orgs/:name/webhooks/:id",
            "uri": "/settings/orgs/acme/webhooks/wh0000000002",
            "params": { "name": "acme", "id": "wh0000000002" }
        },
        {
            "method": "post", "route": "/settings/orgs/:name/webhooks/:id/test",
            "uri": "/settings/orgs/acme/webhooks/wh0000000002/test",
            "params": { "name": "acme", "id": "wh0000000002" }
        }
    ])
}

fn run_node_app(root: &Path, cases: &Value) -> Value {
    let module = format!("file://{}", root.join("lib/app.js").display());
    let output = Command::new("node")
        .current_dir(root)
        .arg("-e")
        .arg(NODE_ROUTE_DRIVER)
        .arg(module)
        .arg(cases.to_string())
        .output()
        .expect("run the Node app reference");
    assert!(
        output.status.success(),
        "Node app reference failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("Node app reference emitted JSON")
}

async fn run_rust_app(harness: &Arc<Harness>, cases: &Value) -> Value {
    let app = artifact_mcp::build_router(harness.deps());
    let mut results = Vec::new();
    for candidate in cases.as_array().expect("parity cases are an array") {
        let method = candidate["method"]
            .as_str()
            .expect("parity method")
            .to_ascii_uppercase();
        let uri = candidate["uri"].as_str().expect("parity URI");
        let method = method.parse::<axum::http::Method>().expect("HTTP method");
        let mut request = Request::builder().method(method).uri(uri);
        let body = candidate.get("body").filter(|body| !body.is_null());
        if body.is_some() {
            request = request.header(header::CONTENT_TYPE, "application/json");
        }
        let response = app
            .clone()
            .oneshot(
                request
                    .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
                    .expect("valid parity request"),
            )
            .await
            .expect("Rust route response");
        let status = response.status().as_u16();
        let is_json = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("application/json"));
        let bytes = to_bytes(response.into_body(), 128 * 1024)
            .await
            .expect("Rust parity response body");
        let mut body = if is_json {
            serde_json::from_slice(&bytes).expect("Rust parity JSON")
        } else {
            Value::String(String::from_utf8(bytes.to_vec()).expect("Rust parity text"))
        };
        if candidate["route"] == "/settings/keys" {
            body["created_at"] = Value::String("<iso-timestamp>".to_owned());
        }
        results.push(json!({ "status": status, "body": body }));
    }
    Value::Array(results)
}

#[tokio::test]
async fn node_app_route_handlers_match_the_rust_admin_contract() {
    let root = repo_root();
    if !node_reference_available(&root) {
        return;
    }

    let harness = Harness::admin();
    harness
        .organizations
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(sample_org());
    let delivery = WebhookDelivery {
        id: WebhookId::from("wh0000000002"),
        org: OrgId::from("acme"),
        url: "https://discord.com/api/webhooks/1/secret-token".to_owned(),
        label: "Ops".to_owned(),
        events: vec![WebhookEvent::Published],
    };
    harness
        .deliveries
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(delivery.id.clone(), delivery);

    let cases = parity_cases();
    let node = run_node_app(&root, &cases);
    let rust = run_rust_app(&harness, &cases).await;
    assert_eq!(rust, node, "Rust U18 routes diverged from lib/app.js");
}

fn audit_route_config(label: &str) -> (AppConfig, PathBuf) {
    let sequence = NEXT_AUDIT_ROUTE_TEMP.fetch_add(1, Ordering::Relaxed);
    let data_dir = std::env::temp_dir().join(format!(
        "artifact-mcp-u58-{label}-{}-{sequence}",
        std::process::id()
    ));
    let config = AppConfig {
        data_dir: data_dir.clone(),
        audit_ledger_hmac_key: Some(Secret::new(AUDIT_KEY)),
        ..AppConfig::defaults()
    };
    (config, data_dir)
}

fn oauth_publisher(org: &str, scopes: &[&str]) -> PublisherIdentity {
    PublisherIdentity {
        client_id: ClientId::from("audit-reader"),
        org: OrgId::from(org),
        label: "Audit reader".to_owned(),
        role: "reader".to_owned(),
        scopes: Some(scopes.iter().map(|scope| (*scope).to_owned()).collect()),
    }
}

fn append_audit_events(config: &AppConfig, acme: usize, other: usize) {
    let pool = artifact_mcp::persistence::db::Database::open(config).expect("audit database");
    let mut conn = pool.get().expect("audit connection");
    let key = artifact_mcp::security::audit::parse_hmac_key(AUDIT_KEY).expect("audit key");
    artifact_mcp::security::audit::initialize_head(&conn, &key).expect("seal audit head");
    for (tenant, count) in [("acme", acme), ("other", other)] {
        for sequence in 0..count {
            let transaction = conn.transaction().expect("audit transaction");
            artifact_mcp::security::audit::append_in_transaction(
                &transaction,
                &key,
                &format!("event-{tenant}-{sequence}"),
                &artifact_mcp::security::audit::AuditContext {
                    tenant: tenant.to_owned(),
                    actor_type: "api_key".to_owned(),
                    actor_id: "credential-secret-must-not-leak".to_owned(),
                    actor_role: "author".to_owned(),
                    source: "mcp".to_owned(),
                    request_id: format!("request-{tenant}-{sequence}"),
                },
                &artifact_mcp::security::audit::AuditEvent {
                    operation: "artifact.update".to_owned(),
                    target_type: "artifact".to_owned(),
                    target_id: format!("artifact-{tenant}-{sequence}"),
                    result: "success".to_owned(),
                    classification: "internal".to_owned(),
                    revision: Some(u64::try_from(sequence).expect("sequence fits")),
                },
            )
            .expect("append audit event");
            transaction.commit().expect("commit audit event");
        }
    }
}

async fn request(router: &axum::Router, uri: &str) -> axum::response::Response {
    router
        .clone()
        .oneshot(
            Request::get(uri)
                .body(Body::empty())
                .expect("audit route request"),
        )
        .await
        .expect("audit route response")
}

async fn mcp_list_artifacts(router: &axum::Router, id: u64) -> axum::response::Response {
    router
        .clone()
        .oneshot(
            Request::post("/mcp")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "method": "tools/call",
                        "params": { "name": "list_artifacts", "arguments": {} }
                    })
                    .to_string(),
                ))
                .expect("MCP list request"),
        )
        .await
        .expect("MCP list response")
}

#[tokio::test]
async fn audit_routes_require_scoped_oauth_and_preserve_tenant_boundaries() {
    let (config, data_dir) = audit_route_config("access");
    append_audit_events(&config, 501, 1);
    let harness = Harness::admin();
    let router = artifact_mcp::build_router(harness.deps_with_config(config));

    let unauthenticated = request(&router, "/audit/events").await;
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        request(&router, "/audit/export").await.status(),
        StatusCode::UNAUTHORIZED
    );
    let unauthenticated_metrics = request(&router, "/metrics").await;
    let unauthenticated_metrics = String::from_utf8(
        to_bytes(unauthenticated_metrics.into_body(), usize::MAX)
            .await
            .expect("audit auth metrics body")
            .to_vec(),
    )
    .expect("audit auth metrics text");
    assert!(signal_value(&unauthenticated_metrics, "auth_failure") >= 2);

    harness.set_publisher(Some(PublisherIdentity {
        client_id: ClientId::from("legacy"),
        org: OrgId::from("acme"),
        label: "Legacy key".to_owned(),
        role: "author".to_owned(),
        scopes: None,
    }));
    assert_eq!(
        request(&router, "/audit/events").await.status(),
        StatusCode::FORBIDDEN,
        "legacy API keys must never inherit audit capabilities"
    );
    assert_eq!(
        request(&router, "/audit/export").await.status(),
        StatusCode::FORBIDDEN,
        "legacy API keys must never inherit audit capabilities"
    );

    harness.set_publisher(Some(oauth_publisher("acme", &["audit:read"])));
    let page = request(&router, "/audit/events?limit=10000").await;
    assert_eq!(page.status(), StatusCode::OK);
    let page_body = to_bytes(page.into_body(), usize::MAX)
        .await
        .expect("read audit page");
    let page_json: Value = serde_json::from_slice(&page_body).expect("audit page JSON");
    assert_eq!(page_json["events"].as_array().map(Vec::len), Some(500));
    assert!(
        page_json["next"].as_str().is_some(),
        "query limit is clamped"
    );
    let page_text = String::from_utf8(page_body.to_vec()).expect("audit JSON text");
    assert!(!page_text.contains("credential-secret-must-not-leak"));
    assert!(!page_text.contains("request-acme-"));

    assert_eq!(
        request(&router, "/audit/events?tenant=other")
            .await
            .status(),
        StatusCode::FORBIDDEN,
        "same-tenant audit read cannot cross tenants"
    );
    harness.set_publisher(Some(oauth_publisher(
        "acme",
        &["audit:read", "audit:global"],
    )));
    let other = request(&router, "/audit/events?tenant=other&limit=1").await;
    assert_eq!(other.status(), StatusCode::OK);
    let other_json: Value = serde_json::from_slice(
        &to_bytes(other.into_body(), usize::MAX)
            .await
            .expect("other tenant body"),
    )
    .expect("other tenant JSON");
    assert_eq!(other_json["events"][0]["tenant"], "other");

    harness.set_publisher(Some(oauth_publisher(
        "acme",
        &["audit:read", "audit:export"],
    )));
    assert_eq!(
        request(&router, "/audit/export?tenant=other")
            .await
            .status(),
        StatusCode::FORBIDDEN,
        "an export credential without audit:global stays tenant-bound"
    );
    harness.set_publisher(Some(oauth_publisher(
        "acme",
        &["audit:read", "audit:export", "audit:global"],
    )));
    assert_eq!(
        request(&router, "/audit/export?tenant=other&limit=1")
            .await
            .status(),
        StatusCode::OK,
        "audit:global explicitly permits a cross-tenant export"
    );

    let first = request(&router, "/audit/events?limit=1").await;
    let first_json: Value = serde_json::from_slice(
        &to_bytes(first.into_body(), usize::MAX)
            .await
            .expect("first page body"),
    )
    .expect("first page JSON");
    let cursor = first_json["next"].as_str().expect("next cursor");
    assert_eq!(
        request(&router, &format!("/audit/events?cursor={cursor}x"))
            .await
            .status(),
        StatusCode::BAD_REQUEST,
        "tampered cursors fail closed"
    );
    assert_eq!(
        request(
            &router,
            &format!("/audit/events?tenant=other&cursor={cursor}"),
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST,
        "cursors are tenant-bound even for a global reader"
    );

    harness.set_publisher(Some(oauth_publisher(
        "acme",
        &["audit:read", "audit:export"],
    )));
    let export = request(&router, "/audit/export?limit=1").await;
    assert_eq!(export.status(), StatusCode::OK);
    assert_eq!(
        export
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("application/x-ndjson; charset=utf-8")
    );
    assert_eq!(
        export
            .headers()
            .get(header::CACHE_CONTROL)
            .and_then(|value| value.to_str().ok()),
        Some("no-store")
    );
    assert!(export.headers().contains_key("x-audit-next"));
    assert_eq!(
        export
            .headers()
            .get("x-audit-truncated")
            .and_then(|value| value.to_str().ok()),
        Some("true")
    );
    let exported = to_bytes(export.into_body(), usize::MAX)
        .await
        .expect("export body");
    assert!(
        String::from_utf8(exported.to_vec())
            .expect("NDJSON text")
            .ends_with('\n')
    );
    harness.set_publisher(Some(oauth_publisher("acme", &["audit:read"])));
    assert_eq!(
        request(&router, "/audit/export").await.status(),
        StatusCode::FORBIDDEN,
        "export requires its distinct OAuth capability"
    );

    let _ = std::fs::remove_dir_all(data_dir);
}

#[tokio::test]
async fn scope_denial_is_not_rate_limit_but_single_operation_throttle_is() {
    let harness = Harness::admin();
    let mut config = AppConfig::defaults();
    config.ingress.reads_per_window = 1;
    let deps = harness.deps_with_config(config);
    let telemetry = deps.mcp_telemetry.clone();
    let router = artifact_mcp::build_router(deps);

    let before = telemetry.render_prometheus();
    let initial_rate = signal_value(&before, "rate_limit");

    harness.set_publisher(Some(oauth_publisher("acme", &[])));
    assert_eq!(
        mcp_list_artifacts(&router, 1).await.status(),
        StatusCode::FORBIDDEN
    );
    let after_scope = telemetry.render_prometheus();
    assert_eq!(signal_value(&after_scope, "rate_limit"), initial_rate);

    harness.set_publisher(Some(oauth_publisher("acme", &["artifacts:read"])));
    assert_eq!(
        mcp_list_artifacts(&router, 2).await.status(),
        StatusCode::OK
    );
    assert_eq!(
        mcp_list_artifacts(&router, 3).await.status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    let after_throttle = telemetry.render_prometheus();
    assert!(signal_value(&after_throttle, "rate_limit") > initial_rate);

    let batch_harness = Harness::admin();
    batch_harness.set_publisher(Some(oauth_publisher("acme", &["artifacts:read"])));
    let mut batch_config = AppConfig::defaults();
    batch_config.ingress.reads_per_window = 1;
    let batch_deps = batch_harness.deps_with_config(batch_config);
    let batch_telemetry = batch_deps.mcp_telemetry.clone();
    let batch_router = artifact_mcp::build_router(batch_deps);
    let batch = serde_json::json!([
        {"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"list_artifacts","arguments":{}}},
        {"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"list_artifacts","arguments":{}}}
    ]);
    let response = batch_router
        .clone()
        .oneshot(
            Request::post("/mcp")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(batch.to_string()))
                .expect("MCP batch request"),
        )
        .await
        .expect("MCP batch response");
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let batch_metrics = batch_telemetry.render_prometheus();
    assert!(signal_value(&batch_metrics, "rate_limit") > initial_rate);
}

#[tokio::test]
async fn audit_route_throttle_emits_the_sustained_rate_limit_signal_without_counting_scope_denials()
{
    let (mut scope_config, scope_dir) = audit_route_config("scope-denial");
    scope_config.ingress.reads_per_window = 10;
    let scope_harness = Harness::admin();
    scope_harness.set_publisher(Some(oauth_publisher("acme", &[])));
    let scope_deps = scope_harness.deps_with_config(scope_config);
    let scope_telemetry = scope_deps.mcp_telemetry.clone();
    let scope_router = artifact_mcp::build_router(scope_deps);
    let before_scope = signal_value(&scope_telemetry.render_prometheus(), "rate_limit");
    assert_eq!(
        request(&scope_router, "/audit/events").await.status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        signal_value(&scope_telemetry.render_prometheus(), "rate_limit"),
        before_scope,
        "a capability denial reaches the audit route but must not look like throttling"
    );

    let (mut throttle_config, throttle_dir) = audit_route_config("rate-limit");
    throttle_config.ingress.reads_per_window = 1;
    let throttle_harness = Harness::admin();
    throttle_harness.set_publisher(Some(oauth_publisher("acme", &["audit:read"])));
    let throttle_deps = throttle_harness.deps_with_config(throttle_config);
    let throttle_telemetry = throttle_deps.mcp_telemetry.clone();
    let throttle_router = artifact_mcp::build_router(throttle_deps);
    let before_throttle = signal_value(&throttle_telemetry.render_prometheus(), "rate_limit");
    assert_eq!(
        request(&throttle_router, "/audit/events").await.status(),
        StatusCode::OK
    );
    assert_eq!(
        request(&throttle_router, "/audit/export").await.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "ingress throttles this route before audit-route authentication/authorization"
    );
    assert_eq!(
        signal_value(&throttle_telemetry.render_prometheus(), "rate_limit"),
        before_throttle + 1,
        "the live audit-route throttle feeds the alert's fixed-cardinality counter"
    );
    let alerts = include_str!("../../ops/prometheus/artifact-mcp-alerts.yml");
    assert!(alerts.contains(
        "sum(rate(artifact_mcp_security_audit_signals_total{signal=\"rate_limit\"}[5m])) > 0.1"
    ));

    let _ = std::fs::remove_dir_all(scope_dir);
    let _ = std::fs::remove_dir_all(throttle_dir);
}

fn signal_value(metrics: &str, signal: &str) -> u64 {
    let prefix = format!("artifact_mcp_security_audit_signals_total{{signal=\"{signal}\"}} ");
    metrics
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .expect("security signal in metrics")
        .parse()
        .expect("numeric security signal")
}

#[tokio::test]
async fn security_signal_metrics_are_emitted_from_live_auth_rate_admin_integrity_and_reconciliation_paths()
 {
    let harness = Harness::admin();
    let mut config = AppConfig::defaults();
    config.ingress.auth_failures_per_window = 1;
    let router = artifact_mcp::build_router(harness.deps_with_config(config));

    for _ in 0..2 {
        let response = router
            .clone()
            .oneshot(
                Request::post("/mcp")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .expect("unauthenticated MCP request"),
            )
            .await
            .expect("unauthenticated MCP response");
        assert!(matches!(
            response.status(),
            StatusCode::UNAUTHORIZED | StatusCode::TOO_MANY_REQUESTS
        ));
    }
    let admin = router
        .clone()
        .oneshot(
            Request::post("/settings/orgs")
                .header("x-artifact-mutation", "1")
                .header("sec-fetch-site", "same-origin")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"metrics"}"#))
                .expect("admin mutation request"),
        )
        .await
        .expect("admin mutation response");
    assert_eq!(admin.status(), StatusCode::OK);
    let metrics = request(&router, "/metrics").await;
    let metrics = String::from_utf8(
        to_bytes(metrics.into_body(), usize::MAX)
            .await
            .expect("metrics body")
            .to_vec(),
    )
    .expect("metrics text");
    assert!(signal_value(&metrics, "auth_failure") >= 2);
    assert!(signal_value(&metrics, "rate_limit") >= 1);
    assert!(signal_value(&metrics, "admin_action") >= 1);

    let (integrity_config, integrity_dir) = audit_route_config("integrity");
    append_audit_events(&integrity_config, 1, 0);
    let key = artifact_mcp::security::audit::parse_hmac_key(AUDIT_KEY).expect("audit key");
    let before_integrity = signal_value(
        &artifact_mcp::observability::McpTelemetry::default().render_prometheus(),
        "integrity_failure",
    );
    {
        let mut conn = rusqlite::Connection::open(integrity_config.database_path())
            .expect("open audit database");
        conn.execute(
            "UPDATE security_audit_events SET event_hash='tampered' WHERE sequence=1",
            [],
        )
        .expect("tamper ledger for signal test");
        assert!(
            artifact_mcp::security::audit::prune_expired(
                &mut conn,
                &key,
                Some("2999-01-01T00:00:00Z"),
                1,
            )
            .is_err()
        );
    }
    assert!(
        signal_value(
            &artifact_mcp::observability::McpTelemetry::default().render_prometheus(),
            "integrity_failure",
        ) >= before_integrity.saturating_add(1),
        "the real retention integrity check increments the metric"
    );

    let (reconciliation_config, reconciliation_dir) = audit_route_config("reconciliation");
    {
        let pool = artifact_mcp::persistence::db::Database::open(&reconciliation_config)
            .expect("reconciliation database");
        drop(pool);
        let reconciliation_db = rusqlite::Connection::open(reconciliation_config.database_path())
            .expect("open reconciliation database");
        reconciliation_db
            .execute_batch("PRAGMA foreign_keys=OFF; DROP TABLE artifacts;")
            .expect("corrupt storage query fixture after migrations");
    }
    let before_reconciliation = signal_value(
        &artifact_mcp::observability::McpTelemetry::default().render_prometheus(),
        "reconciliation_failure",
    );
    let startup = super::u20_runtime::runtime::run_with_bind(
        reconciliation_config,
        Arc::new(AuditStartupObserver),
        |_host, _port, _router| async { Ok(()) },
    )
    .await;
    assert!(
        startup.is_err(),
        "blocked artifact storage fails startup reconciliation"
    );
    assert!(
        signal_value(
            &artifact_mcp::observability::McpTelemetry::default().render_prometheus(),
            "reconciliation_failure",
        ) >= before_reconciliation.saturating_add(1),
        "the runtime reconciliation error path increments the metric"
    );

    let _ = std::fs::remove_dir_all(integrity_dir);
    let _ = std::fs::remove_dir_all(reconciliation_dir);
}
