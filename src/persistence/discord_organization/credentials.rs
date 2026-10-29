use std::sync::Arc;

use rusqlite::{OptionalExtension, TransactionBehavior, params};

use crate::{
    config::Secret,
    error::AppError,
    model::{ArtifactId, OrgId},
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

use super::{
    helpers::{
        ENCRYPTION_REQUIRED, INVALID_CREDENTIAL, artifact_exists, credential_active,
        has_legacy_discussion_connection, internal, org_exists, readiness_in_transaction,
        status_in_transaction, trimmed,
    },
    types::{
        ArtifactDiscussionOverride, DiscordCredentialReadiness, EffectiveDiscussionPolicy,
        OrganizationDiscordStore, OrganizationThreadingStatus,
    },
};

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
