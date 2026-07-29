//! Production composition, observability, health, and listener startup.

use std::{
    collections::{BTreeMap, HashMap},
    fs::OpenOptions,
    future::Future,
    path::PathBuf,
    process::ExitCode,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use artifact_mcp::{
    AppDeps,
    artifacts::{lifecycle::ArtifactStore, validation::SafeArtifactId},
    build_router,
    config::{AccessIdentityMode, AppConfig, Clock, IdSource, NanoIdSource, SeedKeys, SystemClock},
    error::AppError,
    http::middleware::{AccessRetryState, access_session_retry, prevent_response_transforms},
    integrations::{
        notify::{DiscordNotifier, HttpTransport},
        thumbnails::{PreviewArtifactIndex, PreviewArtifactRef, PreviewHtml, PreviewIntegration},
    },
    model::{
        ArtifactId, ClientId, CreateOrganization, CreatePublisherKey, CreateShare, CreateWebhook,
        CreatedPublisherKey, DeliveryResult, EmailAddress, Feedback, FeedbackId, FeedbackMutation,
        FeedbackRef, NotificationPayload, OrgId, Organization, PublicShare, PublisherIdentity,
        PublisherKeySummary, Reaction, ReactionUpdate, Sentiment, ShareGrant, ShareToken,
        SubmitFeedback, TopViewedArtifact, ViewCounts, Viewer, ViewerNotification, ViewerView,
        WebhookDelivery, WebhookEvent, WebhookId, WebhookSummary,
    },
    persistence::{
        db::{self, Database, DbPool},
        feedback,
        keys::{self, KeyStore},
        migrations::{self, MigrationContext},
        notifications,
        orgs::OrgStore,
        reactions, shares, views,
        webhooks::WebhookStore,
    },
    ports::{
        AdminService, ArtifactService, BoxFuture, EngagementService, HealthProbe, NotificationSink,
        PreviewService, PublisherAuthenticator, ShareService, ViewerIdentity,
        integrations::{HealthReport, PreviewPriority},
    },
    render::portal::AskamaPageRenderer,
    security::{
        auth::{KeyAuthenticator, KeyHash, PublisherKeyDirectory, PublisherKeyRecord},
        crypto::{WebhookUrlProtection, warn_if_webhook_encryption_disabled},
        identity::{AccessViewerIdentity, OrgDirectory, assert_ready},
        jwks::{CachingJwks, JwkDocument, JwksProvider, StaticJwks},
        oauth::{CompositePublisherAuthenticator, OAuthAuthenticator},
    },
};
use axum::{
    Router,
    http::{HeaderName, Request},
    middleware,
};
use rusqlite::OptionalExtension as _;
use thiserror::Error;
use tokio::net::TcpListener;
use tower_http::{
    request_id::{MakeRequestUuid, RequestId, SetRequestIdLayer},
    trace::TraceLayer,
};
use tracing::{Instrument as _, info_span};
use tracing_subscriber::EnvFilter;

const REQUEST_ID_HEADER: HeaderName = HeaderName::from_static("x-request-id");
const HEALTHCHECK_TIMEOUT: Duration = Duration::from_secs(5);
static HEALTH_PROBE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Error)]
pub(crate) enum RuntimeError {
    #[error(transparent)]
    Application(#[from] AppError),
    #[error("listener I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("healthcheck failed: {0}")]
    Healthcheck(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StartupStage {
    DatabaseReady,
    StorageReconciled,
    ListenerBindRequested,
}

pub(crate) trait StartupObserver: Send + Sync {
    fn stage(&self, stage: StartupStage);
}

#[derive(Clone, Copy, Debug, Default)]
struct NoopStartupObserver;

impl StartupObserver for NoopStartupObserver {
    fn stage(&self, _stage: StartupStage) {}
}

#[derive(Clone)]
struct ProductionDirectories {
    pool: DbPool,
}

impl ProductionDirectories {
    const fn new(pool: DbPool) -> Self {
        Self { pool }
    }
}

impl std::fmt::Debug for ProductionDirectories {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProductionDirectories")
            .finish_non_exhaustive()
    }
}

impl PublisherKeyDirectory for ProductionDirectories {
    fn find_active<'a>(
        &'a self,
        hash: &'a KeyHash,
    ) -> BoxFuture<'a, Result<Option<PublisherKeyRecord>, AppError>> {
        let pool = self.pool.clone();
        let hash = hash.expose().to_owned();
        Box::pin(async move {
            db::interact(&pool, move |conn| {
                conn.query_row(
                    "SELECT client_id, org, label, role FROM api_keys \
                     WHERE key_hash = ?1 AND revoked_at IS NULL",
                    (&hash,),
                    |row| {
                        Ok(PublisherKeyRecord {
                            client_id: ClientId(row.get(0)?),
                            org: OrgId(row.get(1)?),
                            label: row.get(2)?,
                            role: row.get(3)?,
                        })
                    },
                )
                .optional()
                .map_err(|error| {
                    tracing::error!(error = %error, "publisher key lookup failed");
                    AppError::Internal
                })
            })
            .await
        })
    }
}

impl OrgDirectory for ProductionDirectories {
    fn org_for_email<'a>(
        &'a self,
        email: &'a EmailAddress,
    ) -> BoxFuture<'a, Result<Option<OrgId>, AppError>> {
        let store = OrgStore::new(self.pool.clone());
        Box::pin(async move { store.org_for_email(email).await })
    }

    fn org_for_domain<'a>(
        &'a self,
        domain: &'a str,
    ) -> BoxFuture<'a, Result<Option<OrgId>, AppError>> {
        let store = OrgStore::new(self.pool.clone());
        Box::pin(async move { store.org_for_domain(domain).await })
    }
}

#[derive(Clone)]
struct ProductionAdmin {
    keys: KeyStore,
    orgs: OrgStore,
    webhooks: WebhookStore,
}

impl AdminService for ProductionAdmin {
    fn list_keys(&self) -> BoxFuture<'_, Result<Vec<PublisherKeySummary>, AppError>> {
        Box::pin(self.keys.list_keys())
    }

    fn create_key(
        &self,
        request: CreatePublisherKey,
    ) -> BoxFuture<'_, Result<CreatedPublisherKey, AppError>> {
        Box::pin(self.keys.create_key(request))
    }

    fn revoke_key<'a>(&'a self, client_id: &'a ClientId) -> BoxFuture<'a, Result<bool, AppError>> {
        Box::pin(self.keys.revoke_key(client_id))
    }

    fn set_key_owner(
        &self,
        client_id: ClientId,
        owner_email: Option<String>,
    ) -> BoxFuture<'_, Result<Option<artifact_mcp::model::KeyOwnerUpdate>, AppError>> {
        Box::pin(self.keys.set_key_owner(client_id, owner_email))
    }

    fn backfill_key_owner(
        &self,
        client_id: ClientId,
        owner_email: String,
        confirm: bool,
    ) -> BoxFuture<'_, Result<Option<artifact_mcp::model::OwnerBackfillResult>, AppError>> {
        Box::pin(
            self.keys
                .backfill_key_owner(client_id, owner_email, confirm),
        )
    }

    fn org_exists<'a>(&'a self, org: &'a OrgId) -> BoxFuture<'a, Result<bool, AppError>> {
        Box::pin(self.orgs.org_exists(org))
    }

    fn org_for_domain<'a>(
        &'a self,
        domain: &'a str,
    ) -> BoxFuture<'a, Result<Option<OrgId>, AppError>> {
        Box::pin(self.orgs.org_for_domain(domain))
    }

    fn org_for_email<'a>(
        &'a self,
        email: &'a EmailAddress,
    ) -> BoxFuture<'a, Result<Option<OrgId>, AppError>> {
        Box::pin(self.orgs.org_for_email(email))
    }

    fn org_names(&self) -> BoxFuture<'_, Result<Vec<OrgId>, AppError>> {
        Box::pin(self.orgs.org_names())
    }

    fn list_orgs(&self) -> BoxFuture<'_, Result<Vec<Organization>, AppError>> {
        Box::pin(self.orgs.list_orgs())
    }

    fn create_org(
        &self,
        request: CreateOrganization,
    ) -> BoxFuture<'_, Result<Organization, AppError>> {
        Box::pin(self.orgs.create_org(request))
    }

    fn delete_org<'a>(&'a self, org: &'a OrgId) -> BoxFuture<'a, Result<bool, AppError>> {
        Box::pin(self.orgs.delete_org(org))
    }

    fn add_domain<'a>(
        &'a self,
        org: &'a OrgId,
        domain: &'a str,
    ) -> BoxFuture<'a, Result<String, AppError>> {
        Box::pin(self.orgs.add_domain(org, domain))
    }

    fn remove_domain<'a>(
        &'a self,
        org: &'a OrgId,
        domain: &'a str,
    ) -> BoxFuture<'a, Result<bool, AppError>> {
        Box::pin(self.orgs.remove_domain(org, domain))
    }

    fn add_email_member<'a>(
        &'a self,
        org: &'a OrgId,
        email: &'a EmailAddress,
    ) -> BoxFuture<'a, Result<EmailAddress, AppError>> {
        Box::pin(self.orgs.add_email_member(org, email))
    }

    fn remove_email_member<'a>(
        &'a self,
        org: &'a OrgId,
        email: &'a EmailAddress,
    ) -> BoxFuture<'a, Result<bool, AppError>> {
        Box::pin(self.orgs.remove_email_member(org, email))
    }

    fn categories<'a>(&'a self, org: &'a OrgId) -> BoxFuture<'a, Result<Vec<String>, AppError>> {
        Box::pin(self.orgs.categories(org))
    }

    fn add_category<'a>(
        &'a self,
        org: &'a OrgId,
        name: &'a str,
    ) -> BoxFuture<'a, Result<String, AppError>> {
        Box::pin(self.orgs.add_category(org, name))
    }

    fn remove_category<'a>(
        &'a self,
        org: &'a OrgId,
        name: &'a str,
    ) -> BoxFuture<'a, Result<bool, AppError>> {
        Box::pin(self.orgs.remove_category(org, name))
    }

    fn color_map(&self) -> BoxFuture<'_, Result<BTreeMap<OrgId, Option<String>>, AppError>> {
        Box::pin(self.orgs.color_map())
    }

    fn set_color<'a>(
        &'a self,
        org: &'a OrgId,
        color: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Option<String>, AppError>> {
        Box::pin(self.orgs.set_color(org, color))
    }

    fn list_webhooks<'a>(
        &'a self,
        org: &'a OrgId,
    ) -> BoxFuture<'a, Result<Vec<WebhookSummary>, AppError>> {
        Box::pin(self.webhooks.list_for_org(org))
    }

    fn create_webhook(
        &self,
        request: CreateWebhook,
    ) -> BoxFuture<'_, Result<WebhookSummary, AppError>> {
        Box::pin(self.webhooks.create(request))
    }

    fn remove_webhook<'a>(
        &'a self,
        org: &'a OrgId,
        id: &'a WebhookId,
    ) -> BoxFuture<'a, Result<bool, AppError>> {
        Box::pin(self.webhooks.remove(org, id))
    }

    fn set_webhook_events<'a>(
        &'a self,
        org: &'a OrgId,
        id: &'a WebhookId,
        events: &'a [WebhookEvent],
    ) -> BoxFuture<'a, Result<Option<WebhookSummary>, AppError>> {
        Box::pin(self.webhooks.set_events(org, id, events))
    }

    fn webhook_delivery<'a>(
        &'a self,
        id: &'a WebhookId,
    ) -> BoxFuture<'a, Result<Option<WebhookDelivery>, AppError>> {
        Box::pin(self.webhooks.delivery(id))
    }
}

#[derive(Clone)]
struct ProductionEngagement {
    pool: DbPool,
    ids: Arc<dyn IdSource>,
    feedback_max_body: u64,
}

impl EngagementService for ProductionEngagement {
    fn reaction<'a>(
        &'a self,
        artifact: &'a artifact_mcp::security::access::AuthorizedArtifact,
        viewer: &'a Viewer,
    ) -> BoxFuture<'a, Result<Reaction, AppError>> {
        let email = viewer.email.clone().unwrap_or_default();
        let artifact_id = artifact.meta().id.clone();
        Box::pin(reactions::get_pooled(&self.pool, email, artifact_id))
    }

    fn set_reaction(
        &self,
        artifact: artifact_mcp::security::access::AuthorizedArtifact,
        viewer: Viewer,
        update: ReactionUpdate,
    ) -> BoxFuture<'_, Result<Reaction, AppError>> {
        let email = viewer.email.unwrap_or_default();
        let artifact_id = artifact.meta().id.clone();
        Box::pin(reactions::set_pooled(
            &self.pool,
            email,
            artifact_id,
            update,
        ))
    }

    fn reactions_for_viewer<'a>(
        &'a self,
        viewer: &'a Viewer,
    ) -> BoxFuture<'a, Result<BTreeMap<ArtifactId, Reaction>, AppError>> {
        let email = viewer.email.clone().unwrap_or_default();
        Box::pin(reactions::for_viewer_pooled(&self.pool, email))
    }

    fn sentiment(&self) -> BoxFuture<'_, Result<BTreeMap<ArtifactId, Sentiment>, AppError>> {
        Box::pin(reactions::sentiment_pooled(&self.pool))
    }

    fn record_view<'a>(
        &'a self,
        artifact: &'a artifact_mcp::security::access::AuthorizedArtifact,
        viewer: &'a Viewer,
    ) -> BoxFuture<'a, Result<(), AppError>> {
        let pool = self.pool.clone();
        let artifact_id = artifact.meta().id.clone();
        let org = artifact.meta().org.clone();
        let viewer = viewer.clone();
        Box::pin(async move {
            if !views::should_record(&viewer) {
                return Ok(());
            }
            let Some(email) = viewer.email else {
                return Ok(());
            };
            views::record_pooled(&pool, artifact_id, org, email).await
        })
    }

    fn view_counts<'a>(
        &'a self,
        artifact: &'a artifact_mcp::security::access::AuthorizedArtifact,
    ) -> BoxFuture<'a, Result<ViewCounts, AppError>> {
        Box::pin(views::counts_for_pooled(
            &self.pool,
            artifact.meta().id.clone(),
        ))
    }

    fn view_counts_for_org<'a>(
        &'a self,
        org: &'a OrgId,
    ) -> BoxFuture<'a, Result<BTreeMap<ArtifactId, ViewCounts>, AppError>> {
        Box::pin(views::counts_for_org_pooled(&self.pool, org.clone()))
    }

    fn viewers<'a>(
        &'a self,
        artifact: &'a artifact_mcp::security::access::AuthorizedArtifact,
    ) -> BoxFuture<'a, Result<Vec<ViewerView>, AppError>> {
        Box::pin(views::viewers_for_pooled(
            &self.pool,
            artifact.meta().id.clone(),
        ))
    }

    fn top_for_org<'a>(
        &'a self,
        org: &'a OrgId,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<TopViewedArtifact>, AppError>> {
        Box::pin(views::top_for_org_pooled(&self.pool, org.clone(), limit))
    }

    fn feedback_ref<'a>(
        &'a self,
        id: &'a FeedbackId,
    ) -> BoxFuture<'a, Result<Option<FeedbackRef>, AppError>> {
        let pool = self.pool.clone();
        let id = id.clone();
        Box::pin(
            async move { db::interact(&pool, move |conn| feedback::feedback_ref(conn, &id)).await },
        )
    }

    fn list_feedback<'a>(
        &'a self,
        artifact: &'a artifact_mcp::security::access::AuthorizedArtifact,
    ) -> BoxFuture<'a, Result<Vec<Feedback>, AppError>> {
        let pool = self.pool.clone();
        let id = artifact.meta().id.clone();
        Box::pin(async move {
            db::interact(&pool, move |conn| feedback::list_for_artifact(conn, &id)).await
        })
    }

    fn submit_feedback(
        &self,
        artifact: artifact_mcp::security::access::AuthorizedArtifact,
        submission: SubmitFeedback,
    ) -> BoxFuture<'_, Result<Feedback, AppError>> {
        let pool = self.pool.clone();
        let ids = Arc::clone(&self.ids);
        let max_body = self.feedback_max_body;
        let meta = artifact.into_meta();
        Box::pin(async move {
            db::interact(&pool, move |conn| {
                feedback::add(
                    conn,
                    ids.as_ref(),
                    &feedback::NewFeedback {
                        artifact_id: &meta.id,
                        org: &meta.org,
                        artifact_revision: meta.revision,
                        anchor_page: submission.anchor_page.as_deref(),
                        submission: &submission,
                        max_body,
                    },
                )
            })
            .await
        })
    }

    fn delete_feedback(
        &self,
        artifact: artifact_mcp::security::access::AuthorizedArtifact,
        viewer: Viewer,
        id: FeedbackId,
    ) -> BoxFuture<'_, Result<FeedbackMutation, AppError>> {
        let pool = self.pool.clone();
        let meta = artifact.into_meta();
        Box::pin(async move {
            let scope = FeedbackRef {
                id,
                artifact_id: meta.id,
                org: meta.org,
            };
            let email = viewer.email.unwrap_or_default();
            db::interact(&pool, move |conn| {
                feedback::delete_as_viewer(conn, &scope, &email, viewer.is_admin)
            })
            .await
        })
    }

    fn resolve_feedback_as_viewer(
        &self,
        artifact: artifact_mcp::security::access::AuthorizedArtifact,
        viewer: Viewer,
        id: FeedbackId,
    ) -> BoxFuture<'_, Result<FeedbackMutation, AppError>> {
        let pool = self.pool.clone();
        let meta = artifact.into_meta();
        Box::pin(async move {
            let scope = FeedbackRef {
                id,
                artifact_id: meta.id,
                org: meta.org,
            };
            let email = viewer.email.unwrap_or_default();
            db::interact(&pool, move |conn| {
                feedback::resolve_as_viewer(conn, &scope, &email, viewer.is_admin)
            })
            .await
        })
    }

    fn list_feedback_for_publisher<'a>(
        &'a self,
        publisher: &'a PublisherIdentity,
        artifact: Option<&'a artifact_mcp::security::access::OwnedArtifact>,
    ) -> BoxFuture<'a, Result<Vec<Feedback>, AppError>> {
        let pool = self.pool.clone();
        let publisher = publisher.clone();
        let artifact_id = artifact.map(|owned| owned.meta().id.clone());
        Box::pin(async move {
            db::interact(&pool, move |conn| {
                if publisher.is_admin() {
                    feedback::list_all(conn, artifact_id.as_ref())
                } else if matches!(publisher.role.as_str(), "reader" | "collaborator") {
                    feedback::list_for_org(conn, &publisher.org)
                } else {
                    feedback::list_for_client(
                        conn,
                        &publisher.client_id,
                        artifact_id.as_ref(),
                        Some(&publisher.org),
                    )
                }
            })
            .await
        })
    }

    fn resolve_feedback_as_publisher(
        &self,
        _artifact: artifact_mcp::security::access::OwnedArtifact,
        id: FeedbackId,
        resolved_by: String,
    ) -> BoxFuture<'_, Result<bool, AppError>> {
        let pool = self.pool.clone();
        Box::pin(async move {
            db::interact(&pool, move |conn| {
                feedback::resolve_as_publisher(conn, &id, &resolved_by)
            })
            .await
        })
    }

    fn reopen_feedback_as_publisher(
        &self,
        _artifact: artifact_mcp::security::access::OwnedArtifact,
        id: FeedbackId,
    ) -> BoxFuture<'_, Result<bool, AppError>> {
        let pool = self.pool.clone();
        Box::pin(async move { db::interact(&pool, move |conn| feedback::reopen(conn, &id)).await })
    }

    fn recent_notifications<'a>(
        &'a self,
        viewer: &'a Viewer,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<ViewerNotification>, AppError>> {
        Box::pin(notifications::recent_for_viewer_pooled(
            &self.pool,
            viewer.clone(),
            limit,
        ))
    }

    fn unread_notifications<'a>(
        &'a self,
        viewer: &'a Viewer,
    ) -> BoxFuture<'a, Result<u64, AppError>> {
        Box::pin(notifications::unread_count_pooled(
            &self.pool,
            viewer.clone(),
        ))
    }

    fn mark_notifications_seen<'a>(
        &'a self,
        email: &'a EmailAddress,
    ) -> BoxFuture<'a, Result<(), AppError>> {
        Box::pin(notifications::mark_seen_pooled(&self.pool, email.clone()))
    }
}

#[derive(Clone)]
struct ProductionShares {
    pool: DbPool,
    ids: Arc<dyn IdSource>,
    clock: Arc<dyn Clock>,
}

impl ShareService for ProductionShares {
    fn resolve<'a>(
        &'a self,
        token: &'a ShareToken,
    ) -> BoxFuture<'a, Result<Option<ShareGrant>, AppError>> {
        let pool = self.pool.clone();
        let token = token.clone();
        Box::pin(
            async move { db::interact(&pool, move |conn| shares::resolve(conn, &token)).await },
        )
    }

    fn create(
        &self,
        artifact: artifact_mcp::security::access::AuthorizedArtifact,
        request: CreateShare,
    ) -> BoxFuture<'_, Result<PublicShare, AppError>> {
        let pool = self.pool.clone();
        let ids = Arc::clone(&self.ids);
        let clock = Arc::clone(&self.clock);
        let meta = artifact.into_meta();
        Box::pin(async move {
            db::interact(&pool, move |conn| {
                shares::create(
                    conn,
                    ids.as_ref(),
                    clock.as_ref(),
                    &meta.id,
                    &meta.org,
                    &request,
                )
            })
            .await
        })
    }

    fn list<'a>(
        &'a self,
        artifact: &'a artifact_mcp::security::access::AuthorizedArtifact,
    ) -> BoxFuture<'a, Result<Vec<PublicShare>, AppError>> {
        let pool = self.pool.clone();
        let id = artifact.meta().id.clone();
        Box::pin(async move {
            db::interact(&pool, move |conn| shares::list_for_artifact(conn, &id)).await
        })
    }

    fn revoke(
        &self,
        artifact: artifact_mcp::security::access::AuthorizedArtifact,
        token: ShareToken,
    ) -> BoxFuture<'_, Result<bool, AppError>> {
        let pool = self.pool.clone();
        let id = artifact.into_meta().id;
        Box::pin(
            async move { db::interact(&pool, move |conn| shares::revoke(conn, &id, &token)).await },
        )
    }
}

#[derive(Clone)]
struct ArtifactPreviewNotifier {
    artifacts: Arc<ArtifactStore>,
    previews: Arc<PreviewIntegration>,
    discord: Arc<DiscordNotifier>,
}

impl ArtifactPreviewNotifier {
    async fn emit_detached(self, event: WebhookEvent, org: OrgId, payload: NotificationPayload) {
        if event == WebhookEvent::Deleted {
            self.previews
                .store()
                .remove_artifact(&payload.artifact_id)
                .await;
            self.discord
                .emit_with_preview(event, org, payload, None)
                .await;
            return;
        }

        if !matches!(
            event,
            WebhookEvent::Published | WebhookEvent::Updated | WebhookEvent::Restored
        ) {
            self.discord
                .emit_with_preview(event, org, payload, None)
                .await;
            return;
        }

        let preview = match self.artifacts.find_meta(&payload.artifact_id).await {
            Ok(Some(meta)) if !meta.is_bundle => match self.artifacts.read_body_for(&meta).await {
                Ok(Some(file)) => self
                    .previews
                    .queue()
                    .enqueue(
                        meta,
                        PreviewHtml::Ready(String::from_utf8_lossy(&file.content).into_owned()),
                        PreviewPriority::High,
                    )
                    .await
                    .map(Arc::new),
                _ => None,
            },
            _ => None,
        };
        self.discord
            .emit_with_preview(event, org, payload, preview)
            .await;
    }
}

impl NotificationSink for ArtifactPreviewNotifier {
    fn emit(
        &self,
        event: WebhookEvent,
        org: OrgId,
        payload: NotificationPayload,
    ) -> BoxFuture<'_, Result<(), AppError>> {
        let notifier = self.clone();
        Box::pin(async move {
            tokio::spawn(
                async move { notifier.emit_detached(event, org, payload).await }
                    .instrument(info_span!("notification.prepare")),
            );
            Ok(())
        })
    }

    fn test<'a>(
        &'a self,
        webhook: &'a WebhookDelivery,
    ) -> BoxFuture<'a, Result<DeliveryResult, AppError>> {
        self.discord.test(webhook)
    }
}

#[derive(Clone)]
pub(crate) struct ProductionHealth {
    pool: DbPool,
    artifact_dir: PathBuf,
}

impl ProductionHealth {
    pub(crate) const fn new(pool: DbPool, artifact_dir: PathBuf) -> Self {
        Self { pool, artifact_dir }
    }
}

impl HealthProbe for ProductionHealth {
    fn check(&self) -> BoxFuture<'_, Result<HealthReport, AppError>> {
        let pool = self.pool.clone();
        let artifact_dir = self.artifact_dir.clone();
        Box::pin(async move {
            db::interact(&pool, |conn| {
                conn.query_row("SELECT 1", [], |_| Ok(())).map_err(|error| {
                    tracing::warn!(error = %error, "health SQLite check failed");
                    AppError::Unavailable("database unavailable".to_owned())
                })
            })
            .await?;
            check_artifact_directory(artifact_dir).await?;
            Ok(HealthReport::ok())
        })
    }
}

#[derive(Debug)]
struct EffectivePragmas {
    journal_mode: String,
    synchronous: i64,
    busy_timeout: i64,
    wal_autocheckpoint: i64,
    foreign_keys: i64,
    page_size: i64,
}

#[derive(Default)]
struct StartupPreviewIndex {
    artifacts: HashMap<String, PreviewArtifactRef>,
}

impl PreviewArtifactIndex for StartupPreviewIndex {
    fn artifact(&self, id: &SafeArtifactId) -> Option<PreviewArtifactRef> {
        self.artifacts.get(id.as_str()).cloned()
    }
}

struct Bootstrapped {
    router: Router,
    host: String,
    port: u16,
}

#[tracing::instrument(skip_all)]
async fn check_artifact_directory(artifact_dir: PathBuf) -> Result<(), AppError> {
    tokio::task::spawn_blocking(move || {
        let mut entries = std::fs::read_dir(&artifact_dir).map_err(|error| {
            tracing::warn!(error = %error, "health artifact-directory read check failed");
            AppError::Unavailable("artifact directory unavailable".to_owned())
        })?;
        let _ = entries.next().transpose().map_err(|error| {
            tracing::warn!(error = %error, "health artifact-directory read check failed");
            AppError::Unavailable("artifact directory unavailable".to_owned())
        })?;

        let sequence = HEALTH_PROBE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let probe = artifact_dir.join(format!(".healthcheck-{}-{sequence}", std::process::id()));
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe)
            .map_err(|error| {
                tracing::warn!(error = %error, "health artifact-directory write check failed");
                AppError::Unavailable("artifact directory unavailable".to_owned())
            })?;
        std::fs::remove_file(&probe).map_err(|error| {
            tracing::warn!(error = %error, "health artifact-directory cleanup failed");
            AppError::Unavailable("artifact directory unavailable".to_owned())
        })?;
        Ok(())
    })
    .await
    .map_err(|error| {
        tracing::error!(error = %error, "health filesystem task failed");
        AppError::Internal
    })?
}

fn migration_context(config: &AppConfig) -> MigrationContext {
    MigrationContext {
        org_email_domains: config
            .access
            .domain_orgs
            .iter()
            .map(|(domain, org)| format!("{domain}:{org}"))
            .collect::<Vec<_>>()
            .join(","),
    }
}

#[tracing::instrument(skip_all)]
async fn seed_configured_keys(pool: &DbPool, seed_keys: SeedKeys) -> Result<u64, AppError> {
    for client_id in &seed_keys.ignored_placeholders {
        tracing::warn!(
            client_id = %client_id,
            "ignoring placeholder publisher key secret"
        );
    }
    let pool = pool.clone();
    db::interact(&pool, move |conn| {
        let mut statement = conn
            .prepare(
                "INSERT INTO api_keys (client_id, org, key_hash) VALUES (?1, ?2, ?3) \
                 ON CONFLICT(client_id) DO NOTHING",
            )
            .map_err(|error| {
                tracing::error!(error = %error, "publisher key seed prepare failed");
                AppError::Internal
            })?;
        let mut seeded = 0_u64;
        for entry in seed_keys.entries {
            let hash = keys::key_hash(&entry.secret);
            let changed = statement
                .execute((&entry.client_id.0, &entry.org.0, hash))
                .map_err(|error| {
                    tracing::error!(error = %error, "publisher key seed failed");
                    AppError::Internal
                })?;
            seeded = seeded.saturating_add(u64::try_from(changed).unwrap_or(u64::MAX));
        }
        Ok(seeded)
    })
    .await
}

async fn effective_pragmas(pool: &DbPool) -> Result<(i64, EffectivePragmas), AppError> {
    db::interact(pool, |conn| {
        db::verify_pragmas(conn)?;
        let schema_version = migrations::current_version(conn).map_err(|error| {
            tracing::error!(error = %error, "schema version read failed");
            AppError::Unavailable("database schema version unavailable".to_owned())
        })?;
        Ok((
            schema_version,
            EffectivePragmas {
                journal_mode: conn
                    .query_row("PRAGMA journal_mode", [], |row| row.get(0))
                    .map_err(pragma_read_error)?,
                synchronous: pragma_integer(conn, "PRAGMA synchronous")?,
                busy_timeout: pragma_integer(conn, "PRAGMA busy_timeout")?,
                wal_autocheckpoint: pragma_integer(conn, "PRAGMA wal_autocheckpoint")?,
                foreign_keys: pragma_integer(conn, "PRAGMA foreign_keys")?,
                page_size: pragma_integer(conn, "PRAGMA page_size")?,
            },
        ))
    })
    .await
}

fn pragma_integer(conn: &rusqlite::Connection, sql: &str) -> Result<i64, AppError> {
    conn.query_row(sql, [], |row| row.get(0))
        .map_err(pragma_read_error)
}

fn pragma_read_error(error: rusqlite::Error) -> AppError {
    tracing::error!(error = %error, "effective pragma read failed");
    AppError::Unavailable("database pragmas unavailable".to_owned())
}

fn configured_jwks(config: &AppConfig) -> Result<Arc<dyn JwksProvider>, AppError> {
    match config.access.identity_mode() {
        AccessIdentityMode::Jwt => {
            let url = config.access.jwks_url().ok_or_else(|| {
                AppError::Validation("Cloudflare Access JWKS URL is unavailable".to_owned())
            })?;
            Ok(Arc::new(CachingJwks::remote(url)?))
        }
        AccessIdentityMode::HeaderTrust | AccessIdentityMode::Disabled => {
            Ok(Arc::new(StaticJwks::new(JwkDocument::default())))
        }
    }
}

fn configured_oauth_jwks(config: &AppConfig) -> Result<Option<Arc<dyn JwksProvider>>, AppError> {
    if config.oauth.enabled() {
        Ok(Some(Arc::new(CachingJwks::remote(
            config.oauth.jwks_url.clone(),
        )?)))
    } else {
        Ok(None)
    }
}

fn startup_preview_index(artifacts: &[artifact_mcp::model::ArtifactMeta]) -> StartupPreviewIndex {
    StartupPreviewIndex {
        artifacts: artifacts
            .iter()
            .map(|meta| {
                (
                    meta.id.0.clone(),
                    PreviewArtifactRef {
                        is_bundle: meta.is_bundle,
                        body_sha256: meta.body_sha256.clone(),
                    },
                )
            })
            .collect(),
    }
}

fn schedule_thumbnail_backfill(
    artifacts: Vec<artifact_mcp::model::ArtifactMeta>,
    store: Arc<ArtifactStore>,
    previews: Arc<PreviewIntegration>,
) -> usize {
    if !previews.enabled() {
        return 0;
    }
    let candidates = artifacts
        .into_iter()
        .filter(|meta| !meta.is_bundle && !meta.body_sha256.is_empty())
        .collect::<Vec<_>>();
    let count = candidates.len();
    tokio::spawn(
        async move {
            for meta in candidates {
                if previews
                    .store()
                    .read_thumbnail(&meta, &meta.body_sha256)
                    .await
                    .is_some()
                {
                    continue;
                }
                let id = meta.id.clone();
                let expected_digest = meta.body_sha256.clone();
                let artifact_store = Arc::clone(&store);
                let deferred = PreviewHtml::Deferred(Box::new(move || {
                    Box::pin(async move {
                        let current = artifact_store.find_meta(&id).await.ok().flatten()?;
                        if current.body_sha256 != expected_digest || current.is_bundle {
                            return None;
                        }
                        let file = artifact_store
                            .read_body_for(&current)
                            .await
                            .ok()
                            .flatten()?;
                        String::from_utf8(file.content).ok()
                    })
                }));
                let _ = previews
                    .queue()
                    .enqueue(meta, deferred, PreviewPriority::Low)
                    .await;
            }
        }
        .instrument(info_span!("preview.backfill")),
    );
    count
}

fn runtime_router(deps: AppDeps) -> Router {
    let access_retry =
        AccessRetryState::new(deps.config.access.identity_mode(), Arc::clone(&deps.pages));
    let trace = TraceLayer::new_for_http().make_span_with(|request: &Request<_>| {
        let request_id = request
            .extensions()
            .get::<RequestId>()
            .and_then(|id| id.header_value().to_str().ok())
            .unwrap_or("unknown");
        tracing::info_span!(
            "http.request",
            request_id,
            method = %request.method()
        )
    });
    build_router(deps)
        .layer(middleware::from_fn_with_state(
            access_retry,
            access_session_retry,
        ))
        .layer(trace)
        .layer(SetRequestIdLayer::new(REQUEST_ID_HEADER, MakeRequestUuid))
        .layer(middleware::from_fn(prevent_response_transforms))
}

#[tracing::instrument(skip_all)]
async fn bootstrap(
    config: AppConfig,
    observer: Arc<dyn StartupObserver>,
) -> Result<Bootstrapped, RuntimeError> {
    assert_ready(&config)?;
    let config = Arc::new(config);
    let protection = Arc::new(WebhookUrlProtection::from_config_key(
        config.webhook_enc_key.as_ref(),
    )?);
    if config.webhook_enc_key.is_none() {
        let _ = warn_if_webhook_encryption_disabled(None);
    }

    let data_dir = config.data_dir.clone();
    let migration_context = migration_context(&config);
    let protection_for_open = Arc::clone(&protection);
    let pool = tokio::task::spawn_blocking(move || {
        let cipher = protection_for_open
            .cipher()
            .map(|cipher| cipher as &dyn migrations::WebhookUrlCipher);
        Database::open_with(&data_dir, &migration_context, cipher)
    })
    .await
    .map_err(|error| {
        tracing::error!(error = %error, "database bootstrap task failed");
        AppError::Internal
    })??;
    observer.stage(StartupStage::DatabaseReady);

    let (schema_version, pragmas) = effective_pragmas(&pool).await?;
    tracing::info!(schema_version, "schema version ready");
    tracing::info!(
        journal_mode = %pragmas.journal_mode,
        synchronous = pragmas.synchronous,
        busy_timeout_ms = pragmas.busy_timeout,
        wal_autocheckpoint_pages = pragmas.wal_autocheckpoint,
        foreign_keys = pragmas.foreign_keys,
        page_size = pragmas.page_size,
        "effective database pragmas"
    );

    let seeded = seed_configured_keys(&pool, config.seed_keys.clone()).await?;
    tracing::info!(seeded_keys = seeded, "publisher key seed complete");
    let key_store = KeyStore::new(pool.clone());
    if seeded == 0
        && key_store
            .list_keys()
            .await?
            .iter()
            .all(|key| key.revoked_at.is_some())
    {
        tracing::warn!("no active publisher keys configured; publishing is disabled");
    }

    let ids: Arc<dyn IdSource> = Arc::new(NanoIdSource::default());
    let artifacts = Arc::new(ArtifactStore::from_config(
        pool.clone(),
        &config,
        Arc::clone(&ids),
    ));

    let storage = artifacts.audit_storage(true).await?;
    observer.stage(StartupStage::StorageReconciled);
    tracing::info!(
        recovered = storage.recovered_paths.len(),
        transient = storage.transient_paths.len(),
        missing = storage.missing_bodies.len(),
        divergent = storage.divergent_bodies.len(),
        orphan_bodies = storage.orphan_bodies.len(),
        orphan_history = storage.orphan_history.len(),
        "storage reconciliation complete"
    );

    let digests = artifacts.backfill_body_digests().await?;
    tracing::info!(
        scanned = digests.scanned,
        updated = digests.updated,
        "artifact digest backfill complete"
    );

    let previews = Arc::new(PreviewIntegration::from_config(&config));
    let grouped = artifacts.list_all_grouped_by_org(true).await?;
    let all_artifacts = grouped
        .into_iter()
        .flat_map(|group| group.items)
        .collect::<Vec<_>>();
    let preview_index = startup_preview_index(&all_artifacts);
    let preview_audit = previews.store().audit(&preview_index).await;
    let queued =
        schedule_thumbnail_backfill(all_artifacts, Arc::clone(&artifacts), Arc::clone(&previews));
    tracing::info!(
        enabled = previews.enabled(),
        orphan_paths_removed = preview_audit.orphan_dirs.len(),
        stale_files_removed = preview_audit.partial_files.len(),
        invalid_pngs_removed = preview_audit.invalid_files.len(),
        backfill_candidates = queued,
        "preview startup status"
    );

    let directories = Arc::new(ProductionDirectories::new(pool.clone()));
    let api_keys = config
        .oauth
        .api_keys_enabled
        .then(|| KeyAuthenticator::new(directories.clone()));
    let oauth = configured_oauth_jwks(&config)?
        .map(|jwks| OAuthAuthenticator::new(config.oauth.clone(), jwks));
    let publisher_auth: Arc<dyn PublisherAuthenticator> =
        Arc::new(CompositePublisherAuthenticator::new(api_keys, oauth));
    tracing::info!(
        oauth_enabled = config.oauth.enabled(),
        api_keys_enabled = config.oauth.api_keys_enabled,
        "MCP publisher authentication ready"
    );
    let viewer_identity: Arc<dyn ViewerIdentity> = Arc::new(AccessViewerIdentity::new(
        Arc::clone(&config),
        configured_jwks(&config)?,
        directories,
    ));
    tracing::info!(
        identity_mode = %config.access.identity_mode(),
        "viewer identity mode ready"
    );

    let webhooks = Arc::new(WebhookStore::new(
        pool.clone(),
        Arc::clone(&ids),
        protection,
    ));
    let discord = Arc::new(DiscordNotifier::new(
        Arc::clone(&webhooks),
        Arc::new(HttpTransport::new()?),
    ));
    let notifications: Arc<dyn NotificationSink> = Arc::new(ArtifactPreviewNotifier {
        artifacts: Arc::clone(&artifacts),
        previews: Arc::clone(&previews),
        discord,
    });
    let admin: Arc<dyn AdminService> = Arc::new(ProductionAdmin {
        keys: key_store,
        orgs: OrgStore::new(pool.clone()),
        webhooks: webhooks.as_ref().clone(),
    });
    let engagement: Arc<dyn EngagementService> = Arc::new(ProductionEngagement {
        pool: pool.clone(),
        ids: Arc::clone(&ids),
        feedback_max_body: config.storage.feedback_max_body,
    });
    let shares: Arc<dyn ShareService> = Arc::new(ProductionShares {
        pool: pool.clone(),
        ids,
        clock: Arc::new(SystemClock),
    });
    let health: Arc<dyn HealthProbe> = Arc::new(ProductionHealth::new(pool, config.artifact_dir()));

    let host = config.listen_host.clone();
    let port = config.port;
    let deps = AppDeps {
        publisher_auth,
        viewer_identity,
        artifacts,
        admin,
        engagement,
        shares,
        pages: Arc::new(AskamaPageRenderer::from_config(&config)),
        previews,
        notifications,
        health,
        preview_tasks: artifact_mcp::mcp::tasks::PreviewTaskStore::new(&config.data_dir),
        mcp_telemetry: artifact_mcp::observability::McpTelemetry::default(),
        config,
    };
    artifact_mcp::mcp::tasks::resume_preview_tasks(deps.clone());
    let router = runtime_router(deps);
    Ok(Bootstrapped { router, host, port })
}

#[tracing::instrument(skip_all)]
pub(crate) async fn run_with_bind<B, F>(
    config: AppConfig,
    observer: Arc<dyn StartupObserver>,
    bind_and_serve: B,
) -> Result<(), RuntimeError>
where
    B: FnOnce(String, u16, Router) -> F,
    F: Future<Output = Result<(), RuntimeError>>,
{
    let bootstrapped = bootstrap(config, Arc::clone(&observer)).await?;
    observer.stage(StartupStage::ListenerBindRequested);
    bind_and_serve(bootstrapped.host, bootstrapped.port, bootstrapped.router).await
}

#[tracing::instrument(skip_all)]
async fn serve(config: AppConfig) -> Result<(), RuntimeError> {
    run_with_bind(
        config,
        Arc::new(NoopStartupObserver),
        |host, port, router| async move {
            let listener = TcpListener::bind((host.as_str(), port)).await?;
            tracing::info!(listen_host = %host, port, "listener ready");
            axum::serve(listener, router)
                .with_graceful_shutdown(shutdown_signal())
                .await?;
            tracing::info!("listener stopped");
            Ok(())
        },
    )
    .await
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %error, "Ctrl-C signal handler failed");
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                let _ = signal.recv().await;
            }
            Err(error) => tracing::error!(error = %error, "SIGTERM handler failed"),
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    tracing::info!("shutdown signal received");
}

fn healthcheck_port() -> Result<u16, RuntimeError> {
    match std::env::var("PORT") {
        Ok(raw) => raw
            .trim()
            .parse::<u16>()
            .ok()
            .filter(|port| *port > 0)
            .ok_or_else(|| {
                RuntimeError::Healthcheck("PORT must be between 1 and 65535".to_owned())
            }),
        Err(_) => Ok(artifact_mcp::config::DEFAULT_PORT),
    }
}

#[tracing::instrument(skip_all)]
async fn healthcheck() -> Result<(), RuntimeError> {
    let port = healthcheck_port()?;
    let url = format!("http://127.0.0.1:{port}/health");
    let client = reqwest::Client::builder()
        .timeout(HEALTHCHECK_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| RuntimeError::Healthcheck("HTTP client unavailable".to_owned()))?;
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|_| RuntimeError::Healthcheck("server is unreachable".to_owned()))?;
    if response.status() != reqwest::StatusCode::OK {
        return Err(RuntimeError::Healthcheck(format!(
            "/health returned {}",
            response.status().as_u16()
        )));
    }
    let report = response
        .json::<HealthReport>()
        .await
        .map_err(|_| RuntimeError::Healthcheck("invalid /health response".to_owned()))?;
    if report != HealthReport::ok() {
        return Err(RuntimeError::Healthcheck(
            "/health did not report ok".to_owned(),
        ));
    }
    Ok(())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("artifact_mcp=info,tower_http=info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .json()
        .try_init();
}

fn is_healthcheck(args: &[String]) -> Result<bool, RuntimeError> {
    match args {
        [] => Ok(false),
        [command] if command == "healthcheck" => Ok(true),
        _ => Err(RuntimeError::Healthcheck(
            "usage: artifact-mcp [healthcheck]".to_owned(),
        )),
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    init_tracing();
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let result = match is_healthcheck(&args) {
        Ok(true) => healthcheck().await,
        Ok(false) => match AppConfig::from_env() {
            Ok(config) => serve(config).await,
            Err(error) => Err(RuntimeError::from(error)),
        },
        Err(error) => Err(error),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(error = %error, "artifact-mcp stopped with an error");
            ExitCode::FAILURE
        }
    }
}
