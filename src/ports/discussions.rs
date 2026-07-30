//! Safe application boundary for the optional Discord discussion mirror.

use super::BoxFuture;
use crate::{
    config::Secret,
    error::AppError,
    model::{ArtifactMeta, OrgId},
    security::audit::MutationAudit,
};

/// Narrow server-only PBI-081 credential boundary.  Neither route/view contracts nor PBI-080
/// receive SQLite fields or a process-global token; a successful resolve yields a redacted secret
/// wrapper only to provider code.
pub trait OrganizationDiscordCredentialService: Send + Sync {
    fn credential_for_provider<'a>(
        &'a self,
        org: &'a OrgId,
    ) -> BoxFuture<'a, Result<Option<Secret>, AppError>>;
}

/// Exact externally accepted artifact discussion modes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscussionModeRequest {
    ArtifactOnly,
    DiscordMirror,
}

/// PBI-081's only artifact-level exception. `Inherit` deliberately removes any stored local
/// override; it is not a second way to enable two-way sync.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscussionOverrideRequest {
    Inherit,
    ArtifactOnly,
    DiscordTwoWay,
}

/// Write-only organization credential and inherited outbound-threading status. It contains no
/// bot token, Discord snowflake, webhook URL, or provider response detail.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct OrganizationThreadingView {
    pub credential: String,
    pub enabled: bool,
    pub degraded: bool,
    pub recovery_state: String,
    pub recovery_pending: u64,
}

/// Effective artifact discussion status. Provider identifiers remain server-only.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ArtifactDiscussionOverrideView {
    pub override_mode: String,
    pub effective_mode: String,
    pub state: String,
    pub actionable_error: Option<String>,
}

/// Settings-safe connection projection. Credential identity and URL never cross this port.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct DiscussionConnectionView {
    pub configured: bool,
    pub label: String,
    pub destination: String,
    pub strategy: String,
    pub webhook_id: Option<String>,
    pub bot_configured: bool,
    pub last_error: Option<String>,
}

/// Viewer-safe artifact discussion status. Discord thread/message IDs stay server-only.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ArtifactDiscussionView {
    pub mode: String,
    pub state: String,
    pub enabled: bool,
    /// Safe tenant-level availability signal for authorized artifact viewers.
    pub connection_configured: bool,
    pub last_error: Option<String>,
}

/// The dedicated PBI-079 surface. Route authorization happens before this port is called.
pub trait DiscussionService: Send + Sync {
    /// PBI-081 defaults keep the PBI-079 production adapter source-compatible until the
    /// encrypted credential/policy implementation is composed by the application root.
    fn organization_threading<'a>(
        &'a self,
        _org: &'a OrgId,
    ) -> BoxFuture<'a, Result<OrganizationThreadingView, AppError>> {
        unavailable()
    }
    fn save_organization_threading(
        &self,
        _org: OrgId,
        _bot_token: String,
        _enabled: bool,
        _audit: MutationAudit,
    ) -> BoxFuture<'_, Result<OrganizationThreadingView, AppError>> {
        unavailable()
    }
    fn test_organization_credential(
        &self,
        _org: OrgId,
        _audit: MutationAudit,
    ) -> BoxFuture<'_, Result<bool, AppError>> {
        unavailable()
    }
    fn remove_organization_credential(
        &self,
        _org: OrgId,
        _audit: MutationAudit,
    ) -> BoxFuture<'_, Result<bool, AppError>> {
        unavailable()
    }
    fn queue_historical_recovery(
        &self,
        _org: OrgId,
        _audit: MutationAudit,
    ) -> BoxFuture<'_, Result<bool, AppError>> {
        unavailable()
    }
    fn artifact_override<'a>(
        &'a self,
        _artifact: &'a ArtifactMeta,
    ) -> BoxFuture<'a, Result<ArtifactDiscussionOverrideView, AppError>> {
        unavailable()
    }
    fn set_artifact_override(
        &self,
        _artifact: ArtifactMeta,
        _override_mode: DiscussionOverrideRequest,
        _actor: String,
        _audit: MutationAudit,
    ) -> BoxFuture<'_, Result<ArtifactDiscussionOverrideView, AppError>> {
        unavailable()
    }
    fn connection<'a>(
        &'a self,
        org: &'a OrgId,
    ) -> BoxFuture<'a, Result<DiscussionConnectionView, AppError>>;
    fn configure_connection(
        &self,
        org: OrgId,
        webhook_id: String,
        label: String,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<DiscussionConnectionView, AppError>>;
    fn remove_connection(
        &self,
        org: OrgId,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<bool, AppError>>;
    /// Creates a visible notification, starts its public thread, and posts a confirmation inside
    /// it. Credential material remains internal; callers receive only a fixed success/failure
    /// result.
    fn test_connection(
        &self,
        org: OrgId,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<bool, AppError>>;
    fn status<'a>(
        &'a self,
        _artifact: &'a ArtifactMeta,
    ) -> BoxFuture<'a, Result<ArtifactDiscussionView, AppError>>;
    fn set_mode(
        &self,
        artifact: ArtifactMeta,
        mode: DiscussionModeRequest,
        actor: String,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<ArtifactDiscussionView, AppError>>;
    fn retry(
        &self,
        artifact: ArtifactMeta,
        actor: String,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<ArtifactDiscussionView, AppError>>;
}

/// Deliberately inert capability for unrelated route tests.  It still exposes the same route
/// surface, making an accidental missing production binding impossible to hide behind `None`.
#[derive(Debug, Default)]
pub struct InertDiscussionService;

impl DiscussionService for InertDiscussionService {
    fn connection<'a>(
        &'a self,
        _org: &'a OrgId,
    ) -> BoxFuture<'a, Result<DiscussionConnectionView, AppError>> {
        Box::pin(async {
            Ok(DiscussionConnectionView {
                configured: false,
                label: String::new(),
                destination: String::new(),
                strategy: "notification_thread".to_owned(),
                webhook_id: None,
                bot_configured: false,
                last_error: None,
            })
        })
    }

    fn configure_connection(
        &self,
        _org: OrgId,
        _webhook_id: String,
        _label: String,
        _audit: MutationAudit,
    ) -> BoxFuture<'_, Result<DiscussionConnectionView, AppError>> {
        unavailable()
    }

    fn remove_connection(
        &self,
        _org: OrgId,
        _audit: MutationAudit,
    ) -> BoxFuture<'_, Result<bool, AppError>> {
        unavailable()
    }

    fn test_connection(
        &self,
        _org: OrgId,
        _audit: MutationAudit,
    ) -> BoxFuture<'_, Result<bool, AppError>> {
        unavailable()
    }

    fn status<'a>(
        &'a self,
        _artifact: &'a ArtifactMeta,
    ) -> BoxFuture<'a, Result<ArtifactDiscussionView, AppError>> {
        Box::pin(async move {
            Ok(ArtifactDiscussionView {
                mode: "artifact_only".to_owned(),
                state: "local".to_owned(),
                enabled: false,
                connection_configured: false,
                last_error: None,
            })
        })
    }

    fn set_mode(
        &self,
        _artifact: ArtifactMeta,
        _mode: DiscussionModeRequest,
        _actor: String,
        _audit: MutationAudit,
    ) -> BoxFuture<'_, Result<ArtifactDiscussionView, AppError>> {
        unavailable()
    }

    fn retry(
        &self,
        _artifact: ArtifactMeta,
        _actor: String,
        _audit: MutationAudit,
    ) -> BoxFuture<'_, Result<ArtifactDiscussionView, AppError>> {
        unavailable()
    }
}

fn unavailable<T>() -> BoxFuture<'static, Result<T, AppError>> {
    Box::pin(async {
        Err(AppError::Unavailable(
            "discussion service unavailable".to_owned(),
        ))
    })
}
