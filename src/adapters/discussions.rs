//! Discussion settings and per-artifact discussion state for the HTTP routes.
//!
//! This adapter maps store state to response views. The organization setup flows live in
//! [`crate::integrations::discussion_connections`], and the per-artifact override flow (with
//! two-way sync enablement) lives in [`crate::integrations::discussion_two_way`].

use std::sync::Arc;

use crate::{
    error::AppError,
    integrations::{
        discussion_connections::DiscussionConnections, discussion_two_way::DiscussionTwoWay,
    },
    model::{ArtifactMeta, OrgId},
    persistence::{
        discord_inbound::DiscordInboundStore,
        discord_organization::{
            ArtifactDiscussionOverride, DiscordCredentialReadiness, OrganizationDiscordStore,
            OrganizationThreadingStatus,
        },
        discussions::{
            ArtifactDiscussion, DiscussionConnectionStrategy, DiscussionConnectionSummary,
            DiscussionMode, DiscussionState, DiscussionStore,
        },
        webhooks::WebhookStore,
    },
    ports::{
        ArtifactDiscussionOverrideView, ArtifactDiscussionView, BoxFuture,
        DiscussionConnectionView, DiscussionModeRequest, DiscussionOverrideRequest,
        DiscussionService, OrganizationThreadingView,
    },
    security::audit::MutationAudit,
};

#[derive(Clone)]
pub struct ProductionDiscussions {
    store: DiscussionStore,
    organization: OrganizationDiscordStore,
    inbound: DiscordInboundStore,
    webhooks: Arc<WebhookStore>,
    public_base_url: String,
    audit_key: [u8; 32],
}

impl ProductionDiscussions {
    pub fn new(
        store: DiscussionStore,
        organization: OrganizationDiscordStore,
        inbound: DiscordInboundStore,
        webhooks: Arc<WebhookStore>,
        public_base_url: String,
        audit_key: [u8; 32],
    ) -> Self {
        Self {
            store,
            organization,
            inbound,
            webhooks,
            public_base_url,
            audit_key,
        }
    }

    fn connections(&self) -> DiscussionConnections<'_> {
        DiscussionConnections::new(
            &self.store,
            &self.organization,
            &self.webhooks,
            &self.public_base_url,
            self.audit_key,
        )
    }

    fn two_way(&self) -> DiscussionTwoWay<'_> {
        DiscussionTwoWay::new(
            &self.store,
            &self.organization,
            &self.inbound,
            self.audit_key,
        )
    }

    fn connection_view(
        summary: Option<DiscussionConnectionSummary>,
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

    fn organization_view(status: OrganizationThreadingStatus) -> OrganizationThreadingView {
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

    async fn artifact_override_view(
        &self,
        artifact: &ArtifactMeta,
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
            self.connections()
                .save_organization_threading(org.clone(), bot_token, enabled, audit)
                .await?;
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
        Box::pin(async move { self.connections().test_organization_credential(&org).await })
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
            self.connections()
                .queue_historical_recovery(org, audit)
                .await
        })
    }

    fn artifact_override<'a>(
        &'a self,
        artifact: &'a ArtifactMeta,
    ) -> BoxFuture<'a, Result<ArtifactDiscussionOverrideView, AppError>> {
        Box::pin(async move { self.artifact_override_view(artifact).await })
    }

    fn set_artifact_override(
        &self,
        artifact: ArtifactMeta,
        override_mode: DiscussionOverrideRequest,
        _actor: String,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<ArtifactDiscussionOverrideView, AppError>> {
        Box::pin(async move {
            self.two_way()
                .apply(&artifact, override_mode, audit)
                .await?;
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
            let summary = self
                .connections()
                .configure_connection(org, webhook_id, label, audit)
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
        Box::pin(async move { self.connections().test_connection(org, audit).await })
    }

    fn status<'a>(
        &'a self,
        artifact: &'a ArtifactMeta,
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
        artifact: ArtifactMeta,
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
        artifact: ArtifactMeta,
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
