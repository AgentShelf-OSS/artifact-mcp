//! Route-level and cross-runtime parity proofs for U17.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path as FsPath, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
};

use artifact_mcp::{
    AppDeps, build_router,
    config::AppConfig,
    error::AppError,
    http::artifact_response::{ANCHOR_BRIDGE_MARKER, DOCUMENT_SANDBOX},
    model::*,
    ports::{
        AdminService, ArtifactDiscussionView, ArtifactService, BoxFuture, DiscussionConnectionView,
        DiscussionModeRequest, DiscussionService, EngagementService, HealthProbe, NotificationSink,
        PageRenderer, PreviewService, PublisherAuthenticator, ShareService, ViewerIdentity,
        integrations::{HealthReport, PreviewPriority},
    },
    render::view_models::{GalleryView, SettingsView, ShellView},
    security::access::{AuthorizedArtifact, OwnedArtifact},
};
use axum::{
    body::{Body, to_bytes},
    http::{HeaderMap, Method, Request, StatusCode, header},
    response::Response,
};
use serde_json::{Value, json};
use tower::ServiceExt;

const ID: &str = "abc123def456";
const DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const REQUIRE_NODE_REFERENCE: &str = "REQUIRE_NODE_REFERENCE";

#[derive(Default)]
struct FakeState {
    viewer: Viewer,
    meta: Option<ArtifactMeta>,
    body: Option<ArtifactFile>,
    bundle_files: BTreeMap<String, ArtifactFile>,
    revision_files: BTreeMap<(u64, Option<String>), ArtifactFile>,
    org_artifacts: Vec<ArtifactMeta>,
    grouped: Vec<OrgArtifacts>,
    org_ids: Vec<ArtifactId>,
    org_names: Vec<OrgId>,
    colors: BTreeMap<OrgId, Option<String>>,
    reaction: Reaction,
    reactions: BTreeMap<ArtifactId, Reaction>,
    sentiment: BTreeMap<ArtifactId, Sentiment>,
    counts: ViewCounts,
    counts_by_org: BTreeMap<ArtifactId, ViewCounts>,
    viewers: Vec<ViewerView>,
    top: Vec<TopViewedArtifact>,
    feedback: Vec<Feedback>,
    notifications: Vec<ViewerNotification>,
    unread: u64,
    share: Option<ShareGrant>,
    thumbnail: Option<Vec<u8>>,
    placeholder: Vec<u8>,
    failures: BTreeSet<String>,
    calls: Vec<String>,
    gallery_view: Option<GalleryView>,
    shell_view: Option<ShellView>,
}

#[derive(Clone, Default)]
struct Fake {
    state: Arc<Mutex<FakeState>>,
}

impl Fake {
    fn standard() -> Self {
        let fake = Self::default();
        let meta = artifact(false);
        let viewer = member();
        let mut state = fake.lock();
        state.viewer = viewer;
        state.meta = Some(meta.clone());
        state.body = Some(html_file("<h1>Artifact</h1>"));
        state.org_artifacts = vec![meta.clone()];
        state.grouped = vec![OrgArtifacts {
            org: OrgId::from("acme"),
            items: vec![meta.clone()],
        }];
        state.org_ids = vec![meta.id.clone()];
        state.org_names = vec![OrgId::from("acme")];
        state
            .colors
            .insert(OrgId::from("acme"), Some("#123456".to_owned()));
        state.share = Some(ShareGrant {
            artifact_id: meta.id,
            org: OrgId::from("acme"),
        });
        state.placeholder = b"placeholder".to_vec();
        drop(state);
        fake
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FakeState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn fail(&self, operation: &str) {
        self.lock().failures.insert(operation.to_owned());
    }

    fn calls(&self) -> Vec<String> {
        self.lock().calls.clone()
    }

    fn operation<T>(&self, name: &str, value: T) -> Result<T, AppError> {
        let mut state = self.lock();
        state.calls.push(name.to_owned());
        if state.failures.contains(name) {
            Err(AppError::Internal)
        } else {
            Ok(value)
        }
    }
}

fn artifact(is_bundle: bool) -> ArtifactMeta {
    ArtifactMeta {
        id: ArtifactId::from(ID),
        client_id: ClientId::from("publisher"),
        org: OrgId::from("acme"),
        title: "Raw Case".to_owned(),
        description: "fixture".to_owned(),
        bytes: 17,
        created_at: Timestamp("2026-07-21 00:00:00".to_owned()),
        updated_at: Timestamp("2026-07-21 00:00:00".to_owned()),
        uploader_label: "Agent".to_owned(),
        owner_email: None,
        is_bundle,
        entry: "index.html".to_owned(),
        revision: 2,
        category: String::new(),
        hidden: false,
        body_sha256: DIGEST.to_owned(),
    }
}

fn member() -> Viewer {
    Viewer {
        email: Some(EmailAddress::from("member@acme.test")),
        org: Some(OrgId::from("acme")),
        is_admin: false,
    }
}

fn html_file(content: &str) -> ArtifactFile {
    ArtifactFile {
        content: content.as_bytes().to_vec(),
        content_type: "text/html; charset=utf-8".to_owned(),
    }
}

fn unused<'a, T>() -> BoxFuture<'a, Result<T, AppError>> {
    Box::pin(async { Err(AppError::Unavailable("unused fake operation".to_owned())) })
}

impl ViewerIdentity for Fake {
    fn resolve<'a>(&'a self, _headers: &'a HeaderMap) -> BoxFuture<'a, Result<Viewer, AppError>> {
        Box::pin(async move {
            let viewer = self.lock().viewer.clone();
            self.operation("viewer.resolve", viewer)
        })
    }
}

impl DiscussionService for Fake {
    fn connection<'a>(
        &'a self,
        _org: &'a OrgId,
    ) -> BoxFuture<'a, Result<DiscussionConnectionView, AppError>> {
        Box::pin(async move {
            self.operation(
                "discussions.connection",
                DiscussionConnectionView {
                    configured: true,
                    label: "Forum".to_owned(),
                    destination: "discord.com/…/masked".to_owned(),
                    strategy: "notification_thread".to_owned(),
                    webhook_id: Some("webhook-a".to_owned()),
                    bot_configured: true,
                    last_error: None,
                },
            )
        })
    }

    fn configure_connection(
        &self,
        _org: OrgId,
        _webhook_id: String,
        _label: String,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'_, Result<DiscussionConnectionView, AppError>> {
        Box::pin(async move {
            self.operation(
                "discussions.configure",
                DiscussionConnectionView {
                    configured: true,
                    label: "Forum".to_owned(),
                    destination: "discord.com/…/masked".to_owned(),
                    strategy: "notification_thread".to_owned(),
                    webhook_id: Some("webhook-a".to_owned()),
                    bot_configured: true,
                    last_error: None,
                },
            )
        })
    }

    fn remove_connection(
        &self,
        _org: OrgId,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'_, Result<bool, AppError>> {
        Box::pin(async move { self.operation("discussions.remove", true) })
    }

    fn test_connection(
        &self,
        _org: OrgId,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'_, Result<bool, AppError>> {
        Box::pin(async move { self.operation("discussions.test", true) })
    }

    fn status<'a>(
        &'a self,
        _artifact: &'a ArtifactMeta,
    ) -> BoxFuture<'a, Result<ArtifactDiscussionView, AppError>> {
        Box::pin(async move {
            self.operation(
                "discussions.status",
                ArtifactDiscussionView {
                    mode: "artifact_only".to_owned(),
                    state: "local".to_owned(),
                    enabled: false,
                    connection_configured: true,
                    last_error: None,
                },
            )
        })
    }

    fn set_mode(
        &self,
        _artifact: ArtifactMeta,
        _mode: DiscussionModeRequest,
        _actor: String,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'_, Result<ArtifactDiscussionView, AppError>> {
        Box::pin(async move {
            self.operation(
                "discussions.set_mode",
                ArtifactDiscussionView {
                    mode: "discord_mirror".to_owned(),
                    state: "pending".to_owned(),
                    enabled: true,
                    connection_configured: true,
                    last_error: None,
                },
            )
        })
    }

    fn retry(
        &self,
        _artifact: ArtifactMeta,
        _actor: String,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'_, Result<ArtifactDiscussionView, AppError>> {
        Box::pin(async move {
            self.operation(
                "discussions.retry",
                ArtifactDiscussionView {
                    mode: "discord_mirror".to_owned(),
                    state: "pending".to_owned(),
                    enabled: true,
                    connection_configured: true,
                    last_error: None,
                },
            )
        })
    }
}

impl PublisherAuthenticator for Fake {
    fn authenticate<'a>(
        &'a self,
        _headers: &'a HeaderMap,
    ) -> BoxFuture<'a, Result<PublisherIdentity, AppError>> {
        unused()
    }
}

impl ArtifactService for Fake {
    fn find_meta<'a>(
        &'a self,
        id: &'a ArtifactId,
    ) -> BoxFuture<'a, Result<Option<ArtifactMeta>, AppError>> {
        Box::pin(async move {
            let meta = self.lock().meta.clone().filter(|meta| meta.id == *id);
            self.operation("artifacts.find_meta", meta)
        })
    }

    fn publish(
        &self,
        _request: PublishArtifact,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'_, Result<PublishedArtifact, AppError>> {
        unused()
    }

    fn list_for_publisher<'a>(
        &'a self,
        _publisher: &'a PublisherIdentity,
    ) -> BoxFuture<'a, Result<Vec<ArtifactMeta>, AppError>> {
        unused()
    }

    fn list_org_artifacts<'a>(
        &'a self,
        _org: &'a OrgId,
        _include_hidden: bool,
    ) -> BoxFuture<'a, Result<Vec<ArtifactMeta>, AppError>> {
        Box::pin(async move {
            let rows = self.lock().org_artifacts.clone();
            self.operation("artifacts.list_org_artifacts", rows)
        })
    }

    fn list_all_grouped_by_org(
        &self,
        _include_hidden: bool,
    ) -> BoxFuture<'_, Result<Vec<OrgArtifacts>, AppError>> {
        Box::pin(async move {
            let rows = self.lock().grouped.clone();
            self.operation("artifacts.list_all_grouped", rows)
        })
    }

    fn list_org_ids<'a>(
        &'a self,
        _org: &'a OrgId,
        _include_hidden: bool,
    ) -> BoxFuture<'a, Result<Vec<ArtifactId>, AppError>> {
        Box::pin(async move {
            let ids = self.lock().org_ids.clone();
            self.operation("artifacts.list_org_ids", ids)
        })
    }

    fn read_body<'a>(
        &'a self,
        _artifact: &'a AuthorizedArtifact,
    ) -> BoxFuture<'a, Result<Option<ArtifactFile>, AppError>> {
        Box::pin(async move {
            let body = self.lock().body.clone();
            self.operation("artifacts.read_body", body)
        })
    }

    fn read_bundle_file<'a>(
        &'a self,
        _artifact: &'a AuthorizedArtifact,
        relative_path: &'a str,
    ) -> BoxFuture<'a, Result<Option<ArtifactFile>, AppError>> {
        Box::pin(async move {
            let file = self.lock().bundle_files.get(relative_path).cloned();
            self.operation(&format!("artifacts.read_bundle:{relative_path}"), file)
        })
    }

    fn read_revision_body<'a>(
        &'a self,
        _artifact: &'a AuthorizedArtifact,
        revision: u64,
        relative_path: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Option<ArtifactFile>, AppError>> {
        Box::pin(async move {
            let key = (revision, relative_path.map(ToOwned::to_owned));
            let file = self.lock().revision_files.get(&key).cloned();
            self.operation(
                &format!("artifacts.read_revision:{revision}:{relative_path:?}"),
                file,
            )
        })
    }

    fn list_bundle_files<'a>(
        &'a self,
        _artifact: &'a AuthorizedArtifact,
        _revision: Option<u64>,
    ) -> BoxFuture<'a, Result<Option<Vec<(String, u64)>>, AppError>> {
        unused()
    }

    fn list_revisions<'a>(
        &'a self,
        _artifact: &'a AuthorizedArtifact,
    ) -> BoxFuture<'a, Result<RevisionHistory, AppError>> {
        unused()
    }

    fn update(
        &self,
        _artifact: AuthorizedArtifact,
        _update: ArtifactUpdate,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'_, Result<UpdateArtifactResult, AppError>> {
        unused()
    }

    fn restore(
        &self,
        _artifact: AuthorizedArtifact,
        _revision: u64,
        _acting_client_id: Option<ClientId>,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'_, Result<RestoreArtifactResult, AppError>> {
        unused()
    }

    fn delete(
        &self,
        _artifact: AuthorizedArtifact,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'_, Result<bool, AppError>> {
        unused()
    }

    fn set_category(
        &self,
        _artifact: AuthorizedArtifact,
        _category: String,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'_, Result<ArtifactMeta, AppError>> {
        unused()
    }

    fn set_hidden(
        &self,
        _artifact: AuthorizedArtifact,
        _hidden: bool,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'_, Result<ArtifactMeta, AppError>> {
        unused()
    }

    fn move_to_org(
        &self,
        _artifact: AuthorizedArtifact,
        _target_org: OrgId,
        _category: Option<String>,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'_, Result<ArtifactMeta, AppError>> {
        unused()
    }

    fn audit_storage(
        &self,
        _clean_transient: bool,
    ) -> BoxFuture<'_, Result<StorageAuditReport, AppError>> {
        unused()
    }

    fn backfill_body_digests(&self) -> BoxFuture<'_, Result<DigestBackfillReport, AppError>> {
        unused()
    }
}

impl AdminService for Fake {
    fn list_keys(&self) -> BoxFuture<'_, Result<Vec<PublisherKeySummary>, AppError>> {
        unused()
    }

    fn create_key(
        &self,
        _request: CreatePublisherKey,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'_, Result<CreatedPublisherKey, AppError>> {
        unused()
    }

    fn revoke_key<'a>(
        &'a self,
        _client_id: &'a ClientId,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'a, Result<bool, AppError>> {
        unused()
    }

    fn org_exists<'a>(&'a self, _org: &'a OrgId) -> BoxFuture<'a, Result<bool, AppError>> {
        unused()
    }

    fn org_for_domain<'a>(
        &'a self,
        _domain: &'a str,
    ) -> BoxFuture<'a, Result<Option<OrgId>, AppError>> {
        unused()
    }

    fn org_for_email<'a>(
        &'a self,
        _email: &'a EmailAddress,
    ) -> BoxFuture<'a, Result<Option<OrgId>, AppError>> {
        unused()
    }

    fn org_names(&self) -> BoxFuture<'_, Result<Vec<OrgId>, AppError>> {
        Box::pin(async move {
            let names = self.lock().org_names.clone();
            self.operation("admin.org_names", names)
        })
    }

    fn list_orgs(&self) -> BoxFuture<'_, Result<Vec<Organization>, AppError>> {
        unused()
    }

    fn create_org(
        &self,
        _request: CreateOrganization,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'_, Result<Organization, AppError>> {
        unused()
    }

    fn delete_org<'a>(
        &'a self,
        _org: &'a OrgId,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'a, Result<bool, AppError>> {
        unused()
    }

    fn add_domain<'a>(
        &'a self,
        _org: &'a OrgId,
        _domain: &'a str,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'a, Result<String, AppError>> {
        unused()
    }

    fn remove_domain<'a>(
        &'a self,
        _org: &'a OrgId,
        _domain: &'a str,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'a, Result<bool, AppError>> {
        unused()
    }

    fn add_email_member<'a>(
        &'a self,
        _org: &'a OrgId,
        _email: &'a EmailAddress,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'a, Result<EmailAddress, AppError>> {
        unused()
    }

    fn remove_email_member<'a>(
        &'a self,
        _org: &'a OrgId,
        _email: &'a EmailAddress,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'a, Result<bool, AppError>> {
        unused()
    }

    fn categories<'a>(&'a self, _org: &'a OrgId) -> BoxFuture<'a, Result<Vec<String>, AppError>> {
        unused()
    }

    fn add_category<'a>(
        &'a self,
        _org: &'a OrgId,
        _name: &'a str,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'a, Result<String, AppError>> {
        unused()
    }

    fn remove_category<'a>(
        &'a self,
        _org: &'a OrgId,
        _name: &'a str,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'a, Result<bool, AppError>> {
        unused()
    }

    fn color_map(&self) -> BoxFuture<'_, Result<BTreeMap<OrgId, Option<String>>, AppError>> {
        Box::pin(async move {
            let colors = self.lock().colors.clone();
            self.operation("admin.color_map", colors)
        })
    }

    fn set_color<'a>(
        &'a self,
        _org: &'a OrgId,
        _color: Option<&'a str>,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'a, Result<Option<String>, AppError>> {
        unused()
    }

    fn list_webhooks<'a>(
        &'a self,
        _org: &'a OrgId,
    ) -> BoxFuture<'a, Result<Vec<WebhookSummary>, AppError>> {
        unused()
    }

    fn create_webhook(
        &self,
        _request: CreateWebhook,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'_, Result<WebhookSummary, AppError>> {
        unused()
    }

    fn remove_webhook<'a>(
        &'a self,
        _org: &'a OrgId,
        _id: &'a WebhookId,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'a, Result<bool, AppError>> {
        unused()
    }

    fn set_webhook_events<'a>(
        &'a self,
        _org: &'a OrgId,
        _id: &'a WebhookId,
        _events: &'a [WebhookEvent],
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'a, Result<Option<WebhookSummary>, AppError>> {
        unused()
    }

    fn webhook_delivery<'a>(
        &'a self,
        _id: &'a WebhookId,
    ) -> BoxFuture<'a, Result<Option<WebhookDelivery>, AppError>> {
        unused()
    }
}

impl EngagementService for Fake {
    fn reaction<'a>(
        &'a self,
        _artifact: &'a AuthorizedArtifact,
        _viewer: &'a Viewer,
    ) -> BoxFuture<'a, Result<Reaction, AppError>> {
        Box::pin(async move {
            let reaction = self.lock().reaction;
            self.operation("engagement.reaction", reaction)
        })
    }

    fn set_reaction(
        &self,
        _artifact: AuthorizedArtifact,
        _viewer: Viewer,
        _update: ReactionUpdate,
    ) -> BoxFuture<'_, Result<Reaction, AppError>> {
        unused()
    }

    fn reactions_for_viewer<'a>(
        &'a self,
        _viewer: &'a Viewer,
    ) -> BoxFuture<'a, Result<BTreeMap<ArtifactId, Reaction>, AppError>> {
        Box::pin(async move {
            let reactions = self.lock().reactions.clone();
            self.operation("engagement.reactions_for_viewer", reactions)
        })
    }

    fn sentiment(&self) -> BoxFuture<'_, Result<BTreeMap<ArtifactId, Sentiment>, AppError>> {
        Box::pin(async move {
            let sentiment = self.lock().sentiment.clone();
            self.operation("engagement.sentiment", sentiment)
        })
    }

    fn record_view<'a>(
        &'a self,
        _artifact: &'a AuthorizedArtifact,
        _viewer: &'a Viewer,
    ) -> BoxFuture<'a, Result<(), AppError>> {
        Box::pin(async move { self.operation("engagement.record_view", ()) })
    }

    fn view_counts<'a>(
        &'a self,
        _artifact: &'a AuthorizedArtifact,
    ) -> BoxFuture<'a, Result<ViewCounts, AppError>> {
        Box::pin(async move {
            let counts = self.lock().counts.clone();
            self.operation("engagement.view_counts", counts)
        })
    }

    fn view_counts_for_org<'a>(
        &'a self,
        _org: &'a OrgId,
    ) -> BoxFuture<'a, Result<BTreeMap<ArtifactId, ViewCounts>, AppError>> {
        Box::pin(async move {
            let counts = self.lock().counts_by_org.clone();
            self.operation("engagement.view_counts_for_org", counts)
        })
    }

    fn viewers<'a>(
        &'a self,
        _artifact: &'a AuthorizedArtifact,
    ) -> BoxFuture<'a, Result<Vec<ViewerView>, AppError>> {
        Box::pin(async move {
            let viewers = self.lock().viewers.clone();
            self.operation("engagement.viewers", viewers)
        })
    }

    fn top_for_org<'a>(
        &'a self,
        _org: &'a OrgId,
        _limit: usize,
    ) -> BoxFuture<'a, Result<Vec<TopViewedArtifact>, AppError>> {
        Box::pin(async move {
            let top = self.lock().top.clone();
            self.operation("engagement.top_for_org", top)
        })
    }

    fn feedback_ref<'a>(
        &'a self,
        _id: &'a FeedbackId,
    ) -> BoxFuture<'a, Result<Option<FeedbackRef>, AppError>> {
        unused()
    }

    fn list_feedback<'a>(
        &'a self,
        _artifact: &'a AuthorizedArtifact,
    ) -> BoxFuture<'a, Result<Vec<Feedback>, AppError>> {
        Box::pin(async move {
            let feedback = self.lock().feedback.clone();
            self.operation("engagement.list_feedback", feedback)
        })
    }

    fn submit_feedback(
        &self,
        _artifact: AuthorizedArtifact,
        _submission: SubmitFeedback,
    ) -> BoxFuture<'_, Result<Feedback, AppError>> {
        unused()
    }

    fn delete_feedback(
        &self,
        _artifact: AuthorizedArtifact,
        _viewer: Viewer,
        _id: FeedbackId,
    ) -> BoxFuture<'_, Result<FeedbackMutation, AppError>> {
        unused()
    }

    fn resolve_feedback_as_viewer(
        &self,
        _artifact: AuthorizedArtifact,
        _viewer: Viewer,
        _id: FeedbackId,
    ) -> BoxFuture<'_, Result<FeedbackMutation, AppError>> {
        unused()
    }

    fn list_feedback_for_publisher<'a>(
        &'a self,
        _publisher: &'a PublisherIdentity,
        _artifact: Option<&'a OwnedArtifact>,
    ) -> BoxFuture<'a, Result<Vec<Feedback>, AppError>> {
        unused()
    }

    fn resolve_feedback_as_publisher(
        &self,
        _artifact: OwnedArtifact,
        _id: FeedbackId,
        _resolved_by: String,
    ) -> BoxFuture<'_, Result<bool, AppError>> {
        unused()
    }

    fn reopen_feedback_as_publisher(
        &self,
        _artifact: OwnedArtifact,
        _id: FeedbackId,
    ) -> BoxFuture<'_, Result<bool, AppError>> {
        unused()
    }

    fn recent_notifications<'a>(
        &'a self,
        _viewer: &'a Viewer,
        _limit: usize,
    ) -> BoxFuture<'a, Result<Vec<ViewerNotification>, AppError>> {
        Box::pin(async move {
            let notifications = self.lock().notifications.clone();
            self.operation("engagement.recent_notifications", notifications)
        })
    }

    fn unread_notifications<'a>(
        &'a self,
        _viewer: &'a Viewer,
    ) -> BoxFuture<'a, Result<u64, AppError>> {
        Box::pin(async move {
            let unread = self.lock().unread;
            self.operation("engagement.unread_notifications", unread)
        })
    }

    fn mark_notifications_seen<'a>(
        &'a self,
        _email: &'a EmailAddress,
    ) -> BoxFuture<'a, Result<(), AppError>> {
        Box::pin(async move { self.operation("engagement.mark_notifications_seen", ()) })
    }
}

impl ShareService for Fake {
    fn resolve<'a>(
        &'a self,
        _token: &'a ShareToken,
    ) -> BoxFuture<'a, Result<Option<ShareGrant>, AppError>> {
        Box::pin(async move {
            let share = self.lock().share.clone();
            self.operation("shares.resolve", share)
        })
    }

    fn create(
        &self,
        _artifact: AuthorizedArtifact,
        _request: CreateShare,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'_, Result<PublicShare, AppError>> {
        unused()
    }

    fn list<'a>(
        &'a self,
        _artifact: &'a AuthorizedArtifact,
    ) -> BoxFuture<'a, Result<Vec<PublicShare>, AppError>> {
        unused()
    }

    fn revoke(
        &self,
        _artifact: AuthorizedArtifact,
        _token: ShareToken,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'_, Result<bool, AppError>> {
        unused()
    }
}

impl PageRenderer for Fake {
    fn gallery(&self, view: &GalleryView) -> Result<String, AppError> {
        let mut state = self.lock();
        state.calls.push("pages.gallery".to_owned());
        state.gallery_view = Some(view.clone());
        Ok("gallery".to_owned())
    }

    fn shell(&self, view: &ShellView) -> Result<String, AppError> {
        let mut state = self.lock();
        state.calls.push("pages.shell".to_owned());
        state.shell_view = Some(view.clone());
        Ok("shell".to_owned())
    }

    fn settings(&self, _view: &SettingsView) -> Result<String, AppError> {
        Err(AppError::Unavailable("unused fake operation".to_owned()))
    }

    fn not_found(&self, _message: Option<&str>) -> Result<String, AppError> {
        self.lock().calls.push("pages.not_found".to_owned());
        Ok("not found".to_owned())
    }

    fn not_signed_in(&self) -> Result<String, AppError> {
        self.lock().calls.push("pages.not_signed_in".to_owned());
        Ok("not signed in".to_owned())
    }

    fn access_retry(&self, _target: &str) -> Result<String, AppError> {
        Err(AppError::Unavailable("unused fake operation".to_owned()))
    }
}

impl PreviewService for Fake {
    fn enabled(&self) -> bool {
        true
    }

    fn read_thumbnail<'a>(
        &'a self,
        _artifact: &'a AuthorizedArtifact,
        _digest: &'a str,
    ) -> BoxFuture<'a, Result<Option<Vec<u8>>, AppError>> {
        Box::pin(async move {
            let png = self.lock().thumbnail.clone();
            self.operation("previews.read_thumbnail", png)
        })
    }

    fn read_thumbnail_sync(
        &self,
        _meta: &ArtifactMeta,
        _digest: &str,
    ) -> Result<Option<Vec<u8>>, AppError> {
        let png = self.lock().thumbnail.clone();
        self.operation("previews.read_thumbnail", png)
    }

    fn placeholder(&self, _meta: &ArtifactMeta, _accent: Option<&str>) -> Vec<u8> {
        let mut state = self.lock();
        state.calls.push("previews.placeholder".to_owned());
        state.placeholder.clone()
    }

    fn ensure_thumbnail<'a>(
        &'a self,
        _meta: &'a ArtifactMeta,
        _html: &'a str,
        _priority: PreviewPriority,
    ) -> BoxFuture<'a, Result<Option<Vec<u8>>, AppError>> {
        unused()
    }

    fn remove_artifact<'a>(&'a self, _id: &'a ArtifactId) -> BoxFuture<'a, Result<(), AppError>> {
        unused()
    }
}

impl NotificationSink for Fake {
    fn emit(
        &self,
        _event: WebhookEvent,
        _org: OrgId,
        _payload: NotificationPayload,
    ) -> BoxFuture<'_, Result<(), AppError>> {
        unused()
    }

    fn test<'a>(
        &'a self,
        _webhook: &'a WebhookDelivery,
    ) -> BoxFuture<'a, Result<DeliveryResult, AppError>> {
        unused()
    }
}

impl HealthProbe for Fake {
    fn check(&self) -> BoxFuture<'_, Result<HealthReport, AppError>> {
        Box::pin(async { Ok(HealthReport::ok()) })
    }
}

fn deps(fake: &Fake) -> AppDeps {
    deps_with_config(fake, AppConfig::default())
}

fn deps_with_config(fake: &Fake, config: AppConfig) -> AppDeps {
    let fake = Arc::new(fake.clone());
    AppDeps {
        publisher_auth: fake.clone(),
        viewer_identity: fake.clone(),
        artifacts: fake.clone(),
        admin: fake.clone(),
        discussions: fake.clone(),
        engagement: fake.clone(),
        shares: fake.clone(),
        pages: fake.clone(),
        previews: fake.clone(),
        notifications: fake.clone(),
        health: fake,
        ingress: Arc::new(artifact_mcp::http::ingress::IngressState::from_config(
            &config,
        )),
        preview_tasks: artifact_mcp::mcp::tasks::PreviewTaskStore::new(
            std::env::temp_dir().join(format!("artifact-mcp-u17-tasks-{}", std::process::id())),
        ),
        mcp_telemetry: artifact_mcp::observability::McpTelemetry::default(),
        delivery_telemetry:
            artifact_mcp::integrations::delivery_runtime::DeliveryTelemetry::default(),
        delivery_wake: artifact_mcp::integrations::delivery_runtime::DeliveryWakeSignal::default(),
        audit_access: None,
        config: Arc::new(config),
    }
}

#[tokio::test]
async fn resolved_share_variants_share_the_canonical_verified_budget_while_invalid_tokens_stay_404()
{
    let fake = Fake::standard();
    let mut config = AppConfig::default();
    config.ingress.shares_per_window = 1;
    config.ingress.reads_per_window = 20;
    let app = build_router(deps_with_config(&fake, config));
    let first = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/s/share%2Dtoken")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    let canonical = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/s/share-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(canonical.status(), StatusCode::TOO_MANY_REQUESTS);
    let mut invalid_config = AppConfig::default();
    invalid_config.ingress.shares_per_window = 20;
    fake.lock().share = None;
    let missing = build_router(deps_with_config(&fake, invalid_config))
        .oneshot(
            Request::builder()
                .uri("/s/not-a-share")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn resolved_viewers_are_scoped_by_verified_identity_not_cookie_variants_on_one_nat() {
    let fake = Fake::standard();
    let mut config = AppConfig::default();
    config.ingress.reads_per_window = 20;
    config.ingress.verified_viewers_per_window = 1;
    let app = build_router(deps_with_config(&fake, config));
    let request = |cookie: &'static str| {
        Request::builder()
            .uri(format!("/raw/{ID}"))
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap()
    };
    let first = app.clone().oneshot(request("session=one")).await.unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    let alternate_cookie = app
        .clone()
        .oneshot(request("session=rotated"))
        .await
        .unwrap();
    assert_eq!(alternate_cookie.status(), StatusCode::TOO_MANY_REQUESTS);
    fake.lock().viewer = Viewer {
        email: Some(EmailAddress::from("other@acme.test")),
        org: Some(OrgId::from("acme")),
        is_admin: false,
    };
    let second_viewer = app.oneshot(request("session=other")).await.unwrap();
    assert_eq!(second_viewer.status(), StatusCode::OK);
}

async fn invoke(fake: &Fake, method: Method, uri: &str) -> Response {
    build_router(deps(fake))
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("route response")
}

async fn response_body(response: Response) -> Vec<u8> {
    to_bytes(response.into_body(), 128 * 1024)
        .await
        .expect("response body")
        .to_vec()
}

async fn snapshot(fake: &Fake, method: Method, uri: &str) -> (StatusCode, HeaderMap, Vec<u8>) {
    let response = invoke(fake, method, uri).await;
    let status = response.status();
    let headers = response.headers().clone();
    let body = response_body(response).await;
    (status, headers, body)
}

async fn discussion_request(
    fake: &Fake,
    method: Method,
    uri: &str,
    body: &'static str,
) -> Response {
    build_router(deps(fake))
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .expect("discussion request"),
        )
        .await
        .expect("discussion response")
}

#[tokio::test]
async fn discussion_connection_routes_are_admin_only_and_never_echo_a_webhook_secret() {
    let non_admin = Fake::standard();
    let denied = invoke(
        &non_admin,
        Method::GET,
        "/settings/orgs/acme/discord-discussion",
    )
    .await;
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    assert!(
        !non_admin
            .calls()
            .iter()
            .any(|call| call.starts_with("discussions."))
    );
    for (method, path, body) in [
        (
            Method::PUT,
            "/settings/orgs/acme/discord-discussion",
            r#"{"webhookId":"webhook-a","label":"Artifact thread"}"#,
        ),
        (Method::DELETE, "/settings/orgs/acme/discord-discussion", ""),
        (
            Method::POST,
            "/settings/orgs/acme/discord-discussion/test",
            "{}",
        ),
    ] {
        let response = discussion_request(&non_admin, method, path, body).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
    }

    let admin = Fake::standard();
    admin.lock().viewer.is_admin = true;
    let get = invoke(
        &admin,
        Method::GET,
        "/settings/orgs/acme/discord-discussion",
    )
    .await;
    assert_eq!(get.status(), StatusCode::OK);
    let body = response_body(get).await;
    assert!(
        body.windows(b"masked".len())
            .any(|window| window == b"masked")
    );
    assert!(
        !body
            .windows(b"secret".len())
            .any(|window| window == b"secret")
    );

    for (method, path, body, call) in [
        (
            Method::PUT,
            "/settings/orgs/acme/discord-discussion",
            r#"{"webhookId":"webhook-a","label":"Artifact thread"}"#,
            "discussions.configure",
        ),
        (
            Method::DELETE,
            "/settings/orgs/acme/discord-discussion",
            "",
            "discussions.remove",
        ),
        (
            Method::POST,
            "/settings/orgs/acme/discord-discussion/test",
            "{}",
            "discussions.test",
        ),
    ] {
        let response = discussion_request(&admin, method, path, body).await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        assert!(admin.calls().iter().any(|seen| seen == call));
    }
    let invalid_configure = discussion_request(
        &admin,
        Method::PUT,
        "/settings/orgs/acme/discord-discussion",
        r#"{"webhookId":"webhook-a","label":"Artifact thread","extra":true}"#,
    )
    .await;
    assert_eq!(invalid_configure.status(), StatusCode::BAD_REQUEST);
    let invalid_test = discussion_request(
        &admin,
        Method::POST,
        "/settings/orgs/acme/discord-discussion/test",
        r#"{"unexpected":true}"#,
    )
    .await;
    assert_eq!(invalid_test.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn discussion_artifact_routes_conceal_before_body_work_and_enforce_owner_or_admin() {
    let owner = Fake::standard();
    {
        let mut state = owner.lock();
        state.meta.as_mut().expect("meta").owner_email = Some("member@acme.test".to_owned());
    }
    let exact = discussion_request(
        &owner,
        Method::PUT,
        &format!("/{ID}/discussion"),
        r#"{"mode":"discord_mirror"}"#,
    )
    .await;
    assert_eq!(exact.status(), StatusCode::OK);
    assert!(
        owner
            .calls()
            .iter()
            .any(|call| call == "discussions.set_mode")
    );
    let safe_status = invoke(&owner, Method::GET, &format!("/{ID}/discussion")).await;
    let safe_status_body = response_body(safe_status).await;
    assert!(
        safe_status_body
            .windows(b"artifact_only".len())
            .any(|part| part == b"artifact_only")
    );
    assert!(
        !safe_status_body
            .windows(b"webhooks".len())
            .any(|part| part == b"webhooks")
    );

    let invalid = discussion_request(
        &owner,
        Method::PUT,
        &format!("/{ID}/discussion"),
        r#"{"mode":"discord_mirror","extra":true}"#,
    )
    .await;
    assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);

    let retry = discussion_request(
        &owner,
        Method::POST,
        &format!("/{ID}/discussion/retry"),
        "{}",
    )
    .await;
    assert_eq!(retry.status(), StatusCode::OK);
    let retry_invalid = discussion_request(
        &owner,
        Method::POST,
        &format!("/{ID}/discussion/retry"),
        r#"{"unexpected":true}"#,
    )
    .await;
    assert_eq!(retry_invalid.status(), StatusCode::BAD_REQUEST);

    let admin = Fake::standard();
    admin.lock().viewer.is_admin = true;
    let admin_set = discussion_request(
        &admin,
        Method::PUT,
        &format!("/{ID}/discussion"),
        r#"{"mode":"artifact_only"}"#,
    )
    .await;
    assert_eq!(admin_set.status(), StatusCode::OK);

    let non_owner = Fake::standard();
    non_owner.lock().meta.as_mut().expect("meta").owner_email = Some("owner@acme.test".to_owned());
    let denied = discussion_request(
        &non_owner,
        Method::POST,
        &format!("/{ID}/discussion/retry"),
        "not json",
    )
    .await;
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    assert!(
        !non_owner
            .calls()
            .iter()
            .any(|call| call == "discussions.retry")
    );
    let oversized = build_router(deps(&non_owner))
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri(format!("/{ID}/discussion"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("x".repeat(128 * 1024)))
                .expect("oversized request"),
        )
        .await
        .expect("oversized response");
    assert_eq!(oversized.status(), StatusCode::FORBIDDEN);

    let csrf = build_router(deps(&owner))
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri(format!("/{ID}/discussion"))
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::COOKIE, "CF_Authorization=session")
                .body(Body::from(r#"{"mode":"artifact_only"}"#))
                .expect("csrf request"),
        )
        .await
        .expect("csrf response");
    assert_eq!(csrf.status(), StatusCode::FORBIDDEN);

    let foreign = Fake::standard();
    foreign.lock().viewer.org = Some(OrgId::from("other"));
    let missing = Fake::standard();
    missing.lock().viewer.org = Some(OrgId::from("other"));
    missing.lock().meta = None;
    let foreign_response = discussion_request(
        &foreign,
        Method::PUT,
        &format!("/{ID}/discussion"),
        "not json",
    )
    .await;
    let missing_response = discussion_request(
        &missing,
        Method::PUT,
        &format!("/{ID}/discussion"),
        "not json",
    )
    .await;
    assert_eq!(foreign_response.status(), StatusCode::NOT_FOUND);
    assert_eq!(missing_response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        response_body(foreign_response).await,
        response_body(missing_response).await
    );
}

#[tokio::test]
async fn foreign_and_missing_viewer_routes_are_byte_identical_and_read_nothing_subordinate() {
    for uri in [
        format!("/{ID}"),
        format!("/raw/{ID}"),
        format!("/thumbnails/{ID}"),
    ] {
        let foreign = Fake::standard();
        foreign.lock().viewer = Viewer {
            email: Some(EmailAddress::from("intruder@other.test")),
            org: Some(OrgId::from("other")),
            is_admin: false,
        };
        let missing = Fake::standard();
        missing.lock().viewer = Viewer {
            email: Some(EmailAddress::from("intruder@other.test")),
            org: Some(OrgId::from("other")),
            is_admin: false,
        };
        missing.lock().meta = None;

        let foreign_response = snapshot(&foreign, Method::GET, &uri).await;
        let missing_response = snapshot(&missing, Method::GET, &uri).await;
        assert_eq!(foreign_response.0, StatusCode::NOT_FOUND, "{uri}");
        assert_eq!(foreign_response, missing_response, "{uri}");
        assert_eq!(foreign_response.2, b"not found", "{uri}");

        for calls in [foreign.calls(), missing.calls()] {
            assert_eq!(
                calls,
                ["viewer.resolve", "artifacts.find_meta", "pages.not_found"],
                "{uri}"
            );
        }
    }
}

#[tokio::test]
async fn raw_current_bundle_history_and_public_share_delivery_use_the_u14_policy() {
    let single = Fake::standard();
    let plain = snapshot(&single, Method::GET, &format!("/raw/{ID}")).await;
    assert_eq!(plain.0, StatusCode::OK);
    assert_eq!(plain.1[header::CONTENT_SECURITY_POLICY], DOCUMENT_SANDBOX);
    assert_eq!(plain.2, b"<h1>Artifact</h1>");
    assert!(
        !single
            .calls()
            .contains(&"engagement.record_view".to_owned())
    );

    let anchored = snapshot(&single, Method::GET, &format!("/raw/{ID}?anchor=1")).await;
    assert!(String::from_utf8_lossy(&anchored.2).contains(ANCHOR_BRIDGE_MARKER));
    let download = snapshot(&single, Method::GET, &format!("/raw/{ID}?download")).await;
    assert_eq!(
        download.1[header::CONTENT_DISPOSITION],
        "attachment; filename=\"Raw-Case.html\""
    );
    assert!(!String::from_utf8_lossy(&download.2).contains(ANCHOR_BRIDGE_MARKER));

    let bundle = Fake::standard();
    {
        let mut state = bundle.lock();
        state.meta = Some(artifact(true));
        state
            .bundle_files
            .insert(String::new(), html_file("<h1>Entry</h1>"));
        state.bundle_files.insert(
            "style.css".to_owned(),
            ArtifactFile {
                content: b"body{}".to_vec(),
                content_type: "text/css; charset=utf-8".to_owned(),
            },
        );
        state
            .revision_files
            .insert((1, Some(String::new())), html_file("<h1>Old entry</h1>"));
    }
    let redirect = snapshot(&bundle, Method::GET, &format!("/raw/{ID}")).await;
    assert_eq!(redirect.0, StatusCode::FOUND);
    assert_eq!(redirect.1[header::LOCATION], format!("/raw/{ID}/"));
    let entry = snapshot(&bundle, Method::GET, &format!("/raw/{ID}/")).await;
    assert_eq!(entry.0, StatusCode::OK);
    assert_eq!(entry.2, b"<h1>Entry</h1>");
    let css = snapshot(&bundle, Method::GET, &format!("/raw/{ID}/style.css")).await;
    assert_eq!(css.1[header::CONTENT_TYPE], "text/css; charset=utf-8");
    assert_eq!(css.1[header::CONTENT_SECURITY_POLICY], DOCUMENT_SANDBOX);
    let revision_redirect = snapshot(&bundle, Method::GET, &format!("/raw/{ID}/rev/01")).await;
    assert_eq!(
        revision_redirect.1[header::LOCATION],
        format!("/raw/{ID}/rev/1/")
    );
    let revision = snapshot(&bundle, Method::GET, &format!("/raw/{ID}/rev/1/")).await;
    assert_eq!(revision.2, b"<h1>Old entry</h1>");

    let shared = Fake::standard();
    shared.fail("viewer.resolve");
    let public = snapshot(&shared, Method::GET, "/s/share-token").await;
    assert_eq!(public.0, StatusCode::OK);
    assert_eq!(public.1[header::CACHE_CONTROL], "no-store");
    assert_eq!(public.1["x-robots-tag"], "noindex");
    assert_eq!(public.1[header::CONTENT_SECURITY_POLICY], DOCUMENT_SANDBOX);
    assert!(!shared.calls().contains(&"viewer.resolve".to_owned()));
}

#[tokio::test]
async fn thumbnails_are_identity_gated_and_bound_to_one_current_digest_string() {
    let current = Fake::standard();
    current.lock().thumbnail = Some(b"persisted png".to_vec());
    let png = snapshot(
        &current,
        Method::GET,
        &format!("/thumbnails/{ID}?v={DIGEST}"),
    )
    .await;
    assert_eq!(png.0, StatusCode::OK);
    assert_eq!(png.1[header::CONTENT_TYPE], "image/png");
    assert_eq!(
        png.1[header::CACHE_CONTROL],
        "private, max-age=31536000, immutable"
    );
    assert_eq!(png.2, b"persisted png");

    let duplicate = Fake::standard();
    duplicate.lock().thumbnail = Some(b"must not leak".to_vec());
    let placeholder = snapshot(
        &duplicate,
        Method::GET,
        &format!("/thumbnails/{ID}?v={DIGEST}&v={DIGEST}"),
    )
    .await;
    assert_eq!(
        placeholder.1[header::CONTENT_TYPE],
        "image/svg+xml; charset=utf-8"
    );
    assert_eq!(placeholder.1[header::CACHE_CONTROL], "no-store");
    assert_eq!(placeholder.2, b"placeholder");
    assert!(
        !duplicate
            .calls()
            .contains(&"previews.read_thumbnail".to_owned())
    );
}

#[tokio::test]
async fn analytics_failures_are_best_effort_but_notification_failures_are_not() {
    let gallery = Fake::standard();
    gallery.fail("engagement.view_counts_for_org");
    let response = snapshot(&gallery, Method::GET, "/").await;
    assert_eq!(response.0, StatusCode::OK);
    assert_eq!(response.2, b"gallery");
    let view = gallery.lock().gallery_view.clone().expect("gallery view");
    assert!(view.view_counts.is_empty());
    assert!(
        gallery
            .calls()
            .contains(&"engagement.recent_notifications".to_owned())
    );

    let notifications = Fake::standard();
    notifications.fail("engagement.recent_notifications");
    let failed = snapshot(&notifications, Method::GET, "/").await;
    assert_eq!(failed.0, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(failed.1[header::CACHE_CONTROL], "no-store");
    assert_eq!(failed.2, br#"{"error":"internal error"}"#);

    let shell = Fake::standard();
    shell.fail("engagement.record_view");
    shell.fail("engagement.view_counts");
    let rendered = snapshot(&shell, Method::GET, &format!("/{ID}")).await;
    assert_eq!(rendered.0, StatusCode::OK);
    assert_eq!(rendered.2, b"shell");
    let view = shell.lock().shell_view.clone().expect("shell view");
    assert_eq!(view.view_counts, ViewCounts::default());
    assert!(view.viewers.is_none());

    let mark = Fake::standard();
    mark.fail("engagement.mark_notifications_seen");
    let failed = snapshot(&mark, Method::POST, "/notifications/seen").await;
    assert_eq!(failed.0, StatusCode::INTERNAL_SERVER_ERROR);
    let unsigned = Fake::standard();
    unsigned.lock().viewer = Viewer::default();
    let denied = snapshot(&unsigned, Method::POST, "/notifications/seen").await;
    assert_eq!(denied.0, StatusCode::FORBIDDEN);
    assert_eq!(denied.2, br#"{"error":"Not signed in."}"#);
    assert!(
        !unsigned
            .calls()
            .contains(&"engagement.mark_notifications_seen".to_owned())
    );
}

#[tokio::test]
async fn gallery_preserves_admin_registry_union_order_and_unsigned_cache_policy() {
    let admin = Fake::standard();
    {
        let mut state = admin.lock();
        state.viewer = Viewer {
            email: Some(EmailAddress::from("admin@example.test")),
            org: Some(OrgId::from("admin")),
            is_admin: true,
        };
        let mut shadow = artifact(false);
        shadow.id = ArtifactId::from("def456abc123");
        shadow.org = OrgId::from("shadow");
        state.org_names = vec![OrgId::from("empty"), OrgId::from("acme")];
        state.grouped.push(OrgArtifacts {
            org: OrgId::from("shadow"),
            items: vec![shadow],
        });
        state.counts_by_org.insert(
            ArtifactId::from(ID),
            ViewCounts {
                views: 7,
                unique_viewers: 3,
                last_viewed_at: None,
            },
        );
        state
            .sentiment
            .insert(ArtifactId::from(ID), Sentiment::default());
        state.unread = 4;
    }

    let rendered = snapshot(&admin, Method::GET, "/").await;
    assert_eq!(rendered.0, StatusCode::OK);
    assert_eq!(rendered.1[header::CACHE_CONTROL], "no-store");
    assert_eq!(rendered.2, b"gallery");
    let view = admin.lock().gallery_view.clone().expect("gallery view");
    assert_eq!(
        view.sections
            .iter()
            .map(|section| section.org.0.as_str())
            .collect::<Vec<_>>(),
        ["empty", "acme", "shadow"]
    );
    assert!(view.sections[0].items.is_empty());
    assert_eq!(view.view_counts[&ArtifactId::from(ID)].views, 7);
    assert_eq!(view.top_viewed.keys().count(), 3);
    assert_eq!(view.sentiment.len(), 1);
    assert_eq!(view.unread_notifications, 4);

    let unsigned = Fake::standard();
    unsigned.lock().viewer = Viewer::default();
    let denied = snapshot(&unsigned, Method::GET, "/").await;
    assert_eq!(denied.0, StatusCode::FORBIDDEN);
    assert_eq!(denied.1[header::CACHE_CONTROL], "no-store");
    assert_eq!(denied.2, b"not signed in");
    assert_eq!(unsigned.calls(), ["viewer.resolve", "pages.not_signed_in"]);
}

const NODE_DRIVER: &str = r#"
import(process.argv[1]).then(async ({ createApp }) => {
  const artifact = { id: 'abc123def456', org: 'acme', title: 'Raw Case', client_id: 'publisher', is_bundle: 0, entry: 'index.html', revision: 2, body_sha256: 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa' };
  const deps = {
    checkPublisherKey: () => ({ ok: false }), handleMcp: async () => null,
    resolveViewer: async () => ({ email: 'member@acme.test', org: 'acme', isAdmin: false }),
    artifacts: {
      isReserved: () => false, getArtifactMeta: () => artifact,
      readArtifact: () => ({ html: '<h1>Artifact</h1>' }),
      readBundleFile: () => null, readHistoryArtifact: () => null, readHistoryBundleFile: () => null,
      listOrgArtifacts: () => [], listAllGroupedByOrg: () => new Map(), listOrgIds: () => [artifact.id]
    },
    shares: { resolve: () => ({ artifact_id: artifact.id, org: artifact.org }) },
    keys: { list: () => [] }, orgs: { names: () => [], colorMap: () => ({}) },
    reactions: { get: () => ({ favorite: 0, vote: 0 }), forViewer: () => new Map(), sentiment: () => new Map() },
    views: { record() {}, countsFor: () => null, countsForOrg: () => new Map(), viewersFor: () => [], topForOrg: () => [] },
    feedback: { listForArtifact: () => [] }, notifications: { recentForViewer: () => [], unreadCount: () => 0, markSeen() {} },
    thumbnails: { readThumbnail: async () => null, placeholder: () => Buffer.from('placeholder') },
    pages: { gallery: () => 'gallery', shell: () => 'shell', notFound: () => 'not found', notSignedIn: () => 'not signed in', settings: () => 'settings' },
    logger: { error() {}, info() {} }
  };
  const app = createApp(deps);
  async function invoke(method, routePath, params, query = {}) {
    const route = app._router.stack.find((layer) => layer.route?.path === routePath && layer.route.methods[method]);
    const result = { status: 200, headers: {}, body: null };
    const res = {
      status(code){ result.status=code; return this; },
      set(name,value){ if(typeof name === 'object') Object.assign(result.headers,name); else result.headers[String(name).toLowerCase()]=value; return this; },
      send(value){ result.body=Buffer.isBuffer(value)?value.toString('utf8'):String(value); return this; },
      json(value){ result.headers['content-type']='application/json'; result.body=JSON.stringify(value); return this; },
      redirect(code,location){ result.status=code; result.headers.location=location; return this; }, end(){ return this; }
    };
    await route.route.stack.at(-1).handle({ headers:{}, params, query }, res);
    return { status: result.status, location: result.headers.location || null, contentType: result.headers['content-type'] || result.headers['Content-Type'] || null, cache: result.headers['cache-control'] || result.headers['Cache-Control'] || null, csp: result.headers['content-security-policy'] || result.headers['Content-Security-Policy'] || null, body: result.body };
  }
  const output = [];
  output.push(await invoke('get','/raw/:id',{id:artifact.id},{}));
  output.push(await invoke('get','/raw/:id',{id:artifact.id},{download:''}));
  output.push(await invoke('get','/s/:token',{token:'token'},{}));
  output.push(await invoke('post','/notifications/seen',{},{}));
  process.stdout.write(JSON.stringify(output));
}).catch((error) => { console.error(error); process.exit(1); });
"#;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn node_reference_available(root: &FsPath) -> bool {
    let required = std::env::var(REQUIRE_NODE_REFERENCE).is_ok_and(|value| value == "1");
    let reason = if !root.join("lib/app.js").is_file() {
        Some("lib/app.js is missing")
    } else if !root.join("node_modules/express").is_dir() {
        Some("node_modules/express is missing")
    } else {
        match Command::new("node").arg("--version").output() {
            Ok(output) if output.status.success() => None,
            _ => Some("node is not on PATH"),
        }
    };
    match reason {
        None => true,
        Some(reason) => {
            assert!(
                !required,
                "{REQUIRE_NODE_REFERENCE}=1 but the Node reference is unavailable ({reason}); the U17 route parity proof did not run"
            );
            eprintln!("skipping U17 Node parity proof: {reason}");
            false
        }
    }
}

fn node_route_snapshots(root: &FsPath) -> Value {
    let module = format!("file://{}", root.join("lib/app.js").display());
    let output = Command::new("node")
        .current_dir(root)
        .arg("-e")
        .arg(NODE_DRIVER)
        .arg(module)
        .output()
        .expect("run Node U17 route oracle");
    assert!(
        output.status.success(),
        "Node U17 route oracle failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("Node route snapshots")
}

async fn rust_route_snapshot(fake: &Fake, method: Method, uri: &str) -> Value {
    let response = invoke(fake, method, uri).await;
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let body = String::from_utf8_lossy(&response_body(response).await).into_owned();
    json!({
        "status": status,
        "location": headers.get(header::LOCATION).and_then(|value| value.to_str().ok()),
        "contentType": headers.get(header::CONTENT_TYPE).and_then(|value| value.to_str().ok()),
        "cache": headers.get(header::CACHE_CONTROL).and_then(|value| value.to_str().ok()),
        "csp": headers.get(header::CONTENT_SECURITY_POLICY).and_then(|value| value.to_str().ok()),
        "body": body,
    })
}

#[tokio::test]
async fn rust_route_shapes_match_the_real_node_app_oracle() {
    let root = repo_root();
    if !node_reference_available(&root) {
        return;
    }
    let node = node_route_snapshots(&root);
    let fake = Fake::standard();
    let rust = Value::Array(vec![
        rust_route_snapshot(&fake, Method::GET, &format!("/raw/{ID}")).await,
        rust_route_snapshot(&fake, Method::GET, &format!("/raw/{ID}?download")).await,
        rust_route_snapshot(&fake, Method::GET, "/s/token").await,
        rust_route_snapshot(&fake, Method::POST, "/notifications/seen").await,
    ]);

    // Express adds transport-level details (ETag/content length) outside these route handlers;
    // the route-owned status, representation headers and bytes must be identical.
    assert_eq!(rust, node);
}
