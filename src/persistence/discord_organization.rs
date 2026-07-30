//! PBI-081 organization-scoped Discord credentials, inherited policy, and recovery evidence.
//!
//! This store deliberately has no HTTP or Discord dependency.  A route/provider validates a
//! proposed token *before* calling [`OrganizationDiscordStore::save_validated_credential`], so a
//! failed external validation cannot replace a working credential while a SQLite transaction is
//! open.  The only secret-bearing return type is the server-side [`Secret`] used by providers.

use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::{
    config::Secret,
    error::AppError,
    integrations::delivery_envelope::stable_delivery_event_id,
    model::{ArtifactId, OrgId, WebhookEvent},
    persistence::{
        db::{self, DbPool},
        migrations::EncryptedUrl,
    },
    ports::discussions::OrganizationDiscordCredentialService,
    security::{
        audit::{AuditEvent, MutationAudit, mutate_in_transaction},
        crypto::WebhookUrlProtection,
    },
};

const ENCRYPTION_REQUIRED: &str =
    "A server encryption key is required before storing Discord bot credentials.";
const INVALID_CREDENTIAL: &str = "Discord bot credential validation failed.";
const INVALID_RECOVERY_DESTINATION: &str =
    "Discord recovery destination does not match the organization connection.";

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
    fn as_str(self) -> &'static str {
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
    pool: DbPool,
    protection: Arc<WebhookUrlProtection>,
    // This is supplied by composition from the existing deployment configuration.  This module
    // never reads process environment, preventing accidental inspection/logging of a fallback.
    legacy_fallback: Option<Secret>,
}

impl OrganizationDiscordStore {
    #[must_use]
    pub const fn new(
        pool: DbPool,
        protection: Arc<WebhookUrlProtection>,
        legacy_fallback: Option<Secret>,
    ) -> Self {
        Self {
            pool,
            protection,
            legacy_fallback,
        }
    }

    /// Saves a token only after the caller's provider validation has succeeded.  Encryption occurs
    /// before the immediate transaction; persistence is a one-statement upsert, so a database
    /// failure cannot replace an existing ciphertext.
    pub async fn save_validated_credential(
        &self,
        org: &OrgId,
        token: Secret,
        validated: bool,
    ) -> Result<DiscordCredentialReadiness, AppError> {
        if !validated {
            return Err(AppError::Validation(INVALID_CREDENTIAL.to_owned()));
        }
        let Some(cipher) = self.protection.cipher() else {
            return Err(AppError::Unavailable(ENCRYPTION_REQUIRED.to_owned()));
        };
        let encrypted = cipher.encrypt(token.expose())?;
        let org = trimmed(&org.0);
        db::interact(&self.pool, move |conn| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(internal)?;
            if !org_exists(&tx, &org)? {
                return Err(AppError::Validation(format!("Unknown organization \"{org}\".")));
            }
            tx.execute(
                "INSERT INTO org_discord_bot_credentials \
                 (org, ciphertext, nonce, tag, version, active, deactivated_at) \
                 VALUES (?1, ?2, ?3, ?4, 1, 1, NULL) \
                 ON CONFLICT(org) DO UPDATE SET ciphertext=excluded.ciphertext, nonce=excluded.nonce, \
                   tag=excluded.tag, version=org_discord_bot_credentials.version+1, active=1, \
                   deactivated_at=NULL, updated_at=datetime('now')",
                params![org, encrypted.ciphertext, encrypted.nonce, encrypted.tag],
            )
            .map_err(internal)?;
            tx.commit().map_err(internal)?;
            Ok(DiscordCredentialReadiness::Configured)
        })
        .await
    }

    /// Deactivation preserves discussion and recovery history while denying future token resolves.
    pub async fn deactivate_credential(&self, org: &OrgId) -> Result<bool, AppError> {
        let org = trimmed(&org.0);
        db::interact(&self.pool, move |conn| {
            conn.execute(
                "UPDATE org_discord_bot_credentials SET active=0, deactivated_at=datetime('now'), \
                 updated_at=datetime('now') WHERE org=?1 AND active=1",
                [org],
            )
            .map(|count| count == 1)
            .map_err(internal)
        })
        .await
    }

    pub async fn credential_readiness(
        &self,
        org: &OrgId,
    ) -> Result<DiscordCredentialReadiness, AppError> {
        let org = trimmed(&org.0);
        let fallback = self.legacy_fallback.is_some();
        db::interact(&self.pool, move |conn| {
            match credential_active(conn, &org)? {
                Some(true) => Ok(DiscordCredentialReadiness::Configured),
                Some(false) => Ok(DiscordCredentialReadiness::Deactivated),
                None if fallback && has_legacy_discussion_connection(conn, &org)? => {
                    Ok(DiscordCredentialReadiness::LegacyFallback)
                }
                None => Ok(DiscordCredentialReadiness::Unconfigured),
            }
        })
        .await
    }

    pub async fn set_outbound_enabled(&self, org: &OrgId, enabled: bool) -> Result<(), AppError> {
        let org = trimmed(&org.0);
        db::interact(&self.pool, move |conn| {
            if !org_exists(conn, &org)? {
                return Err(AppError::Validation(format!("Unknown organization \"{org}\".")));
            }
            conn.execute(
                "INSERT INTO org_discord_threading_policies (org, outbound_enabled, updated_at) VALUES (?1, ?2, datetime('now')) \
                 ON CONFLICT(org) DO UPDATE SET outbound_enabled=excluded.outbound_enabled, updated_at=datetime('now')",
                params![org, i64::from(enabled)],
            )
            .map_err(internal)?;
            Ok(())
        })
        .await
    }

    /// Atomically rotates an already provider-validated credential, updates the inherited
    /// policy, and appends a secret-free audit event. An empty rotation keeps the active
    /// credential (or explicitly adopted deployment fallback) unchanged.
    pub async fn save_validated_credential_and_policy_audited(
        &self,
        org: OrgId,
        token: Option<Secret>,
        enabled: bool,
        audit: MutationAudit,
        audit_key: [u8; 32],
    ) -> Result<OrganizationThreadingStatus, AppError> {
        let encrypted = match token.as_ref() {
            Some(token) => {
                let Some(cipher) = self.protection.cipher() else {
                    return Err(AppError::Unavailable(ENCRYPTION_REQUIRED.to_owned()));
                };
                Some(cipher.encrypt(token.expose())?)
            }
            None => None,
        };
        let fallback_available = self.legacy_fallback.is_some();
        let org_name = trimmed(&org.0);
        db::interact(&self.pool, move |conn| {
            let audit = audit.for_target_tenant(&org_name)?;
            mutate_in_transaction(conn, &audit_key, &audit, |tx| {
                if !org_exists(tx, &org_name)? {
                    return Err(AppError::Validation(format!(
                        "Unknown organization \"{org_name}\"."
                    )));
                }
                let existing = credential_active(tx, &org_name)?;
                let legacy_fallback_adoptable =
                    fallback_available && has_legacy_discussion_connection(tx, &org_name)?;
                if enabled
                    && encrypted.is_none()
                    && !matches!(existing, Some(true))
                    && !(existing.is_none() && legacy_fallback_adoptable)
                {
                    return Err(AppError::Validation(
                        "A validated organization Discord credential is required.".to_owned(),
                    ));
                }
                if let Some(encrypted) = &encrypted {
                    tx.execute(
                        "INSERT INTO org_discord_bot_credentials \
                         (org, ciphertext, nonce, tag, version, active, deactivated_at) \
                         VALUES (?1, ?2, ?3, ?4, 1, 1, NULL) \
                         ON CONFLICT(org) DO UPDATE SET ciphertext=excluded.ciphertext, \
                           nonce=excluded.nonce, tag=excluded.tag, \
                           version=org_discord_bot_credentials.version+1, active=1, \
                           deactivated_at=NULL, updated_at=datetime('now')",
                        params![
                            org_name,
                            encrypted.ciphertext,
                            encrypted.nonce,
                            encrypted.tag
                        ],
                    )
                    .map_err(internal)?;
                }
                tx.execute(
                    "INSERT INTO org_discord_threading_policies \
                     (org, outbound_enabled, updated_at) VALUES (?1, ?2, datetime('now')) \
                     ON CONFLICT(org) DO UPDATE SET outbound_enabled=excluded.outbound_enabled, \
                       updated_at=datetime('now')",
                    params![org_name, i64::from(enabled)],
                )
                .map_err(internal)?;
                let readiness = readiness_in_transaction(tx, &org_name, fallback_available)?;
                let status = status_in_transaction(tx, &org_name, readiness)?;
                let classification = if encrypted.is_some() {
                    "discord_credential_rotated"
                } else {
                    "discord_threading_policy_changed"
                };
                Ok((
                    status,
                    AuditEvent {
                        operation: "discord.organization-threading.save".to_owned(),
                        target_type: "organization".to_owned(),
                        target_id: org_name.clone(),
                        result: "success".to_owned(),
                        classification: classification.to_owned(),
                        revision: None,
                    },
                ))
            })
        })
        .await
    }

    pub async fn organization_status(
        &self,
        org: &OrgId,
    ) -> Result<OrganizationThreadingStatus, AppError> {
        let org = trimmed(&org.0);
        let fallback = self.legacy_fallback.is_some();
        db::interact(&self.pool, move |conn| {
            if !org_exists(conn, &org)? {
                return Err(AppError::NotFound("Organization not found.".to_owned()));
            }
            let readiness = readiness_in_transaction(conn, &org, fallback)?;
            status_in_transaction(conn, &org, readiness)
        })
        .await
    }

    pub async fn deactivate_credential_audited(
        &self,
        org: OrgId,
        audit: MutationAudit,
        audit_key: [u8; 32],
    ) -> Result<bool, AppError> {
        let org_name = trimmed(&org.0);
        db::interact(&self.pool, move |conn| {
            let audit = audit.for_target_tenant(&org_name)?;
            mutate_in_transaction(conn, &audit_key, &audit, |tx| {
                if !org_exists(tx, &org_name)? {
                    return Err(AppError::NotFound("Organization not found.".to_owned()));
                }
                let changed = tx
                    .execute(
                        "UPDATE org_discord_bot_credentials SET active=0, \
                         deactivated_at=datetime('now'), updated_at=datetime('now') \
                         WHERE org=?1 AND active=1",
                        [&org_name],
                    )
                    .map_err(internal)?
                    == 1;
                tx.execute(
                    "INSERT INTO org_discord_threading_policies \
                     (org, outbound_enabled, updated_at) VALUES (?1, 0, datetime('now')) \
                     ON CONFLICT(org) DO UPDATE SET outbound_enabled=0, \
                       updated_at=datetime('now')",
                    [&org_name],
                )
                .map_err(internal)?;
                Ok((
                    changed,
                    AuditEvent {
                        operation: "discord.organization-credential.remove".to_owned(),
                        target_type: "organization".to_owned(),
                        target_id: org_name.clone(),
                        result: "success".to_owned(),
                        classification: "discord_credential_deactivated".to_owned(),
                        revision: None,
                    },
                ))
            })
        })
        .await
    }

    pub async fn set_artifact_override_audited(
        &self,
        artifact: ArtifactId,
        org: OrgId,
        override_mode: ArtifactDiscussionOverride,
        audit: MutationAudit,
        audit_key: [u8; 32],
    ) -> Result<(), AppError> {
        let artifact_id = trimmed(&artifact.0);
        let org_name = trimmed(&org.0);
        db::interact(&self.pool, move |conn| {
            let audit = audit.for_target_tenant(&org_name)?;
            mutate_in_transaction(conn, &audit_key, &audit, |tx| {
                if !artifact_exists(tx, &artifact_id, &org_name)? {
                    return Err(AppError::NotFound("Artifact not found.".to_owned()));
                }
                match override_mode {
                    ArtifactDiscussionOverride::Inherit => {
                        tx.execute(
                            "DELETE FROM artifact_discussion_overrides \
                             WHERE artifact_id=?1 AND org=?2",
                            params![artifact_id, org_name],
                        )
                        .map_err(internal)?;
                    }
                    ArtifactDiscussionOverride::ArtifactOnly => {
                        tx.execute(
                            "INSERT INTO artifact_discussion_overrides \
                             (artifact_id, org, mode, updated_at) \
                             VALUES (?1, ?2, 'artifact_only', datetime('now')) \
                             ON CONFLICT(artifact_id, org) DO UPDATE SET \
                               updated_at=datetime('now')",
                            params![artifact_id, org_name],
                        )
                        .map_err(internal)?;
                        tx.execute(
                            "UPDATE artifact_discussions SET mode='artifact_only', state='paused', \
                             disabled_at=datetime('now'), updated_at=datetime('now') \
                             WHERE artifact_id=?1 AND org=?2",
                            params![artifact_id, org_name],
                        )
                        .map_err(internal)?;
                    }
                }
                Ok((
                    (),
                    AuditEvent {
                        operation: "discord.artifact-discussion.override".to_owned(),
                        target_type: "artifact".to_owned(),
                        target_id: artifact_id.clone(),
                        result: "success".to_owned(),
                        classification: match override_mode {
                            ArtifactDiscussionOverride::Inherit => "discussion_override_inherit",
                            ArtifactDiscussionOverride::ArtifactOnly => {
                                "discussion_override_artifact_only"
                            }
                        }
                        .to_owned(),
                        revision: None,
                    },
                ))
            })
        })
        .await
    }

    /// `ArtifactOnly` is the only persisted exception.  Resetting inherit deletes the exception,
    /// so policy toggles cannot erase a deliberate owner choice.
    pub async fn set_artifact_override(
        &self,
        artifact: &ArtifactId,
        org: &OrgId,
        override_mode: ArtifactDiscussionOverride,
    ) -> Result<(), AppError> {
        let artifact = trimmed(&artifact.0);
        let org = trimmed(&org.0);
        db::interact(&self.pool, move |conn| {
            if !artifact_exists(conn, &artifact, &org)? {
                return Err(AppError::NotFound("Artifact not found.".to_owned()));
            }
            match override_mode {
                ArtifactDiscussionOverride::Inherit => {
                    conn.execute(
                        "DELETE FROM artifact_discussion_overrides WHERE artifact_id=?1 AND org=?2",
                        params![artifact, org],
                    )
                    .map_err(internal)?;
                }
                ArtifactDiscussionOverride::ArtifactOnly => {
                    conn.execute(
                        "INSERT INTO artifact_discussion_overrides (artifact_id, org, mode, updated_at) \
                         VALUES (?1, ?2, 'artifact_only', datetime('now')) \
                         ON CONFLICT(artifact_id, org) DO UPDATE SET updated_at=datetime('now')",
                        params![artifact, org],
                    )
                    .map_err(internal)?;
                }
            }
            Ok(())
        })
        .await
    }

    pub async fn effective_policy(
        &self,
        artifact: &ArtifactId,
        org: &OrgId,
    ) -> Result<EffectiveDiscussionPolicy, AppError> {
        let artifact = trimmed(&artifact.0);
        let org = trimmed(&org.0);
        let fallback = self.legacy_fallback.is_some();
        db::interact(&self.pool, move |conn| {
            if !artifact_exists(conn, &artifact, &org)? {
                return Err(AppError::NotFound("Artifact not found.".to_owned()));
            }
            let override_mode = match conn
                .query_row(
                    "SELECT mode FROM artifact_discussion_overrides WHERE artifact_id=?1 AND org=?2",
                    params![artifact, org],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(internal)?
                .as_deref()
            {
                Some("artifact_only") => ArtifactDiscussionOverride::ArtifactOnly,
                _ => ArtifactDiscussionOverride::Inherit,
            };
            let outbound_enabled = conn
                .query_row(
                    "SELECT outbound_enabled FROM org_discord_threading_policies WHERE org=?1",
                    [&org],
                    |row| row.get::<_, i64>(0),
                )
                .optional()
                .map_err(internal)?
                .unwrap_or(0)
                != 0;
            let readiness = match credential_active(conn, &org)? {
                Some(true) => DiscordCredentialReadiness::Configured,
                Some(false) => DiscordCredentialReadiness::Deactivated,
                None if fallback && has_legacy_discussion_connection(conn, &org)? => {
                    DiscordCredentialReadiness::LegacyFallback
                }
                None => DiscordCredentialReadiness::Unconfigured,
            };
            let destination_ready: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM org_discord_discussion_connections \
                     WHERE org=?1 AND strategy='notification_thread' \
                       AND notification_webhook_id IS NOT NULL AND guild_id <> '' AND channel_id <> '')",
                    [&org],
                    |row| row.get(0),
                )
                .map_err(internal)?;
            Ok(EffectiveDiscussionPolicy {
                outbound_enabled,
                artifact_override: override_mode,
                effective_outbound: outbound_enabled
                    && override_mode == ArtifactDiscussionOverride::Inherit
                    && destination_ready
                    && matches!(readiness, DiscordCredentialReadiness::Configured | DiscordCredentialReadiness::LegacyFallback),
                credential_readiness: readiness,
            })
        })
        .await
    }

    /// Enqueues only local evidence for a bounded recovery worker.  Provider scans happen later;
    /// no Discord response or body can enter this transaction.
    pub async fn schedule_recovery(
        &self,
        artifact: &ArtifactId,
        org: &OrgId,
        destination: RecoveryDestination,
        canonical_artifact_url: String,
    ) -> Result<(), AppError> {
        if !is_canonical_url(&canonical_artifact_url) {
            return Err(AppError::Validation(
                "A canonical artifact URL is required for Discord recovery.".to_owned(),
            ));
        }
        let artifact = trimmed(&artifact.0);
        let org = trimmed(&org.0);
        db::interact(&self.pool, move |conn| {
            if !artifact_exists(conn, &artifact, &org)? || !destination_matches(conn, &org, &destination)? {
                return Err(AppError::Validation(INVALID_RECOVERY_DESTINATION.to_owned()));
            }
            conn.execute(
                "INSERT INTO discord_notification_anchor_recoveries \
                 (artifact_id, org, connection_id, notification_webhook_id, provider_webhook_id, guild_id, channel_id, canonical_artifact_url, state) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending') \
                 ON CONFLICT(artifact_id, org) DO UPDATE SET connection_id=excluded.connection_id, \
                   notification_webhook_id=excluded.notification_webhook_id, provider_webhook_id=excluded.provider_webhook_id, guild_id=excluded.guild_id, \
                   channel_id=excluded.channel_id, canonical_artifact_url=excluded.canonical_artifact_url, \
                   state='pending', recovered_message_id=NULL, provenance='', last_error='', completed_at=NULL, updated_at=datetime('now')",
                params![artifact, org, destination.connection_id, destination.notification_webhook_id, destination.provider_webhook_id, destination.guild_id, destination.channel_id, canonical_artifact_url],
            ).map_err(internal)?;
            Ok(())
        }).await
    }

    /// Queue eligible historical artifacts without touching artifacts that already have an exact
    /// retained publication receipt or recovered provenance.
    pub async fn queue_recoveries_for_org_audited(
        &self,
        org: OrgId,
        destination: RecoveryDestination,
        public_base_url: String,
        audit: MutationAudit,
        audit_key: [u8; 32],
    ) -> Result<u64, AppError> {
        let org_name = trimmed(&org.0);
        let base = public_base_url.trim_end_matches('/').to_owned();
        if !destination_matches_preflight(&destination) {
            return Err(AppError::Validation(
                INVALID_RECOVERY_DESTINATION.to_owned(),
            ));
        }
        db::interact(&self.pool, move |conn| {
            let audit = audit.for_target_tenant(&org_name)?;
            mutate_in_transaction(conn, &audit_key, &audit, |tx| {
                if !destination_matches(tx, &org_name, &destination)? {
                    return Err(AppError::Validation(INVALID_RECOVERY_DESTINATION.to_owned()));
                }
                let mut artifact_ids = Vec::new();
                {
                    let mut statement = tx
                        .prepare(
                            "SELECT a.id FROM artifacts a \
                             WHERE a.org=?1 AND NOT EXISTS (\
                               SELECT 1 FROM artifact_discussion_overrides x \
                                WHERE x.artifact_id=a.id AND x.org=a.org\
                             ) ORDER BY a.id",
                        )
                        .map_err(internal)?;
                    let rows = statement
                        .query_map([&org_name], |row| row.get::<_, String>(0))
                        .map_err(internal)?;
                    for row in rows {
                        artifact_ids.push(row.map_err(internal)?);
                    }
                }
                let mut queued = 0_u64;
                for artifact_id in artifact_ids {
                    let subject = format!("artifact:{artifact_id}:1");
                    let event_id = stable_delivery_event_id(
                        &OrgId(org_name.clone()),
                        &WebhookEvent::Published,
                        &subject,
                    );
                    let retained: bool = tx
                        .query_row(
                            "SELECT EXISTS(SELECT 1 FROM provider_delivery_outbox \
                             WHERE provider='discord' AND tenant=?1 AND target_key=?2 \
                               AND event_id=?3 AND event_type='published' \
                               AND delivery_kind='event' AND state='accepted' \
                               AND discord_message_id IS NOT NULL)",
                            params![org_name, destination.notification_webhook_id, event_id],
                            |row| row.get(0),
                        )
                        .map_err(internal)?;
                    if retained {
                        continue;
                    }
                    let canonical_url = format!("{base}/{artifact_id}");
                    if !is_canonical_url(&canonical_url) {
                        return Err(AppError::Validation(
                            "A canonical artifact URL is required for Discord recovery.".to_owned(),
                        ));
                    }
                    queued += tx
                        .execute(
                            "INSERT INTO discord_notification_anchor_recoveries \
                             (artifact_id, org, connection_id, notification_webhook_id, \
                              provider_webhook_id, guild_id, channel_id, canonical_artifact_url, state) \
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending') \
                             ON CONFLICT(artifact_id, org) DO UPDATE SET \
                               connection_id=excluded.connection_id, \
                               notification_webhook_id=excluded.notification_webhook_id, \
                               provider_webhook_id=excluded.provider_webhook_id, \
                               guild_id=excluded.guild_id, channel_id=excluded.channel_id, \
                               canonical_artifact_url=excluded.canonical_artifact_url, \
                               state='pending', recovered_message_id=NULL, provenance='', \
                               last_error='', completed_at=NULL, updated_at=datetime('now') \
                             WHERE discord_notification_anchor_recoveries.state <> 'recovered'",
                            params![
                                artifact_id,
                                org_name,
                                destination.connection_id,
                                destination.notification_webhook_id,
                                destination.provider_webhook_id,
                                destination.guild_id,
                                destination.channel_id,
                                canonical_url
                            ],
                        )
                        .map_err(internal)? as u64;
                }
                Ok((
                    queued,
                    AuditEvent {
                        operation: "discord.history-recovery.queue".to_owned(),
                        target_type: "organization".to_owned(),
                        target_id: org_name.clone(),
                        result: "success".to_owned(),
                        classification: "discord_recovery_queued".to_owned(),
                        revision: None,
                    },
                ))
            })
        })
        .await
    }

    pub async fn claim_recovery(&self) -> Result<Option<RecoveryJob>, AppError> {
        db::interact(&self.pool, move |conn| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(internal)?;
            let job = tx
                .query_row(
                    "SELECT artifact_id, org, connection_id, notification_webhook_id, \
                            provider_webhook_id, guild_id, channel_id, canonical_artifact_url \
                     FROM discord_notification_anchor_recoveries \
                     WHERE attempts < 5 AND (state='pending' \
                        OR (state='recovering' AND updated_at <= datetime('now','-60 seconds')) \
                        OR (state='retryable' AND updated_at <= datetime('now','-5 seconds')) \
                        OR (state='rate_limited' AND updated_at <= datetime('now','-30 seconds'))) \
                     ORDER BY updated_at, org, artifact_id LIMIT 1",
                    [],
                    |row| {
                        Ok(RecoveryJob {
                            artifact_id: ArtifactId(row.get(0)?),
                            org: OrgId(row.get(1)?),
                            destination: RecoveryDestination {
                                connection_id: row.get(2)?,
                                notification_webhook_id: row.get(3)?,
                                provider_webhook_id: row.get(4)?,
                                guild_id: row.get(5)?,
                                channel_id: row.get(6)?,
                            },
                            canonical_artifact_url: row.get(7)?,
                        })
                    },
                )
                .optional()
                .map_err(internal)?;
            if let Some(job) = &job {
                let changed = tx
                    .execute(
                        "UPDATE discord_notification_anchor_recoveries \
                         SET state='recovering', attempts=attempts+1, updated_at=datetime('now') \
                         WHERE artifact_id=?1 AND org=?2 \
                           AND state IN ('pending','recovering','retryable','rate_limited')",
                        params![job.artifact_id.0, job.org.0],
                    )
                    .map_err(internal)?;
                if changed != 1 {
                    return Err(AppError::Internal);
                }
            }
            tx.commit().map_err(internal)?;
            Ok(job)
        })
        .await
    }

    /// Records a provider outcome only after the recovery worker has applied exact selected
    /// webhook + canonical URL matching. `message_id` is accepted only for `Recovered`.
    pub async fn complete_recovery(
        &self,
        artifact: &ArtifactId,
        org: &OrgId,
        state: RecoveryState,
        message_id: Option<String>,
    ) -> Result<RecoveryRecord, AppError> {
        let artifact = trimmed(&artifact.0);
        let org = trimmed(&org.0);
        let message_id = message_id.map(|value| trimmed(&value));
        if (state == RecoveryState::Recovered)
            != message_id
                .as_ref()
                .is_some_and(|value| valid_discord_id(value))
        {
            return Err(AppError::Validation(
                "Invalid exact Discord recovery result.".to_owned(),
            ));
        }
        db::interact(&self.pool, move |conn| {
            let changed = conn.execute(
                "UPDATE discord_notification_anchor_recoveries SET state=?3, recovered_message_id=?4, \
                 provenance=CASE WHEN ?3='recovered' THEN 'exact_selected_webhook_canonical_url' ELSE '' END, \
                 last_error=CASE WHEN ?3 IN ('not_found','ambiguous','permission_denied','rate_limited','retryable','invalid') THEN ?3 ELSE '' END, \
                 completed_at=CASE WHEN ?3 IN ('pending','recovering','retryable','rate_limited') THEN NULL ELSE datetime('now') END, updated_at=datetime('now') \
                 WHERE artifact_id=?1 AND org=?2",
                params![artifact, org, state.as_str(), message_id],
            ).map_err(internal)?;
            if changed != 1 { return Err(AppError::NotFound("Discord recovery was not scheduled.".to_owned())); }
            recovery_record(conn, &artifact, &org)?.ok_or(AppError::Internal)
        }).await
    }

    /// Worker-only terminal/retry transition with a secret-free, fixed-classification audit
    /// event in the same immediate transaction. If audit persistence fails, the recovering row
    /// remains reclaimable instead of silently losing outcome evidence.
    pub async fn complete_recovery_audited(
        &self,
        artifact: ArtifactId,
        org: OrgId,
        state: RecoveryState,
        message_id: Option<String>,
        audit: MutationAudit,
        audit_key: [u8; 32],
    ) -> Result<RecoveryRecord, AppError> {
        let artifact_name = trimmed(&artifact.0);
        let org_name = trimmed(&org.0);
        let message_id = message_id.map(|value| trimmed(&value));
        if (state == RecoveryState::Recovered)
            != message_id
                .as_ref()
                .is_some_and(|value| valid_discord_id(value))
        {
            return Err(AppError::Validation(
                "Invalid exact Discord recovery result.".to_owned(),
            ));
        }
        db::interact(&self.pool, move |conn| {
            let audit = audit.for_affected_tenant(&OrgId(org_name.clone()));
            mutate_in_transaction(conn, &audit_key, &audit, |tx| {
                let changed = tx
                    .execute(
                        "UPDATE discord_notification_anchor_recoveries SET state=?3, recovered_message_id=?4, \
                         provenance=CASE WHEN ?3='recovered' THEN 'exact_selected_webhook_canonical_url' ELSE '' END, \
                         last_error=CASE WHEN ?3 IN ('not_found','ambiguous','permission_denied','rate_limited','retryable','invalid') THEN ?3 ELSE '' END, \
                         completed_at=CASE WHEN ?3 IN ('pending','recovering','retryable','rate_limited') THEN NULL ELSE datetime('now') END, \
                         updated_at=datetime('now') WHERE artifact_id=?1 AND org=?2 AND state='recovering'",
                        params![artifact_name, org_name, state.as_str(), message_id],
                    )
                    .map_err(internal)?;
                if changed != 1 {
                    return Err(AppError::Conflict(
                        "Discord recovery lease is no longer current.".to_owned(),
                    ));
                }
                let record =
                    recovery_record(tx, &artifact_name, &org_name)?.ok_or(AppError::Internal)?;
                Ok((
                    record,
                    AuditEvent {
                        operation: "discord.history-recovery.complete".to_owned(),
                        target_type: "artifact".to_owned(),
                        target_id: artifact_name.clone(),
                        result: "success".to_owned(),
                        classification: format!("discord_recovery_{}", state.as_str()),
                        revision: None,
                    },
                ))
            })
        })
        .await
    }

    pub async fn recovery_record(
        &self,
        artifact: &ArtifactId,
        org: &OrgId,
    ) -> Result<Option<RecoveryRecord>, AppError> {
        let artifact = trimmed(&artifact.0);
        let org = trimmed(&org.0);
        db::interact(&self.pool, move |conn| {
            recovery_record(conn, &artifact, &org)
        })
        .await
    }

    /// Returns a recovered anchor only to the root-thread delivery path after binding every
    /// artifact, tenant, and selected destination dimension.  This intentionally does *not*
    /// synthesize a `provider_delivery_outbox` acceptance: a recovered historic notification has
    /// no durable delivery row, and its provenance stays in the recovery table instead.
    pub async fn recovered_anchor_message(
        &self,
        artifact: &ArtifactId,
        org: &OrgId,
        destination: &RecoveryDestination,
    ) -> Result<Option<String>, AppError> {
        let artifact = trimmed(&artifact.0);
        let org = trimmed(&org.0);
        let destination = destination.clone();
        db::interact(&self.pool, move |conn| {
            conn.query_row(
                "SELECT recovered_message_id FROM discord_notification_anchor_recoveries \
                 WHERE artifact_id=?1 AND org=?2 AND connection_id=?3 AND notification_webhook_id=?4 \
                   AND provider_webhook_id=?5 AND guild_id=?6 AND channel_id=?7 AND state='recovered' \
                   AND provenance='exact_selected_webhook_canonical_url'",
                params![artifact, org, destination.connection_id, destination.notification_webhook_id, destination.provider_webhook_id, destination.guild_id, destination.channel_id],
                |row| row.get::<_, String>(0),
            ).optional().map_err(internal)
        }).await
    }
}

impl OrganizationDiscordCredentialService for OrganizationDiscordStore {
    fn credential_for_provider<'a>(
        &'a self,
        org: &'a OrgId,
    ) -> crate::ports::BoxFuture<'a, Result<Option<Secret>, AppError>> {
        Box::pin(async move {
            let org_name = trimmed(&org.0);
            let protection = Arc::clone(&self.protection);
            let fallback = self.legacy_fallback.clone();
            db::interact(&self.pool, move |conn| {
                let row = conn.query_row(
                    "SELECT ciphertext, nonce, tag, active FROM org_discord_bot_credentials WHERE org=?1",
                    [&org_name],
                    |row| {
                        Ok((
                            EncryptedUrl {
                                ciphertext: row.get(0)?,
                                nonce: row.get(1)?,
                                tag: row.get(2)?,
                            },
                            row.get::<_, i64>(3)? != 0,
                        ))
                    },
                ).optional().map_err(internal)?;
                match row {
                    Some((_, false)) => Ok(None),
                    Some((encrypted, true)) => {
                        let Some(cipher) = protection.cipher() else { return Err(AppError::Internal); };
                        cipher.decrypt(&encrypted).map(Secret::new).map(Some)
                    }
                    None if has_legacy_discussion_connection(conn, &org_name)? => Ok(fallback),
                    None => Ok(None),
                }
            }).await
        })
    }
}

fn credential_active(conn: &Connection, org: &str) -> Result<Option<bool>, AppError> {
    conn.query_row(
        "SELECT active FROM org_discord_bot_credentials WHERE org=?1",
        [org],
        |row| row.get::<_, i64>(0),
    )
    .optional()
    .map(|active| active.map(|value| value != 0))
    .map_err(internal)
}

fn readiness_in_transaction(
    conn: &Connection,
    org: &str,
    fallback_available: bool,
) -> Result<DiscordCredentialReadiness, AppError> {
    match credential_active(conn, org)? {
        Some(true) => Ok(DiscordCredentialReadiness::Configured),
        Some(false) => Ok(DiscordCredentialReadiness::Deactivated),
        None if fallback_available && has_legacy_discussion_connection(conn, org)? => {
            Ok(DiscordCredentialReadiness::LegacyFallback)
        }
        None => Ok(DiscordCredentialReadiness::Unconfigured),
    }
}

fn status_in_transaction(
    conn: &Connection,
    org: &str,
    credential_readiness: DiscordCredentialReadiness,
) -> Result<OrganizationThreadingStatus, AppError> {
    let outbound_enabled = conn
        .query_row(
            "SELECT outbound_enabled FROM org_discord_threading_policies WHERE org=?1",
            [org],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(internal)?
        .unwrap_or(0)
        != 0;
    let (pending, failed): (i64, i64) = conn
        .query_row(
            "SELECT \
               COUNT(*) FILTER (WHERE state IN ('pending','recovering','retryable','rate_limited')), \
               COUNT(*) FILTER (WHERE state IN ('not_found','ambiguous','permission_denied','invalid')) \
             FROM discord_notification_anchor_recoveries WHERE org=?1",
            [org],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(internal)?;
    let recovery_state = if pending > 0 {
        "recovering"
    } else if failed > 0 {
        "degraded"
    } else {
        "idle"
    };
    Ok(OrganizationThreadingStatus {
        outbound_enabled,
        credential_readiness,
        recovery_pending: pending.max(0) as u64,
        recovery_state,
    })
}

fn recovery_record(
    conn: &Connection,
    artifact: &str,
    org: &str,
) -> Result<Option<RecoveryRecord>, AppError> {
    conn.query_row(
        "SELECT state, recovered_message_id, attempts, last_error FROM discord_notification_anchor_recoveries WHERE artifact_id=?1 AND org=?2",
        params![artifact, org],
        |row| Ok(RecoveryRecord {
            state: parse_recovery_state(row.get(0)?)?,
            recovered_message_id: row.get(1)?,
            attempts: row.get::<_, i64>(2)?.unsigned_abs(),
            last_error: row.get(3)?,
        }),
    ).optional().map_err(internal)
}

fn parse_recovery_state(value: String) -> rusqlite::Result<RecoveryState> {
    match value.as_str() {
        "pending" => Ok(RecoveryState::Pending),
        "recovering" => Ok(RecoveryState::Recovering),
        "recovered" => Ok(RecoveryState::Recovered),
        "not_found" => Ok(RecoveryState::NotFound),
        "ambiguous" => Ok(RecoveryState::Ambiguous),
        "permission_denied" => Ok(RecoveryState::PermissionDenied),
        "rate_limited" => Ok(RecoveryState::RateLimited),
        "retryable" => Ok(RecoveryState::Retryable),
        "invalid" => Ok(RecoveryState::Invalid),
        _ => Err(rusqlite::Error::InvalidQuery),
    }
}

fn destination_matches(
    conn: &Connection,
    org: &str,
    destination: &RecoveryDestination,
) -> Result<bool, AppError> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM org_discord_discussion_connections WHERE id=?1 AND org=?2 \
         AND strategy='notification_thread' AND notification_webhook_id=?3 \
         AND notification_provider_webhook_id=?4 AND guild_id=?5 AND channel_id=?6)",
        params![
            destination.connection_id,
            org,
            destination.notification_webhook_id,
            destination.provider_webhook_id,
            destination.guild_id,
            destination.channel_id
        ],
        |row| row.get(0),
    )
    .map_err(internal)
}

fn destination_matches_preflight(destination: &RecoveryDestination) -> bool {
    !destination.connection_id.trim().is_empty()
        && !destination.notification_webhook_id.trim().is_empty()
        && valid_discord_id(destination.provider_webhook_id.trim())
        && valid_discord_id(destination.guild_id.trim())
        && valid_discord_id(destination.channel_id.trim())
}

fn artifact_exists(conn: &Connection, artifact: &str, org: &str) -> Result<bool, AppError> {
    conn.query_row(
        "SELECT 1 FROM artifacts WHERE id=?1 AND org=?2",
        params![artifact, org],
        |_| Ok(()),
    )
    .optional()
    .map(|row: Option<()>| row.is_some())
    .map_err(internal)
}

fn org_exists(conn: &Connection, org: &str) -> Result<bool, AppError> {
    conn.query_row("SELECT 1 FROM orgs WHERE name=?1", [org], |_| Ok(()))
        .optional()
        .map(|row: Option<()>| row.is_some())
        .map_err(internal)
}

/// The process-level token exists only to carry forward a deployment that already used a
/// PBI-079 discussion connection. It is never offered to an unrelated organization merely
/// because the deployment has a fallback configured.
fn has_legacy_discussion_connection(conn: &Connection, org: &str) -> Result<bool, AppError> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM org_discord_discussion_connections WHERE org=?1)",
        [org],
        |row| row.get(0),
    )
    .map_err(internal)
}

fn is_canonical_url(value: &str) -> bool {
    url::Url::parse(value).is_ok_and(|parsed| {
        matches!(parsed.scheme(), "http" | "https")
            && parsed.host_str().is_some()
            && parsed.fragment().is_none()
    })
}

fn valid_discord_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 32 && value.bytes().all(|byte| byte.is_ascii_digit())
}
fn trimmed(value: &str) -> String {
    value.trim().to_owned()
}
fn internal(error: rusqlite::Error) -> AppError {
    tracing::error!(error = %error, "organization Discord persistence failed");
    AppError::Internal
}

impl std::fmt::Debug for OrganizationDiscordStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OrganizationDiscordStore")
            .field("protection", &self.protection)
            .field("legacy_fallback", &self.legacy_fallback.is_some())
            .finish_non_exhaustive()
    }
}
