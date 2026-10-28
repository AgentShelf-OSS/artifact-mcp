//! Per-artifact Discord discussion override, including two-way sync enablement.
//!
//! Enabling two-way sync is a guarded sequence. The organization credential and the effective
//! outbound threading policy must be ready, the artifact must have a connected mirror thread,
//! Discord must accept that thread over REST, and the inbound Gateway must report readiness.
//! Only then are the two audited writes made: the outbound override returns to `Inherit`, and
//! the inbound policy is enabled. Disabling writes in the reverse order, so inbound sync stops
//! before the outbound override changes.

use std::time::Duration;

use crate::{
    error::AppError,
    integrations::discord_gateway_runtime::DiscordInboundReadinessRest,
    model::ArtifactMeta,
    persistence::{
        discord_inbound::DiscordInboundStore,
        discord_organization::{ArtifactDiscussionOverride, OrganizationDiscordStore},
        discussions::{DiscussionMode, DiscussionState, DiscussionStore},
    },
    ports::{DiscussionOverrideRequest, discussions::OrganizationDiscordCredentialService as _},
    security::audit::MutationAudit,
};

/// Number of Gateway readiness polls after the first readiness request.
const GATEWAY_READINESS_POLLS: usize = 20;
/// Delay before each Gateway readiness poll.
const GATEWAY_READINESS_INTERVAL: Duration = Duration::from_millis(250);

/// Applies an artifact discussion override request against the discussion, organization, and
/// inbound stores.
pub struct DiscussionTwoWay<'a> {
    discussions: &'a DiscussionStore,
    organization: &'a OrganizationDiscordStore,
    inbound: &'a DiscordInboundStore,
    audit_key: [u8; 32],
}

impl<'a> DiscussionTwoWay<'a> {
    pub const fn new(
        discussions: &'a DiscussionStore,
        organization: &'a OrganizationDiscordStore,
        inbound: &'a DiscordInboundStore,
        audit_key: [u8; 32],
    ) -> Self {
        Self {
            discussions,
            organization,
            inbound,
            audit_key,
        }
    }

    /// Apply `request` to `artifact`.
    ///
    /// # Errors
    ///
    /// Returns `AppError::Conflict` when two-way sync is requested before its preconditions
    /// hold, and propagates store, provider, and audit failures.
    pub async fn apply(
        &self,
        artifact: &ArtifactMeta,
        request: DiscussionOverrideRequest,
        audit: MutationAudit,
    ) -> Result<(), AppError> {
        match request {
            DiscussionOverrideRequest::DiscordTwoWay => self.enable(artifact, audit).await,
            DiscussionOverrideRequest::Inherit => {
                self.disable(artifact, ArtifactDiscussionOverride::Inherit, audit)
                    .await
            }
            DiscussionOverrideRequest::ArtifactOnly => {
                self.disable(artifact, ArtifactDiscussionOverride::ArtifactOnly, audit)
                    .await
            }
        }
    }

    async fn enable(&self, artifact: &ArtifactMeta, audit: MutationAudit) -> Result<(), AppError> {
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
            .discussions
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
        for _ in 0..GATEWAY_READINESS_POLLS {
            if gateway_ready {
                break;
            }
            tokio::time::sleep(GATEWAY_READINESS_INTERVAL).await;
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
        Ok(())
    }

    async fn disable(
        &self,
        artifact: &ArtifactMeta,
        artifact_override: ArtifactDiscussionOverride,
        audit: MutationAudit,
    ) -> Result<(), AppError> {
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
                artifact_override,
                audit,
                self.audit_key,
            )
            .await?;
        Ok(())
    }
}
