//! Production composition, observability, health, and listener startup.

use std::{
    collections::{BTreeMap, HashMap},
    fs::OpenOptions,
    future::Future,
    net::SocketAddr,
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
    http::middleware::{
        AccessRetryState, access_session_retry, attach_audit_request_id,
        prevent_response_transforms,
    },
    integrations::{
        delivery_runtime::{DeliveryRuntime, DeliveryTelemetry},
        delivery_worker::{ArtifactDeliveryPreviewResolver, OrganizationDiscordDiscussionProvider},
        discord_delivery::DiscordProviderTransport,
        discord_discussion::{DiscordDiscussionTransport, DiscussionResult},
        discord_gateway_runtime::{DiscordInboundReadinessRest, DiscordInboundRuntime},
        discord_history_recovery::DiscordHistoryRest,
        discord_recovery_runtime::DiscordRecoveryRuntime,
        notify::{DiscordNotifier, HttpTransport},
        preview_notifier::ArtifactPreviewNotifier,
        thumbnails::{
            PersistentThumbnailScheduler, PreviewArtifactIndex, PreviewArtifactRef, PreviewHtml,
            PreviewIntegration,
        },
    },
    model::{
        ArtifactId, ClientId, CreateOrganization, CreatePublisherKey, CreateShare, CreateWebhook,
        CreatedPublisherKey, EmailAddress, Feedback, FeedbackId, FeedbackMutation, FeedbackRef,
        OrgId, Organization, PublicShare, PublisherIdentity, PublisherKeySummary, Reaction,
        ReactionUpdate, Sentiment, ShareGrant, ShareToken, SubmitFeedback, TopViewedArtifact,
        ViewCounts, Viewer, ViewerNotification, ViewerView, WebhookDelivery, WebhookEvent,
        WebhookId, WebhookSummary,
    },
    persistence::{
        db::{self, Database, DbPool},
        discord_inbound::DiscordInboundStore,
        discord_organization::{
            ArtifactDiscussionOverride, DiscordCredentialReadiness, OrganizationDiscordStore,
        },
        discussions::{
            ArtifactDiscussion, CreateNotificationThreadConnection, DiscussionConnectionStrategy,
            DiscussionMode, DiscussionState, DiscussionStore,
        },
        feedback, feedback_delivery,
        feedback_delivery::DeliveryPlanningContext,
        keys::{self, KeyStore},
        migrations::{self, MigrationContext},
        notifications,
        orgs::OrgStore,
        outbox::OutboxRepository,
        reactions, shares, views,
        webhooks::WebhookStore,
    },
    ports::{
        AdminService, ArtifactDiscussionOverrideView, ArtifactDiscussionView, ArtifactService,
        BoxFuture, DiscussionConnectionView, DiscussionModeRequest, DiscussionOverrideRequest,
        DiscussionService, EngagementService, HealthProbe, NotificationSink,
        OrganizationThreadingView, PreviewService, PublisherAuthenticator, ShareService,
        ViewerIdentity,
        discussions::OrganizationDiscordCredentialService,
        integrations::{HealthReport, PreviewPriority},
    },
    render::portal::AskamaPageRenderer,
    security::{
        audit::{AuditAccess, AuditEvent, MutationAudit, parse_hmac_key},
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
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use rusqlite::OptionalExtension as _;
use thiserror::Error;
use tokio::{net::TcpListener, sync::watch, task::JoinSet};
use tower::ServiceExt as _;
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
    DeliveryWorkersStarted,
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
    audit_key: [u8; 32],
}

impl AdminService for ProductionAdmin {
    fn list_keys(&self) -> BoxFuture<'_, Result<Vec<PublisherKeySummary>, AppError>> {
        Box::pin(self.keys.list_keys())
    }

    fn create_key(
        &self,
        request: CreatePublisherKey,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<CreatedPublisherKey, AppError>> {
        Box::pin(self.keys.create_key_audited(request, audit, self.audit_key))
    }

    fn revoke_key<'a>(
        &'a self,
        client_id: &'a ClientId,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<bool, AppError>> {
        Box::pin(
            self.keys
                .revoke_key_audited(client_id.clone(), audit, self.audit_key),
        )
    }

    fn update_key(
        &self,
        client_id: ClientId,
        request: artifact_mcp::model::UpdatePublisherKey,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<Option<artifact_mcp::model::UpdatedPublisherKey>, AppError>> {
        Box::pin(
            self.keys
                .update_key_audited(client_id, request, audit, self.audit_key),
        )
    }

    fn set_key_owner(
        &self,
        client_id: ClientId,
        owner_email: Option<String>,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<Option<artifact_mcp::model::KeyOwnerUpdate>, AppError>> {
        Box::pin(
            self.keys
                .set_key_owner_audited(client_id, owner_email, audit, self.audit_key),
        )
    }

    fn backfill_key_owner(
        &self,
        client_id: ClientId,
        owner_email: String,
        confirm: bool,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<Option<artifact_mcp::model::OwnerBackfillResult>, AppError>> {
        // A preview is intentionally not ledgered: it changes no state and must not become a
        // target-existence oracle. Confirmed backfills are atomically ledgered.
        if confirm {
            Box::pin(self.keys.backfill_key_owner_audited(
                client_id,
                owner_email,
                true,
                audit,
                self.audit_key,
            ))
        } else {
            Box::pin(self.keys.backfill_key_owner(client_id, owner_email, false))
        }
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
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<Organization, AppError>> {
        Box::pin(self.orgs.create_org_audited(request, audit, self.audit_key))
    }

    fn delete_org<'a>(
        &'a self,
        org: &'a OrgId,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<bool, AppError>> {
        Box::pin(
            self.orgs
                .delete_org_audited(org.clone(), audit, self.audit_key),
        )
    }

    fn add_domain<'a>(
        &'a self,
        org: &'a OrgId,
        domain: &'a str,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<String, AppError>> {
        Box::pin(self.orgs.add_domain_audited(
            org.clone(),
            domain.to_owned(),
            audit,
            self.audit_key,
        ))
    }

    fn remove_domain<'a>(
        &'a self,
        org: &'a OrgId,
        domain: &'a str,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<bool, AppError>> {
        Box::pin(self.orgs.remove_domain_audited(
            org.clone(),
            domain.to_owned(),
            audit,
            self.audit_key,
        ))
    }

    fn add_email_member<'a>(
        &'a self,
        org: &'a OrgId,
        email: &'a EmailAddress,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<EmailAddress, AppError>> {
        Box::pin(self.orgs.add_email_member_audited(
            org.clone(),
            email.clone(),
            audit,
            self.audit_key,
        ))
    }

    fn remove_email_member<'a>(
        &'a self,
        org: &'a OrgId,
        email: &'a EmailAddress,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<bool, AppError>> {
        Box::pin(self.orgs.remove_email_member_audited(
            org.clone(),
            email.clone(),
            audit,
            self.audit_key,
        ))
    }

    fn categories<'a>(&'a self, org: &'a OrgId) -> BoxFuture<'a, Result<Vec<String>, AppError>> {
        Box::pin(self.orgs.categories(org))
    }

    fn add_category<'a>(
        &'a self,
        org: &'a OrgId,
        name: &'a str,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<String, AppError>> {
        Box::pin(self.orgs.add_category_audited(
            org.clone(),
            name.to_owned(),
            audit,
            self.audit_key,
        ))
    }

    fn remove_category<'a>(
        &'a self,
        org: &'a OrgId,
        name: &'a str,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<bool, AppError>> {
        Box::pin(self.orgs.remove_category_audited(
            org.clone(),
            name.to_owned(),
            audit,
            self.audit_key,
        ))
    }

    fn color_map(&self) -> BoxFuture<'_, Result<BTreeMap<OrgId, Option<String>>, AppError>> {
        Box::pin(self.orgs.color_map())
    }

    fn set_color<'a>(
        &'a self,
        org: &'a OrgId,
        color: Option<&'a str>,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<Option<String>, AppError>> {
        Box::pin(self.orgs.set_color_audited(
            org.clone(),
            color.map(str::to_owned),
            audit,
            self.audit_key,
        ))
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
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<WebhookSummary, AppError>> {
        Box::pin(self.webhooks.create_audited(request, audit, self.audit_key))
    }

    fn remove_webhook<'a>(
        &'a self,
        org: &'a OrgId,
        id: &'a WebhookId,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<bool, AppError>> {
        Box::pin(
            self.webhooks
                .remove_audited(org.clone(), id.clone(), audit, self.audit_key),
        )
    }

    fn set_webhook_events<'a>(
        &'a self,
        org: &'a OrgId,
        id: &'a WebhookId,
        events: &'a [WebhookEvent],
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<Option<WebhookSummary>, AppError>> {
        Box::pin(self.webhooks.set_events_audited(
            org.clone(),
            id.clone(),
            events.to_vec(),
            audit,
            self.audit_key,
        ))
    }

    fn webhook_delivery<'a>(
        &'a self,
        id: &'a WebhookId,
    ) -> BoxFuture<'a, Result<Option<WebhookDelivery>, AppError>> {
        Box::pin(self.webhooks.delivery(id))
    }

    fn audit_webhook_test<'a>(
        &'a self,
        org: &'a OrgId,
        id: &'a WebhookId,
        outcome: Option<bool>,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<(), AppError>> {
        Box::pin(
            self.webhooks
                .audit_test(org.clone(), id.clone(), outcome, audit, self.audit_key),
        )
    }
}

#[derive(Clone)]
struct ProductionDiscussions {
    store: DiscussionStore,
    organization: OrganizationDiscordStore,
    inbound: DiscordInboundStore,
    webhooks: Arc<WebhookStore>,
    public_base_url: String,
    audit_key: [u8; 32],
}

impl ProductionDiscussions {
    fn connection_view(
        summary: Option<artifact_mcp::persistence::discussions::DiscussionConnectionSummary>,
        bot_configured: bool,
    ) -> DiscussionConnectionView {
        summary.map_or_else(
            || DiscussionConnectionView {
                configured: false,
                label: String::new(),
                destination: String::new(),
                strategy: "notification_thread".to_owned(),
                webhook_id: None,
                bot_configured,
                last_error: None,
            },
            |summary| DiscussionConnectionView {
                configured: true,
                label: summary.label,
                destination: summary.destination,
                strategy: match summary.strategy {
                    DiscussionConnectionStrategy::ForumWebhook => "forum_webhook",
                    DiscussionConnectionStrategy::NotificationThread => "notification_thread",
                }
                .to_owned(),
                webhook_id: summary.notification_webhook_id,
                bot_configured,
                last_error: summary.last_error,
            },
        )
    }

    fn discussion_view(discussion: ArtifactDiscussion) -> ArtifactDiscussionView {
        let mode = match discussion.mode {
            DiscussionMode::ArtifactMcpOnly => "artifact_only",
            DiscussionMode::DiscordMirror => "discord_mirror",
        };
        let state = match discussion.state {
            DiscussionState::Local => "local",
            DiscussionState::Pending => "pending",
            DiscussionState::Connected => "connected",
            DiscussionState::Paused => "paused",
            DiscussionState::Failed => "failed",
        };
        ArtifactDiscussionView {
            mode: mode.to_owned(),
            state: state.to_owned(),
            enabled: discussion.mode == DiscussionMode::DiscordMirror,
            connection_configured: discussion.connection_id.is_some(),
            // Provider diagnostics may contain remote body text. The route exposes only a stable
            // state, never that untrusted detail.
            last_error: None,
        }
    }

    fn organization_view(
        status: artifact_mcp::persistence::discord_organization::OrganizationThreadingStatus,
    ) -> OrganizationThreadingView {
        OrganizationThreadingView {
            credential: match status.credential_readiness {
                DiscordCredentialReadiness::Configured => "configured",
                DiscordCredentialReadiness::LegacyFallback => "fallback",
                DiscordCredentialReadiness::Unconfigured
                | DiscordCredentialReadiness::Deactivated => "missing",
            }
            .to_owned(),
            enabled: status.outbound_enabled,
            degraded: status.recovery_state == "degraded"
                || matches!(
                    status.credential_readiness,
                    DiscordCredentialReadiness::Unconfigured
                        | DiscordCredentialReadiness::Deactivated
                ),
            recovery_state: status.recovery_state.to_owned(),
            recovery_pending: status.recovery_pending,
        }
    }

    async fn validate_credential(
        &self,
        org: &OrgId,
        token: artifact_mcp::config::Secret,
    ) -> Result<(), AppError> {
        let transport = DiscordDiscussionTransport::with_bot_token(Some(token))?;
        let Some(summary) = self.store.connection_summary(org).await? else {
            return transport.validate_bot_token().await;
        };
        let delivery = self
            .store
            .connection_for_delivery(&summary.id, org)
            .await?
            .ok_or_else(|| {
                AppError::Validation(
                    "Configure an organization notification destination first.".to_owned(),
                )
            })?;
        if delivery.strategy != DiscussionConnectionStrategy::NotificationThread {
            return Err(AppError::Validation(
                "Select a notification-thread destination.".to_owned(),
            ));
        }
        let destination = transport
            .inspect_notification_webhook(&delivery.url)
            .await?;
        if delivery.channel_id.as_deref() != Some(destination.channel_id.as_str())
            || delivery.guild_id.as_deref() != Some(destination.guild_id.as_str())
        {
            return Err(AppError::Validation(
                "The credential does not match the selected Discord destination.".to_owned(),
            ));
        }
        Ok(())
    }

    async fn artifact_override_view(
        &self,
        artifact: &artifact_mcp::model::ArtifactMeta,
    ) -> Result<ArtifactDiscussionOverrideView, AppError> {
        let policy = self
            .organization
            .effective_policy(&artifact.id, &artifact.org)
            .await?;
        let discussion = self
            .store
            .get_discussion(&artifact.id, &artifact.org)
            .await?;
        let inbound = self
            .inbound
            .policy_status(&artifact.id, &artifact.org)
            .await?;
        let effective_outbound = policy.effective_outbound
            || discussion
                .as_ref()
                .is_some_and(|row| row.mode == DiscussionMode::DiscordMirror);
        let state = discussion.as_ref().map_or("local", |row| match row.state {
            DiscussionState::Local => "local",
            DiscussionState::Pending => "pending",
            DiscussionState::Connected => "connected",
            DiscussionState::Paused => "local",
            DiscussionState::Failed => "failed",
        });
        Ok(ArtifactDiscussionOverrideView {
            override_mode: if inbound.enabled {
                "discord_two_way"
            } else {
                match policy.artifact_override {
                    ArtifactDiscussionOverride::Inherit => "inherit",
                    ArtifactDiscussionOverride::ArtifactOnly => "artifact_only",
                }
            }
            .to_owned(),
            effective_mode: if inbound.enabled {
                "discord_two_way"
            } else if effective_outbound {
                "discord_mirror"
            } else {
                "artifact_only"
            }
            .to_owned(),
            state: if inbound.enabled {
                inbound.health
            } else {
                state.to_owned()
            },
            actionable_error: if inbound.enabled && !inbound.safe_error.is_empty() {
                Some(inbound.safe_error)
            } else if policy.outbound_enabled
                && !effective_outbound
                && policy.artifact_override == ArtifactDiscussionOverride::Inherit
            {
                Some("threading_unavailable".to_owned())
            } else {
                None
            },
        })
    }

    async fn recovery_destination(
        &self,
        org: &OrgId,
    ) -> Result<artifact_mcp::persistence::discord_organization::RecoveryDestination, AppError>
    {
        let summary = self.store.connection_summary(org).await?.ok_or_else(|| {
            AppError::Validation(
                "Configure a notification-thread destination before enabling threading.".to_owned(),
            )
        })?;
        let delivery = self
            .store
            .connection_for_delivery(&summary.id, org)
            .await?
            .ok_or(AppError::Internal)?;
        if delivery.strategy != DiscussionConnectionStrategy::NotificationThread {
            return Err(AppError::Validation(
                "Select a notification-thread destination.".to_owned(),
            ));
        }
        Ok(
            artifact_mcp::persistence::discord_organization::RecoveryDestination {
                connection_id: summary.id,
                notification_webhook_id: delivery
                    .notification_webhook_id
                    .ok_or(AppError::Internal)?,
                provider_webhook_id: delivery.notification_provider_webhook_id.ok_or_else(
                    || {
                        AppError::Conflict(
                            "Re-save the selected Discord destination before enabling threading."
                                .to_owned(),
                        )
                    },
                )?,
                guild_id: delivery.guild_id.ok_or(AppError::Internal)?,
                channel_id: delivery.channel_id.ok_or(AppError::Internal)?,
            },
        )
    }
}

impl DiscussionService for ProductionDiscussions {
    fn organization_threading<'a>(
        &'a self,
        org: &'a OrgId,
    ) -> BoxFuture<'a, Result<OrganizationThreadingView, AppError>> {
        Box::pin(async move {
            self.organization
                .organization_status(org)
                .await
                .map(Self::organization_view)
        })
    }

    fn save_organization_threading(
        &self,
        org: OrgId,
        bot_token: String,
        enabled: bool,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<OrganizationThreadingView, AppError>> {
        Box::pin(async move {
            let recovery_destination = if enabled {
                Some(self.recovery_destination(&org).await?)
            } else {
                None
            };
            let rotating = !bot_token.trim().is_empty();
            // Disabling the organization default is an entirely local safety action. It must
            // remain available while Discord is down and before any credential exists. A token
            // rotation still requires provider validation even when the outbound policy remains
            // disabled.
            let rotation = if rotating {
                let token = artifact_mcp::config::Secret::new(bot_token);
                self.validate_credential(&org, token.clone()).await?;
                Some(token)
            } else if enabled {
                let token = self
                    .organization
                    .credential_for_provider(&org)
                    .await?
                    .ok_or_else(|| {
                        AppError::Validation(
                            "A Discord bot credential is required before enabling threading."
                                .to_owned(),
                        )
                    })?;
                self.validate_credential(&org, token).await?;
                None
            } else {
                None
            };
            self.organization
                .save_validated_credential_and_policy_audited(
                    org.clone(),
                    rotation,
                    enabled,
                    audit.clone(),
                    self.audit_key,
                )
                .await?;
            if let Some(destination) = recovery_destination {
                self.organization
                    .queue_recoveries_for_org_audited(
                        org.clone(),
                        destination,
                        self.public_base_url.clone(),
                        audit,
                        self.audit_key,
                    )
                    .await?;
            }
            self.organization
                .organization_status(&org)
                .await
                .map(Self::organization_view)
        })
    }

    fn test_organization_credential(
        &self,
        org: OrgId,
        _audit: MutationAudit,
    ) -> BoxFuture<'_, Result<bool, AppError>> {
        Box::pin(async move {
            let token = self
                .organization
                .credential_for_provider(&org)
                .await?
                .ok_or_else(|| {
                    AppError::Validation(
                        "No organization Discord credential is configured.".to_owned(),
                    )
                })?;
            self.validate_credential(&org, token).await?;
            Ok(true)
        })
    }

    fn remove_organization_credential(
        &self,
        org: OrgId,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<bool, AppError>> {
        Box::pin(async move {
            self.organization
                .deactivate_credential_audited(org, audit, self.audit_key)
                .await
        })
    }

    fn queue_historical_recovery(
        &self,
        org: OrgId,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<bool, AppError>> {
        Box::pin(async move {
            let destination = self.recovery_destination(&org).await?;
            let queued = self
                .organization
                .queue_recoveries_for_org_audited(
                    org,
                    destination,
                    self.public_base_url.clone(),
                    audit,
                    self.audit_key,
                )
                .await?;
            Ok(queued > 0)
        })
    }

    fn artifact_override<'a>(
        &'a self,
        artifact: &'a artifact_mcp::model::ArtifactMeta,
    ) -> BoxFuture<'a, Result<ArtifactDiscussionOverrideView, AppError>> {
        Box::pin(async move { self.artifact_override_view(artifact).await })
    }

    fn set_artifact_override(
        &self,
        artifact: artifact_mcp::model::ArtifactMeta,
        override_mode: DiscussionOverrideRequest,
        _actor: String,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<ArtifactDiscussionOverrideView, AppError>> {
        Box::pin(async move {
            match override_mode {
                DiscussionOverrideRequest::DiscordTwoWay => {
                    let effective = self
                        .organization
                        .effective_policy(&artifact.id, &artifact.org)
                        .await?;
                    let credential = self
                        .organization
                        .credential_for_provider(&artifact.org)
                        .await?;
                    if !effective.effective_outbound || credential.is_none() {
                        return Err(AppError::Conflict(
                            "The organization Discord credential and outbound threading policy must be ready before enabling two-way sync."
                                .to_owned(),
                        ));
                    }
                    let thread_id = self
                        .store
                        .get_discussion(&artifact.id, &artifact.org)
                        .await?
                        .filter(|discussion| {
                            discussion.mode == DiscussionMode::DiscordMirror
                                && discussion.state == DiscussionState::Connected
                        })
                        .and_then(|discussion| discussion.thread_id)
                        .ok_or_else(|| {
                            AppError::Conflict(
                                "A mapped Discord thread must be connected before enabling two-way sync."
                                    .to_owned(),
                            )
                        })?;
                    DiscordInboundReadinessRest::new()?
                        .validate_thread(credential.as_ref().ok_or(AppError::Internal)?, &thread_id)
                        .await?;
                    let mut gateway_ready = self
                        .inbound
                        .request_gateway_readiness(&artifact.id, &artifact.org)
                        .await?;
                    for _ in 0..20 {
                        if gateway_ready {
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                        gateway_ready = self.inbound.gateway_ready(&artifact.org).await?;
                    }
                    if !gateway_ready {
                        return Err(AppError::Conflict(
                            "The Discord Gateway is connecting or unavailable. Verify the Message Content intent and retry two-way sync."
                                .to_owned(),
                        ));
                    }
                    self.organization
                        .set_artifact_override_audited(
                            artifact.id.clone(),
                            artifact.org.clone(),
                            ArtifactDiscussionOverride::Inherit,
                            audit.clone(),
                            self.audit_key,
                        )
                        .await?;
                    self.inbound
                        .set_policy_audited(
                            artifact.id.clone(),
                            artifact.org.clone(),
                            true,
                            audit,
                            self.audit_key,
                        )
                        .await?;
                }
                DiscussionOverrideRequest::Inherit | DiscussionOverrideRequest::ArtifactOnly => {
                    self.inbound
                        .set_policy_audited(
                            artifact.id.clone(),
                            artifact.org.clone(),
                            false,
                            audit.clone(),
                            self.audit_key,
                        )
                        .await?;
                    self.organization
                        .set_artifact_override_audited(
                            artifact.id.clone(),
                            artifact.org.clone(),
                            match override_mode {
                                DiscussionOverrideRequest::Inherit => {
                                    ArtifactDiscussionOverride::Inherit
                                }
                                DiscussionOverrideRequest::ArtifactOnly => {
                                    ArtifactDiscussionOverride::ArtifactOnly
                                }
                                DiscussionOverrideRequest::DiscordTwoWay => unreachable!(),
                            },
                            audit,
                            self.audit_key,
                        )
                        .await?;
                }
            }
            self.artifact_override_view(&artifact).await
        })
    }

    fn connection<'a>(
        &'a self,
        org: &'a OrgId,
    ) -> BoxFuture<'a, Result<DiscussionConnectionView, AppError>> {
        Box::pin(async move {
            let configured = self
                .organization
                .credential_readiness(org)
                .await
                .is_ok_and(|value| {
                    matches!(
                        value,
                        DiscordCredentialReadiness::Configured
                            | DiscordCredentialReadiness::LegacyFallback
                    )
                });
            Ok(Self::connection_view(
                self.store.connection_summary(org).await?,
                configured,
            ))
        })
    }

    fn configure_connection(
        &self,
        org: OrgId,
        webhook_id: String,
        label: String,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<DiscussionConnectionView, AppError>> {
        Box::pin(async move {
            let token = self
                .organization
                .credential_for_provider(&org)
                .await?
                .ok_or_else(|| {
                    AppError::Validation(
                        "Save an organization Discord credential before selecting a destination."
                            .to_owned(),
                    )
                })?;
            let transport = DiscordDiscussionTransport::with_bot_token(Some(token))?;
            let webhook_id = WebhookId(webhook_id);
            let delivery = self
                .webhooks
                .delivery(&webhook_id)
                .await?
                .filter(|delivery| delivery.org == org)
                .ok_or_else(|| {
                    AppError::Validation(
                        "Select an existing webhook for this organization.".to_owned(),
                    )
                })?;
            if !delivery.events.contains(&WebhookEvent::Published) {
                return Err(AppError::Validation(
                    "The selected webhook must subscribe to published artifacts.".to_owned(),
                ));
            }
            let destination = transport
                .inspect_notification_webhook(&delivery.url)
                .await?;
            let summary = self
                .store
                .upsert_notification_thread_connection_audited(
                    CreateNotificationThreadConnection {
                        org,
                        notification_webhook_id: webhook_id.0,
                        notification_provider_webhook_id: destination.webhook_id,
                        channel_id: destination.channel_id,
                        guild_id: destination.guild_id,
                        label,
                    },
                    audit,
                    self.audit_key,
                )
                .await?;
            Ok(Self::connection_view(Some(summary), true))
        })
    }

    fn remove_connection(
        &self,
        org: OrgId,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<bool, AppError>> {
        Box::pin(async move {
            self.store
                .remove_connection_audited(org, audit, self.audit_key)
                .await
        })
    }

    fn test_connection(
        &self,
        org: OrgId,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<bool, AppError>> {
        Box::pin(async move {
            let transport = DiscordDiscussionTransport::with_bot_token(
                self.organization.credential_for_provider(&org).await?,
            )?;
            let Some(summary) = self.store.connection_summary(&org).await? else {
                return Err(AppError::Validation(
                    "Discord discussion connection is not configured.".to_owned(),
                ));
            };
            // Commit requested evidence before making the external call. The URL is resolved only
            // below this boundary and never enters an audit event, response, error, or debug log.
            self.store
                .audit_connection_test(
                    org.clone(),
                    summary.id.clone(),
                    None,
                    audit.clone(),
                    self.audit_key,
                )
                .await?;
            // Every post-request failure is converted to a fixed failed outcome so the completed
            // audit marker is attempted even when lookup, decrypt, request construction, or
            // transport delivery fails. The only early return remaining is an audit failure.
            let success = match self.store.connection_for_delivery(&summary.id, &org).await {
                Ok(Some(delivery))
                    if delivery.strategy == DiscussionConnectionStrategy::NotificationThread =>
                {
                    match delivery.channel_id {
                        Some(channel_id) => {
                            transport
                                .test_notification_thread(
                                    &delivery.url,
                                    &format!("discussion:{}", summary.id),
                                    &channel_id,
                                    &delivery.label,
                                )
                                .await
                        }
                        None => false,
                    }
                }
                Ok(Some(delivery)) => {
                    let operation = artifact_mcp::integrations::discord_discussion::DiscussionOperation::create_thread(
                        "Artifact MCP connection test".to_owned(),
                        "Artifact MCP Discord discussion connection test. This visible post confirms delivery and is not linked to an artifact.".to_owned(),
                    );
                    match operation.and_then(|operation| {
                        artifact_mcp::integrations::discord_discussion::discussion_request(
                            &delivery.url,
                            format!("discussion:{}", summary.id),
                            operation,
                        )
                    }) {
                        Ok(request) => matches!(
                            transport.deliver(request).await,
                            DiscussionResult::Accepted { .. }
                        ),
                        Err(_) => false,
                    }
                }
                Ok(None) | Err(_) => false,
            };
            self.store
                .audit_connection_test(org, summary.id, Some(success), audit, self.audit_key)
                .await?;
            if success {
                Ok(true)
            } else {
                Err(AppError::Unavailable(
                    "discord discussion unavailable".to_owned(),
                ))
            }
        })
    }

    fn status<'a>(
        &'a self,
        artifact: &'a artifact_mcp::model::ArtifactMeta,
    ) -> BoxFuture<'a, Result<ArtifactDiscussionView, AppError>> {
        Box::pin(async move {
            let discussion = self
                .store
                .get_discussion(&artifact.id, &artifact.org)
                .await?
                .unwrap_or_else(|| {
                    ArtifactDiscussion::local_only(artifact.id.clone(), artifact.org.clone())
                });
            let mut view = Self::discussion_view(discussion);
            view.connection_configured = self
                .store
                .connection_summary(&artifact.org)
                .await?
                .is_some();
            Ok(view)
        })
    }

    fn set_mode(
        &self,
        artifact: artifact_mcp::model::ArtifactMeta,
        mode: DiscussionModeRequest,
        actor: String,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<ArtifactDiscussionView, AppError>> {
        Box::pin(async move {
            let desired = match mode {
                DiscussionModeRequest::ArtifactOnly => DiscussionMode::ArtifactMcpOnly,
                DiscussionModeRequest::DiscordMirror => DiscussionMode::DiscordMirror,
            };
            let next = self
                .store
                .set_mode_audited(
                    artifact.id,
                    artifact.org.clone(),
                    desired,
                    actor,
                    audit,
                    self.audit_key,
                )
                .await?;
            let mut view = Self::discussion_view(next);
            view.connection_configured = self
                .store
                .connection_summary(&artifact.org)
                .await?
                .is_some();
            Ok(view)
        })
    }

    fn retry(
        &self,
        artifact: artifact_mcp::model::ArtifactMeta,
        actor: String,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<ArtifactDiscussionView, AppError>> {
        Box::pin(async move {
            let next = self
                .store
                .retry_audited(
                    artifact.id,
                    artifact.org.clone(),
                    actor,
                    audit,
                    self.audit_key,
                )
                .await?;
            let mut view = Self::discussion_view(next);
            view.connection_configured = self
                .store
                .connection_summary(&artifact.org)
                .await?
                .is_some();
            Ok(view)
        })
    }
}

#[derive(Clone)]
struct ProductionEngagement {
    pool: DbPool,
    ids: Arc<dyn IdSource>,
    feedback_max_body: u64,
    public_base_url: String,
    delivery_planning: DeliveryPlanningContext,
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
        let public_base_url = self.public_base_url.clone();
        let planning = self.delivery_planning.clone();
        Box::pin(async move {
            db::interact(&pool, move |conn| {
                feedback_delivery::submit(
                    conn,
                    ids.as_ref(),
                    &planning,
                    &public_base_url,
                    &meta,
                    &submission,
                    max_body,
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
        let planning = self.delivery_planning.clone();
        Box::pin(async move {
            db::interact(&pool, move |conn| {
                feedback_delivery::delete_as_viewer_with_delivery(
                    conn, &planning, &meta, &viewer, id,
                )
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
        let public_base_url = self.public_base_url.clone();
        let planning = self.delivery_planning.clone();
        Box::pin(async move {
            db::interact(&pool, move |conn| {
                feedback_delivery::resolve_as_viewer(
                    conn,
                    &planning,
                    &public_base_url,
                    &meta,
                    &viewer,
                    id,
                )
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
        artifact: artifact_mcp::security::access::OwnedArtifact,
        id: FeedbackId,
        resolved_by: String,
    ) -> BoxFuture<'_, Result<bool, AppError>> {
        let pool = self.pool.clone();
        let meta = artifact.meta().clone();
        let public_base_url = self.public_base_url.clone();
        let planning = self.delivery_planning.clone();
        Box::pin(async move {
            db::interact(&pool, move |conn| {
                feedback_delivery::resolve_as_publisher(
                    conn,
                    &planning,
                    &public_base_url,
                    &meta,
                    id,
                    &resolved_by,
                )
            })
            .await
        })
    }

    fn reopen_feedback_as_publisher(
        &self,
        artifact: artifact_mcp::security::access::OwnedArtifact,
        id: FeedbackId,
    ) -> BoxFuture<'_, Result<bool, AppError>> {
        let pool = self.pool.clone();
        let meta = artifact.meta().clone();
        let planning = self.delivery_planning.clone();
        Box::pin(async move {
            db::interact(&pool, move |conn| {
                feedback_delivery::reopen_as_publisher_with_delivery(conn, &planning, &meta, id)
            })
            .await
        })
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
    audit_key: [u8; 32],
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
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<PublicShare, AppError>> {
        let ids = Arc::clone(&self.ids);
        let clock = Arc::clone(&self.clock);
        Box::pin(shares::create_audited_pooled(
            &self.pool,
            ids,
            clock,
            artifact,
            request,
            audit,
            self.audit_key,
        ))
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
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<bool, AppError>> {
        Box::pin(shares::revoke_audited_pooled(
            &self.pool,
            artifact,
            token,
            audit,
            self.audit_key,
        ))
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
    delivery_runtime: DeliveryRuntime,
    recovery_runtime: DiscordRecoveryRuntime,
    inbound_runtime: DiscordInboundRuntime,
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
async fn seed_configured_keys(
    pool: &DbPool,
    seed_keys: SeedKeys,
    audit_key: [u8; 32],
) -> Result<u64, AppError> {
    for client_id in &seed_keys.ignored_placeholders {
        tracing::warn!(
            client_id = %client_id,
            "ignoring placeholder publisher key secret"
        );
    }
    let pool = pool.clone();
    db::interact(&pool, move |conn| {
        let tx = conn.transaction().map_err(|_| AppError::Internal)?;
        let mut statement = tx
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
        drop(statement);
        if seeded > 0 {
            let audit = MutationAudit::maintenance()?;
            let event = AuditEvent {
                operation: "key.seed".to_owned(),
                target_type: "key_set".to_owned(),
                target_id: String::new(),
                result: "success".to_owned(),
                classification: "bootstrap_key_seed".to_owned(),
                revision: None,
            };
            artifact_mcp::security::audit::append_in_transaction(
                &tx,
                &audit_key,
                &audit.event_id()?,
                audit.context(),
                &event,
            )?;
        }
        tx.commit().map_err(|_| AppError::Internal)?;
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
        .layer(middleware::from_fn(attach_audit_request_id))
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

    let audit_key = parse_hmac_key(
        config.audit_ledger_hmac_key.as_ref().ok_or_else(|| AppError::Validation("AUDIT_LEDGER_HMAC_KEY is required; refusing to start without a tamper-evident audit ledger".to_owned()))?.expose(),
    )?;
    let seeded = seed_configured_keys(&pool, config.seed_keys.clone(), audit_key).await?;
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
    let ingress = Arc::new(artifact_mcp::http::ingress::IngressState::from_config(
        &config,
    ));
    let previews = Arc::new(PreviewIntegration::from_config_with_queue_counter(
        &config,
        Some(ingress.preview_queue_rejection_counter()),
        Some(ingress.preview_queue_pressure()),
    ));
    let artifacts = Arc::new(
        ArtifactStore::from_config(pool.clone(), &config, Arc::clone(&ids))?
            .with_post_commit_preview_scheduler(Arc::new(PersistentThumbnailScheduler::new(
                Arc::clone(&previews),
            ))),
    );
    let storage = artifacts.audit_storage(true).await.inspect_err(|_error| {
        artifact_mcp::observability::record_global_security_signal("reconciliation_failure");
    })?;
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

    let digests = artifacts
        .backfill_body_digests()
        .await
        .inspect_err(|_error| {
            artifact_mcp::observability::record_global_security_signal("reconciliation_failure");
        })?;
    tracing::info!(
        scanned = digests.scanned,
        updated = digests.updated,
        "artifact digest backfill complete"
    );

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
        Arc::clone(&protection),
    ));
    let discussions = Arc::new(DiscussionStore::new(pool.clone(), Arc::clone(&protection)));
    let organization_discord = OrganizationDiscordStore::new(
        pool.clone(),
        Arc::clone(&protection),
        config.discord_bot_token.clone(),
    );
    let discord = Arc::new(DiscordNotifier::new(
        Arc::clone(&webhooks),
        Arc::new(HttpTransport::new()?),
    ));
    let notifications: Arc<dyn NotificationSink> =
        Arc::new(ArtifactPreviewNotifier::from_artifact_store(
            Arc::clone(&artifacts),
            Arc::clone(&previews),
            discord,
        ));
    // Durable workers start only after storage reconciliation, digest recovery, and preview
    // cache reconciliation have finished.
    let delivery_telemetry = DeliveryTelemetry::default();
    let worker_discussions: Arc<
        dyn artifact_mcp::integrations::delivery_worker::WorkerDiscussions,
    > = discussions.clone();
    let inbound_store = DiscordInboundStore::new(pool.clone());
    let discussion_service: Arc<dyn DiscussionService> = Arc::new(ProductionDiscussions {
        store: discussions.as_ref().clone(),
        organization: organization_discord.clone(),
        inbound: inbound_store.clone(),
        webhooks: Arc::clone(&webhooks),
        public_base_url: config.public_base_url.clone(),
        audit_key,
    });
    let worker_discussion_provider: Arc<
        dyn artifact_mcp::integrations::delivery_worker::WorkerDiscussionProvider,
    > = Arc::new(OrganizationDiscordDiscussionProvider::new(Arc::new(
        organization_discord.clone(),
    )));
    let (delivery_runtime, delivery_wake) = DeliveryRuntime::start(
        Arc::new(OutboxRepository::new(pool.clone())),
        Arc::clone(&webhooks),
        Arc::new(DiscordProviderTransport::new()?),
        worker_discussions,
        worker_discussion_provider,
        Arc::new(ArtifactDeliveryPreviewResolver::new(
            Arc::clone(&artifacts),
            Arc::clone(&previews),
        )),
        delivery_telemetry.clone(),
    );
    let recovery_runtime = DiscordRecoveryRuntime::start(
        organization_discord.clone(),
        Arc::new(DiscordHistoryRest::new()?),
        audit_key,
    );
    let inbound_runtime = DiscordInboundRuntime::start(
        config.discord_inbound_enabled,
        inbound_store,
        Arc::new(organization_discord),
    );
    observer.stage(StartupStage::DeliveryWorkersStarted);
    let admin: Arc<dyn AdminService> = Arc::new(ProductionAdmin {
        keys: key_store,
        orgs: OrgStore::new(pool.clone()),
        webhooks: webhooks.as_ref().clone(),
        audit_key,
    });
    let engagement: Arc<dyn EngagementService> = Arc::new(ProductionEngagement {
        pool: pool.clone(),
        ids: Arc::clone(&ids),
        feedback_max_body: config.storage.feedback_max_body,
        public_base_url: config.public_base_url.clone(),
        delivery_planning: DeliveryPlanningContext::production(),
    });
    let shares: Arc<dyn ShareService> = Arc::new(ProductionShares {
        pool: pool.clone(),
        ids,
        clock: Arc::new(SystemClock),
        audit_key,
    });
    let health: Arc<dyn HealthProbe> =
        Arc::new(ProductionHealth::new(pool.clone(), config.artifact_dir()));
    let audit_access = config
        .audit_ledger_hmac_key
        .as_ref()
        .map(|encoded| parse_hmac_key(encoded.expose()))
        .transpose()?
        .map(|key| Arc::new(AuditAccess::new(pool, key)));

    let host = config.listen_host.clone();
    let port = config.port;
    let deps = AppDeps {
        publisher_auth,
        viewer_identity,
        artifacts,
        admin,
        discussions: discussion_service,
        engagement,
        shares,
        pages: Arc::new(AskamaPageRenderer::from_config(&config)),
        previews,
        notifications,
        health,
        ingress,
        preview_tasks: artifact_mcp::mcp::tasks::PreviewTaskStore::new(&config.data_dir),
        mcp_telemetry: artifact_mcp::observability::McpTelemetry::default(),
        delivery_telemetry,
        delivery_wake,
        audit_access,
        config,
    };
    artifact_mcp::mcp::tasks::resume_preview_tasks(deps.clone());
    let router = runtime_router(deps);
    Ok(Bootstrapped {
        router,
        host,
        port,
        delivery_runtime,
        recovery_runtime,
        inbound_runtime,
    })
}

#[tracing::instrument(skip_all)]
#[allow(dead_code)] // exercised by the listener-free native runtime tests
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
    let result = bind_and_serve(bootstrapped.host, bootstrapped.port, bootstrapped.router).await;
    bootstrapped.delivery_runtime.shutdown().await;
    bootstrapped.recovery_runtime.shutdown().await;
    bootstrapped.inbound_runtime.shutdown().await;
    result
}

#[tracing::instrument(skip_all)]
async fn serve(config: AppConfig) -> Result<(), RuntimeError> {
    let ingress = config.ingress.clone();
    let observer = Arc::new(NoopStartupObserver);
    let bootstrapped = bootstrap(config, observer.clone()).await?;
    observer.stage(StartupStage::ListenerBindRequested);
    serve_listener(
        bootstrapped.host,
        bootstrapped.port,
        bootstrapped.router,
        ingress,
        bootstrapped.delivery_runtime,
        bootstrapped.recovery_runtime,
        bootstrapped.inbound_runtime,
    )
    .await
}

/// HTTP/1 listener boundary. Hyper owns header parsing here so slowloris/header limits apply
/// before Axum constructs a request; body deadlines remain in the JSON/MCP readers because a
/// blanket handler timeout would make durable mutations ambiguously succeed after a client-side
/// timeout.
async fn serve_listener(
    host: String,
    port: u16,
    router: Router,
    ingress: artifact_mcp::config::IngressConfig,
    delivery_runtime: DeliveryRuntime,
    recovery_runtime: DiscordRecoveryRuntime,
    inbound_runtime: DiscordInboundRuntime,
) -> Result<(), RuntimeError> {
    let listener = TcpListener::bind((host.as_str(), port)).await?;
    tracing::info!(listen_host = %host, port, "listener ready");
    serve_listener_with_shutdown_inner(
        listener,
        router,
        ingress,
        Some(delivery_runtime),
        Some(recovery_runtime),
        Some(inbound_runtime),
        shutdown_signal(),
    )
    .await
}

/// Serve an already-bound listener until the supplied shutdown future resolves. Keeping this
/// seam separate lets the runtime test exercise Hyper's actual header deadline and connection
/// permits without binding a process-wide signal handler.
#[allow(dead_code)] // exercised by the listener-free native runtime tests
pub(crate) async fn serve_listener_with_shutdown<F>(
    listener: TcpListener,
    router: Router,
    ingress: artifact_mcp::config::IngressConfig,
    shutdown: F,
) -> Result<(), RuntimeError>
where
    F: Future<Output = ()> + Send,
{
    serve_listener_with_shutdown_inner(listener, router, ingress, None, None, None, shutdown).await
}

async fn serve_listener_with_shutdown_inner<F>(
    listener: TcpListener,
    router: Router,
    ingress: artifact_mcp::config::IngressConfig,
    delivery_runtime: Option<DeliveryRuntime>,
    recovery_runtime: Option<DiscordRecoveryRuntime>,
    inbound_runtime: Option<DiscordInboundRuntime>,
    shutdown: F,
) -> Result<(), RuntimeError>
where
    F: Future<Output = ()> + Send,
{
    tokio::pin!(shutdown);
    let connection_permits = Arc::new(tokio::sync::Semaphore::new(
        usize::try_from(ingress.max_connections).unwrap_or(usize::MAX),
    ));
    let (stop_send, stop_receive) = watch::channel(());
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            () = &mut shutdown => break,
            Some(joined) = connections.join_next(), if !connections.is_empty() => {
                if let Err(error) = joined {
                    tracing::warn!(error = %error, "connection task ended unexpectedly");
                }
            }
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(accepted) => accepted,
                    Err(error) => {
                        tracing::warn!(error = %error, "listener accept failed");
                        continue;
                    }
                };
                let router = router.clone();
                let ingress = ingress.clone();
                let mut stop = stop_receive.clone();
                let Some(connection_permit) = connection_permits.clone().try_acquire_owned().ok() else {
                    tracing::warn!("connection admission limit reached; dropping socket");
                    drop(stream);
                    continue;
                };
                connections.spawn(async move {
                    let _connection_permit = connection_permit;
                    let service = match router
                        .into_make_service_with_connect_info::<SocketAddr>()
                        .oneshot(peer)
                        .await
                    {
                        Ok(service) => service,
                        Err(never) => match never {},
                    };
                    let mut builder = http1::Builder::new();
                    builder
                        .max_headers(usize::try_from(ingress.max_headers).unwrap_or(usize::MAX))
                        .max_buf_size(usize::try_from(ingress.max_header_bytes).unwrap_or(usize::MAX))
                        .timer(TokioTimer::new())
                        .header_read_timeout(Duration::from_millis(ingress.read_timeout_ms));
                    let mut connection = Box::pin(
                        builder.serve_connection(TokioIo::new(stream), TowerToHyperService::new(service)),
                    );
                    let result = tokio::select! {
                        result = &mut connection => result,
                        _ = stop.changed() => {
                            connection.as_mut().graceful_shutdown();
                            connection.await
                        }
                    };
                    if let Err(error) = result {
                        tracing::debug!(error = %error, "HTTP connection ended");
                    }
                });
            }
        }
    }
    // Stop accepting new sockets and ask each HTTP/1 connection to leave keep-alive mode. We
    // join rather than abort: a mutation already admitted may complete its durable work, while
    // new requests are no longer accepted on that connection.
    if let Some(delivery_runtime) = delivery_runtime {
        delivery_runtime.shutdown().await;
    }
    if let Some(recovery_runtime) = recovery_runtime {
        recovery_runtime.shutdown().await;
    }
    if let Some(inbound_runtime) = inbound_runtime {
        inbound_runtime.shutdown().await;
    }
    drop(listener);
    let _ = stop_send.send(());
    let grace = Duration::from_millis(ingress.shutdown_grace_ms);
    if tokio::time::timeout(grace, async {
        while let Some(joined) = connections.join_next().await {
            if let Err(error) = joined {
                tracing::warn!(error = %error, "connection task ended during graceful shutdown");
            }
        }
    })
    .await
    .is_err()
    {
        // This is process termination, not a success/failure response to a durable mutation.
        // The lifecycle's crash recovery path reconciles any interrupted staging work on restart.
        tracing::warn!(
            grace_ms = ingress.shutdown_grace_ms,
            "graceful shutdown deadline exceeded; terminating remaining connections"
        );
        connections.abort_all();
        while connections.join_next().await.is_some() {}
    }
    tracing::info!("listener stopped");
    Ok(())
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
            Ok(config) if config.audit_ledger_hmac_key.is_none() => Err(RuntimeError::Application(
                AppError::Validation("AUDIT_LEDGER_HMAC_KEY is required; refusing to start without a tamper-evident audit ledger".to_owned()),
            )),
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
