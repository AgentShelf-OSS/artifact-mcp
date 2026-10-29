use std::sync::Arc;

use crate::{
    config::Secret,
    model::{ArtifactId, OrgId},
    persistence::db::DbPool,
    security::crypto::WebhookUrlProtection,
};

/// Browser- and audit-safe credential readiness.  It deliberately has no identifier or token
/// prefix: configured is all a caller needs to render a setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscordCredentialReadiness {
    Unconfigured,
    Configured,
    Deactivated,
    LegacyFallback,
}

/// A safe organization policy projection.  `effective_outbound` is false unless the policy,
/// destination, and a credential source are all ready.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EffectiveDiscussionPolicy {
    pub outbound_enabled: bool,
    pub artifact_override: ArtifactDiscussionOverride,
    pub effective_outbound: bool,
    pub credential_readiness: DiscordCredentialReadiness,
}

/// Fixed-cardinality settings projection. It contains no credential material or provider IDs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OrganizationThreadingStatus {
    pub outbound_enabled: bool,
    pub credential_readiness: DiscordCredentialReadiness,
    pub recovery_pending: u64,
    pub recovery_state: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArtifactDiscussionOverride {
    Inherit,
    ArtifactOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryState {
    Pending,
    Recovering,
    Recovered,
    NotFound,
    Ambiguous,
    PermissionDenied,
    RateLimited,
    Retryable,
    Invalid,
}

impl RecoveryState {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Recovering => "recovering",
            Self::Recovered => "recovered",
            Self::NotFound => "not_found",
            Self::Ambiguous => "ambiguous",
            Self::PermissionDenied => "permission_denied",
            Self::RateLimited => "rate_limited",
            Self::Retryable => "retryable",
            Self::Invalid => "invalid",
        }
    }
}

/// Destination dimensions established by the selected PBI-079 connection.  No browser-supplied
/// guild/channel/webhook is trusted; callers obtain these from the server-side connection row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryDestination {
    pub connection_id: String,
    pub notification_webhook_id: String,
    pub provider_webhook_id: String,
    pub guild_id: String,
    pub channel_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryRecord {
    pub state: RecoveryState,
    pub recovered_message_id: Option<String>,
    pub attempts: u64,
    pub last_error: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryJob {
    pub artifact_id: ArtifactId,
    pub org: OrgId,
    pub destination: RecoveryDestination,
    pub canonical_artifact_url: String,
}

/// SQLite implementation of the credential/policy ports shared by outbound delivery and PBI-080.
#[derive(Clone)]
pub struct OrganizationDiscordStore {
    pub(super) pool: DbPool,
    pub(super) protection: Arc<WebhookUrlProtection>,
    // This is supplied by composition from the existing deployment configuration.  This module
    // never reads process environment, preventing accidental inspection/logging of a fallback.
    pub(super) legacy_fallback: Option<Secret>,
}

impl std::fmt::Debug for OrganizationDiscordStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OrganizationDiscordStore")
            .field("protection", &self.protection)
            .field("legacy_fallback", &self.legacy_fallback.is_some())
            .finish_non_exhaustive()
    }
}
