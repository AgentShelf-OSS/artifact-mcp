//! Organization Discord discussion setup: credential validation, threading policy, the
//! notification-thread destination, historical recovery, and the destination test post.
//!
//! These flows combine the discussion and organization stores with bounded Discord REST calls.
//! Each method keeps a fixed order of checks, provider calls, and audited writes; the HTTP
//! adapter only maps the results to response views.

use crate::{
    config::Secret,
    error::AppError,
    integrations::discord_discussion::{
        DiscordDiscussionTransport, DiscussionOperation, DiscussionResult, discussion_request,
    },
    model::{OrgId, WebhookEvent, WebhookId},
    persistence::{
        discord_organization::{OrganizationDiscordStore, RecoveryDestination},
        discussions::{
            CreateNotificationThreadConnection, DiscussionConnectionStrategy,
            DiscussionConnectionSummary, DiscussionStore,
        },
        webhooks::WebhookStore,
    },
    ports::discussions::OrganizationDiscordCredentialService as _,
    security::audit::MutationAudit,
};

/// Organization-level discussion setup over the discussion, organization, and webhook stores.
pub struct DiscussionConnections<'a> {
    discussions: &'a DiscussionStore,
    organization: &'a OrganizationDiscordStore,
    webhooks: &'a WebhookStore,
    public_base_url: &'a str,
    audit_key: [u8; 32],
}

impl<'a> DiscussionConnections<'a> {
    pub const fn new(
        discussions: &'a DiscussionStore,
        organization: &'a OrganizationDiscordStore,
        webhooks: &'a WebhookStore,
        public_base_url: &'a str,
        audit_key: [u8; 32],
    ) -> Self {
        Self {
            discussions,
            organization,
            webhooks,
            public_base_url,
            audit_key,
        }
    }

    /// Validate a bot credential against Discord and, when a destination is configured, check
    /// that the credential matches the selected notification thread's guild and channel.
    ///
    /// # Errors
    ///
    /// Returns `AppError::Validation` for a missing or mismatched destination and propagates
    /// store and provider failures.
    pub async fn validate_credential(&self, org: &OrgId, token: Secret) -> Result<(), AppError> {
        let transport = DiscordDiscussionTransport::with_bot_token(Some(token))?;
        let Some(summary) = self.discussions.connection_summary(org).await? else {
            return transport.validate_bot_token().await;
        };
        let delivery = self
            .discussions
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

    /// Resolve the notification-thread destination that historical recovery posts into.
    ///
    /// # Errors
    ///
    /// Returns `AppError::Validation` or `AppError::Conflict` when the destination is missing
    /// or incomplete, and propagates store failures.
    pub async fn recovery_destination(&self, org: &OrgId) -> Result<RecoveryDestination, AppError> {
        let summary = self
            .discussions
            .connection_summary(org)
            .await?
            .ok_or_else(|| {
                AppError::Validation(
                    "Configure a notification-thread destination before enabling threading."
                        .to_owned(),
                )
            })?;
        let delivery = self
            .discussions
            .connection_for_delivery(&summary.id, org)
            .await?
            .ok_or(AppError::Internal)?;
        if delivery.strategy != DiscussionConnectionStrategy::NotificationThread {
            return Err(AppError::Validation(
                "Select a notification-thread destination.".to_owned(),
            ));
        }
        Ok(RecoveryDestination {
            connection_id: summary.id,
            notification_webhook_id: delivery.notification_webhook_id.ok_or(AppError::Internal)?,
            provider_webhook_id: delivery.notification_provider_webhook_id.ok_or_else(|| {
                AppError::Conflict(
                    "Re-save the selected Discord destination before enabling threading."
                        .to_owned(),
                )
            })?,
            guild_id: delivery.guild_id.ok_or(AppError::Internal)?,
            channel_id: delivery.channel_id.ok_or(AppError::Internal)?,
        })
    }

    /// Save the organization credential and outbound threading policy, then queue historical
    /// recovery when threading is enabled.
    ///
    /// # Errors
    ///
    /// Returns validation errors for a missing destination or credential, and propagates
    /// provider, store, and audit failures.
    pub async fn save_organization_threading(
        &self,
        org: OrgId,
        bot_token: String,
        enabled: bool,
        audit: MutationAudit,
    ) -> Result<(), AppError> {
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
            let token = Secret::new(bot_token);
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
                    self.public_base_url.to_owned(),
                    audit,
                    self.audit_key,
                )
                .await?;
        }
        Ok(())
    }

    /// Validate the stored organization credential against Discord.
    ///
    /// # Errors
    ///
    /// Returns `AppError::Validation` when no credential is configured and propagates provider
    /// and store failures.
    pub async fn test_organization_credential(&self, org: &OrgId) -> Result<bool, AppError> {
        let token = self
            .organization
            .credential_for_provider(org)
            .await?
            .ok_or_else(|| {
                AppError::Validation("No organization Discord credential is configured.".to_owned())
            })?;
        self.validate_credential(org, token).await?;
        Ok(true)
    }

    /// Queue historical recovery for every eligible artifact in the organization.
    ///
    /// # Errors
    ///
    /// Propagates destination, store, and audit failures.
    pub async fn queue_historical_recovery(
        &self,
        org: OrgId,
        audit: MutationAudit,
    ) -> Result<bool, AppError> {
        let destination = self.recovery_destination(&org).await?;
        let queued = self
            .organization
            .queue_recoveries_for_org_audited(
                org,
                destination,
                self.public_base_url.to_owned(),
                audit,
                self.audit_key,
            )
            .await?;
        Ok(queued > 0)
    }

    /// Select an organization webhook as the notification-thread destination.
    ///
    /// # Errors
    ///
    /// Returns `AppError::Validation` for a missing credential or an unsuitable webhook, and
    /// propagates provider, store, and audit failures.
    pub async fn configure_connection(
        &self,
        org: OrgId,
        webhook_id: String,
        label: String,
        audit: MutationAudit,
    ) -> Result<DiscussionConnectionSummary, AppError> {
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
                AppError::Validation("Select an existing webhook for this organization.".to_owned())
            })?;
        if !delivery.events.contains(&WebhookEvent::Published) {
            return Err(AppError::Validation(
                "The selected webhook must subscribe to published artifacts.".to_owned(),
            ));
        }
        let destination = transport
            .inspect_notification_webhook(&delivery.url)
            .await?;
        self.discussions
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
            .await
    }

    /// Post a visible test message to the configured destination, with requested and
    /// completed audit markers around the provider call.
    ///
    /// # Errors
    ///
    /// Returns `AppError::Validation` when no connection is configured,
    /// `AppError::Unavailable` when the test post fails, and propagates audit failures.
    pub async fn test_connection(
        &self,
        org: OrgId,
        audit: MutationAudit,
    ) -> Result<bool, AppError> {
        let transport = DiscordDiscussionTransport::with_bot_token(
            self.organization.credential_for_provider(&org).await?,
        )?;
        let Some(summary) = self.discussions.connection_summary(&org).await? else {
            return Err(AppError::Validation(
                "Discord discussion connection is not configured.".to_owned(),
            ));
        };
        // Commit requested evidence before making the external call. The URL is resolved only
        // below this boundary and never enters an audit event, response, error, or debug log.
        self.discussions
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
        let success = match self
            .discussions
            .connection_for_delivery(&summary.id, &org)
            .await
        {
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
                let operation = DiscussionOperation::create_thread(
                    "Artifact MCP connection test".to_owned(),
                    "Artifact MCP Discord discussion connection test. This visible post confirms delivery and is not linked to an artifact.".to_owned(),
                );
                match operation.and_then(|operation| {
                    discussion_request(
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
        self.discussions
            .audit_connection_test(org, summary.id, Some(success), audit, self.audit_key)
            .await?;
        if success {
            Ok(true)
        } else {
            Err(AppError::Unavailable(
                "discord discussion unavailable".to_owned(),
            ))
        }
    }
}
