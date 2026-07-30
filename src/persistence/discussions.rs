//! Durable persistence for PBI-079's optional Discord discussion mirror.
//!
//! This module owns only local state. It deliberately does not perform Discord I/O, and it keeps
//! the bearer URL behind the same [`WebhookUrlProtection`] boundary used by event webhooks.
//! Absent `artifact_discussions` rows mean local-only; migration v28 never backfills them.

use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension, Row, Transaction, TransactionBehavior, params};

use crate::{
    error::AppError,
    integrations::delivery_envelope::stable_delivery_event_id,
    model::{ArtifactId, FeedbackId, OrgId, Timestamp, WebhookEvent},
    persistence::{
        db::{self, DbPool},
        migrations::EncryptedUrl,
        outbox::{DeadLetterTransition, dead_letter_in_transaction},
        webhooks::{
            INVALID_URL_MESSAGE, MAX_LABEL_UTF16, is_discord_webhook_url, mask_url, truncate_utf16,
        },
    },
    security::audit::{AuditEvent, MutationAudit, append_in_transaction, mutate_in_transaction},
    security::crypto::{StoredWebhookUrl, WebhookUrlProtection},
};

const CONNECTION_COLUMNS: &str = "id, org, url, url_cipher, url_nonce, url_tag, label, created_at, \
                                  updated_at, last_ok_at, last_error, strategy, \
                                  notification_webhook_id, channel_id, guild_id, \
                                  notification_provider_webhook_id";
const DISCUSSION_COLUMNS: &str = "artifact_id, org, provider, mode, connection_org, connection_id, thread_id, \
    root_message_id, state, generation, enabled_by, enabled_at, disabled_at, last_synced_at, \
    last_error, created_at, updated_at, anchor_outbox_id";
const LINK_COLUMNS: &str = "provider, artifact_id, org, connection_id, feedback_id, delivery_event_id, outbox_id, \
    tombstone_outbox_id, external_thread_id, external_message_id, source, generation, state, last_error, local_deleted_at, \
    created_at, updated_at, posted_at";
const CONNECTION_ID_ALPHABET: &[char] = &[
    'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j', 'k', 'l', 'm', 'n', 'o', 'p', 'q', 'r', 's',
    't', 'u', 'v', 'w', 'x', 'y', 'z', '0', '1', '2', '3', '4', '5', '6', '7', '8', '9',
];

fn new_connection_id() -> String {
    format!("discussion-{}", nanoid::nanoid!(21, CONNECTION_ID_ALPHABET))
}

fn append_audit_then_commit<T>(
    tx: Transaction<'_>,
    audit_key: &[u8; 32],
    audit: &MutationAudit,
    event: AuditEvent,
    value: T,
) -> Result<T, AppError> {
    append_in_transaction(&tx, audit_key, &audit.event_id()?, audit.context(), &event)?;
    tx.commit().map_err(internal)?;
    Ok(value)
}

/// The persisted mode. No row is the same as [`Self::ArtifactMcpOnly`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscussionMode {
    ArtifactMcpOnly,
    DiscordMirror,
}

impl DiscussionMode {
    fn parse(value: String) -> rusqlite::Result<Self> {
        match value.as_str() {
            "artifact_only" => Ok(Self::ArtifactMcpOnly),
            "discord_mirror" => Ok(Self::DiscordMirror),
            _ => Err(rusqlite::Error::InvalidQuery),
        }
    }
}

/// Operator-visible state of an artifact discussion mirror.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscussionState {
    Local,
    Pending,
    Connected,
    Paused,
    Failed,
}

impl DiscussionState {
    fn parse(value: String) -> rusqlite::Result<Self> {
        match value.as_str() {
            "local" => Ok(Self::Local),
            "pending" => Ok(Self::Pending),
            "connected" => Ok(Self::Connected),
            "paused" => Ok(Self::Paused),
            "failed" => Ok(Self::Failed),
            _ => Err(rusqlite::Error::InvalidQuery),
        }
    }
}

/// How Discord discussion delivery is rooted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscussionConnectionStrategy {
    /// Legacy PBI-079 destination that creates a standalone Forum/Media post.
    ForumWebhook,
    /// A public thread attached to the existing artifact-published notification.
    NotificationThread,
}

impl DiscussionConnectionStrategy {
    fn parse(value: String) -> rusqlite::Result<Self> {
        match value.as_str() {
            "forum_webhook" => Ok(Self::ForumWebhook),
            "notification_thread" => Ok(Self::NotificationThread),
            _ => Err(rusqlite::Error::InvalidQuery),
        }
    }
}

/// A settings-safe discussion connection representation. `destination` is always masked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscussionConnectionSummary {
    /// Opaque, immutable credential identity used by durable delivery correlations.
    pub id: String,
    pub org: OrgId,
    pub label: String,
    pub destination: String,
    pub strategy: DiscussionConnectionStrategy,
    pub notification_webhook_id: Option<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub last_ok_at: Option<Timestamp>,
    pub last_error: Option<String>,
}

/// The narrowly-scoped delivery capability. It must never be rendered or logged.
#[derive(Clone, PartialEq, Eq)]
pub struct DiscussionConnectionDelivery {
    pub org: OrgId,
    pub label: String,
    pub url: String,
    pub strategy: DiscussionConnectionStrategy,
    pub notification_webhook_id: Option<String>,
    pub notification_provider_webhook_id: Option<String>,
    pub channel_id: Option<String>,
    pub guild_id: Option<String>,
}

impl std::fmt::Debug for DiscussionConnectionDelivery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscussionConnectionDelivery")
            .field("org", &self.org)
            .field("label", &self.label)
            .field("url", &"<redacted>")
            .field("strategy", &self.strategy)
            .field("notification_webhook_id", &self.notification_webhook_id)
            .field(
                "notification_provider_webhook_id",
                &self.notification_provider_webhook_id,
            )
            .field("channel_id", &self.channel_id)
            .field("guild_id", &self.guild_id)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactDiscussion {
    pub artifact_id: ArtifactId,
    pub org: OrgId,
    pub mode: DiscussionMode,
    pub connection_org: Option<OrgId>,
    pub connection_id: Option<String>,
    pub thread_id: Option<String>,
    pub root_message_id: Option<String>,
    pub state: DiscussionState,
    pub generation: u64,
    pub enabled_by: Option<String>,
    pub enabled_at: Option<Timestamp>,
    pub disabled_at: Option<Timestamp>,
    pub last_synced_at: Option<Timestamp>,
    pub last_error: Option<String>,
    pub created_at: Option<Timestamp>,
    pub updated_at: Option<Timestamp>,
    pub anchor_outbox_id: Option<String>,
}

impl ArtifactDiscussion {
    /// Construct the virtual state represented by a missing database row.
    #[must_use]
    pub fn local_only(artifact_id: ArtifactId, org: OrgId) -> Self {
        Self {
            artifact_id,
            org,
            mode: DiscussionMode::ArtifactMcpOnly,
            connection_org: None,
            connection_id: None,
            thread_id: None,
            root_message_id: None,
            state: DiscussionState::Local,
            generation: 0,
            enabled_by: None,
            enabled_at: None,
            disabled_at: None,
            last_synced_at: None,
            last_error: None,
            created_at: None,
            updated_at: None,
            anchor_outbox_id: None,
        }
    }
}

/// One persistent correlation between canonical feedback and one provider delivery job/message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscussionMessageLink {
    pub artifact_id: ArtifactId,
    pub org: OrgId,
    pub connection_id: String,
    pub feedback_id: FeedbackId,
    pub delivery_event_id: String,
    pub outbox_id: String,
    pub tombstone_outbox_id: Option<String>,
    pub external_thread_id: Option<String>,
    pub external_message_id: Option<String>,
    pub generation: u64,
    pub state: String,
    pub last_error: Option<String>,
    pub local_deleted_at: Option<Timestamp>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub posted_at: Option<Timestamp>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateDiscussionConnection {
    pub org: OrgId,
    pub url: String,
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateNotificationThreadConnection {
    pub org: OrgId,
    pub notification_webhook_id: String,
    pub notification_provider_webhook_id: String,
    pub channel_id: String,
    pub guild_id: String,
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateDiscussionMessageLink {
    pub artifact_id: ArtifactId,
    pub org: OrgId,
    pub connection_id: String,
    pub feedback_id: FeedbackId,
    pub delivery_event_id: String,
    pub outbox_id: String,
    pub external_thread_id: Option<String>,
    pub generation: u64,
}

/// A tombstone job attached to an already-posted feedback mapping.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BindDiscussionTombstone {
    pub artifact_id: ArtifactId,
    pub org: OrgId,
    pub feedback_id: FeedbackId,
    pub connection_id: String,
    pub generation: u64,
    pub outbox_id: String,
}

/// Inputs proven by the worker before one atomic accepted-result commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcceptedDiscussionDelivery {
    pub outbox_id: String,
    pub worker: String,
    pub lease_token: String,
    pub lease_version: i64,
    pub external_thread_id: String,
    pub external_message_id: String,
    pub now_millis: i64,
}

/// Inputs proven by the worker before accepting a marker. Markers intentionally have no
/// message-link row; their durable receipt is the outbox row itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcceptedDiscussionMarker {
    pub outbox_id: String,
    pub worker: String,
    pub lease_token: String,
    pub lease_version: i64,
    pub artifact_id: ArtifactId,
    pub org: OrgId,
    pub connection_id: String,
    pub generation: u64,
    pub message_id: String,
    pub now_millis: i64,
}

/// Guarded terminal update for a discussion delivery. The operation-specific durable state is
/// changed in the same SQLite transaction as the outbox terminal transition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalDiscussionDelivery {
    pub outbox_id: String,
    pub worker: String,
    pub lease_token: String,
    pub lease_version: i64,
    pub artifact_id: ArtifactId,
    pub org: OrgId,
    pub connection_id: String,
    pub generation: u64,
    pub delivery_kind: String,
    pub feedback_id: Option<FeedbackId>,
    pub classification: String,
    pub duplicate_risk: bool,
    pub now_millis: i64,
}

/// PBI-079 repository. The store is intentionally independent from `WebhookStore`: a general
/// event webhook must never become a discussion target by accident.
#[derive(Clone)]
pub struct DiscussionStore {
    pool: DbPool,
    protection: Arc<WebhookUrlProtection>,
}

impl DiscussionStore {
    #[must_use]
    pub const fn new(pool: DbPool, protection: Arc<WebhookUrlProtection>) -> Self {
        Self { pool, protection }
    }

    /// Creates or replaces an organization's dedicated discussion destination.
    pub async fn upsert_connection(
        &self,
        input: CreateDiscussionConnection,
    ) -> Result<DiscussionConnectionSummary, AppError> {
        let org = trimmed(&input.org.0);
        let url = trimmed(&input.url);
        if !is_discord_webhook_url(&url) {
            return Err(AppError::Validation(INVALID_URL_MESSAGE.to_owned()));
        }
        let label = truncate_utf16(&trimmed(&input.label), MAX_LABEL_UTF16);
        let stored = self.protection.protect(&url)?;
        db::interact(&self.pool, move |conn| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(internal)?;
            if !org_exists(&tx, &org)? {
                return Err(AppError::Validation(format!(
                    "Unknown organization \"{org}\"."
                )));
            }
            if has_bound_connection_authority(&tx, &org)? {
                return Err(AppError::Conflict(
                    "This connection is retained because Discord discussion history depends on it."
                        .to_owned(),
                ));
            }
            let connection_id = new_connection_id();
            // A connection ID is credential identity, not an organization alias.  Replacement is
            // allowed only with no durable authority bound to the old credential, then deletes
            // and recreates the row so no queued work can silently resolve to new bearer URL.
            tx.execute(
                "DELETE FROM org_discord_discussion_connections WHERE org = ?1",
                [&org],
            )
            .map_err(internal)?;
            tx.execute(
                "INSERT INTO org_discord_discussion_connections \
                 (id, org, url, url_cipher, url_nonce, url_tag, label) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    connection_id,
                    org,
                    stored.url,
                    stored.encrypted.as_ref().map(|value| &value.ciphertext),
                    stored.encrypted.as_ref().map(|value| &value.nonce),
                    stored.encrypted.as_ref().map(|value| &value.tag),
                    label,
                ],
            )
            .map_err(internal)?;
            let summary = connection_row(&tx, &org)?
                .map(DiscussionConnectionRow::summary)
                .ok_or(AppError::Internal)?;
            tx.commit().map_err(internal)?;
            Ok(summary)
        })
        .await
    }

    /// Configure a credential and record the durable mutation in the same transaction.  The URL
    /// is protected before SQLite and never crosses into the audit event.
    pub async fn upsert_connection_audited(
        &self,
        input: CreateDiscussionConnection,
        audit: MutationAudit,
        audit_key: [u8; 32],
    ) -> Result<DiscussionConnectionSummary, AppError> {
        let org = trimmed(&input.org.0);
        let url = trimmed(&input.url);
        if !is_discord_webhook_url(&url) {
            return Err(AppError::Validation(INVALID_URL_MESSAGE.to_owned()));
        }
        let label = truncate_utf16(&trimmed(&input.label), MAX_LABEL_UTF16);
        let stored = self.protection.protect(&url)?;
        let target = org.clone();
        db::interact(&self.pool, move |conn| {
            let audit = audit.for_target_tenant(&org)?;
            mutate_in_transaction(conn, &audit_key, &audit, |tx| {
                if !org_exists(tx, &org)? {
                    return Err(AppError::Validation(format!("Unknown organization \"{org}\".")));
                }
                if has_bound_connection_authority(tx, &org)? {
                    return Err(AppError::Conflict("This connection is retained because Discord discussion history depends on it.".to_owned()));
                }
                tx.execute("DELETE FROM org_discord_discussion_connections WHERE org = ?1", [&org]).map_err(internal)?;
                tx.execute(
                    "INSERT INTO org_discord_discussion_connections (id, org, url, url_cipher, url_nonce, url_tag, label) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![new_connection_id(), org, stored.url, stored.encrypted.as_ref().map(|value| &value.ciphertext), stored.encrypted.as_ref().map(|value| &value.nonce), stored.encrypted.as_ref().map(|value| &value.tag), label],
                ).map_err(internal)?;
                let summary = connection_row(tx, &org)?.map(DiscussionConnectionRow::summary).ok_or(AppError::Internal)?;
                Ok((summary, AuditEvent {
                    operation: "discussion.connection.configure".to_owned(), target_type: "organization".to_owned(), target_id: target,
                    result: "success".to_owned(), classification: "discussion_connection_configured".to_owned(), revision: None,
                }))
            })
        }).await
    }

    /// Select an existing artifact-notification webhook as the anchor for public Discord threads.
    /// The incoming webhook remains the message author; the bot credential is never persisted
    /// here and is used only by the transport to manage the thread.
    pub async fn upsert_notification_thread_connection_audited(
        &self,
        input: CreateNotificationThreadConnection,
        audit: MutationAudit,
        audit_key: [u8; 32],
    ) -> Result<DiscussionConnectionSummary, AppError> {
        let org = trimmed(&input.org.0);
        let webhook_id = trimmed(&input.notification_webhook_id);
        let provider_webhook_id = trimmed(&input.notification_provider_webhook_id);
        let channel_id = trimmed(&input.channel_id);
        let guild_id = trimmed(&input.guild_id);
        if webhook_id.is_empty()
            || webhook_id.len() > 160
            || !valid_discord_id(&provider_webhook_id)
            || !valid_discord_id(&channel_id)
            || !valid_discord_id(&guild_id)
        {
            return Err(AppError::Validation(
                "Invalid Discord notification destination.".to_owned(),
            ));
        }
        let label = truncate_utf16(&trimmed(&input.label), MAX_LABEL_UTF16);
        let target = org.clone();
        db::interact(&self.pool, move |conn| {
            let audit = audit.for_target_tenant(&org)?;
            mutate_in_transaction(conn, &audit_key, &audit, |tx| {
                if !org_exists(tx, &org)? {
                    return Err(AppError::Validation(format!(
                        "Unknown organization \"{org}\"."
                    )));
                }
                if !notification_webhook_is_eligible(tx, &org, &webhook_id)? {
                    return Err(AppError::Validation(
                        "Select an organization webhook subscribed to published artifacts."
                            .to_owned(),
                    ));
                }
                if has_bound_connection_authority(tx, &org)? {
                    return Err(AppError::Conflict(
                        "This connection is retained because Discord discussion history depends on it."
                            .to_owned(),
                    ));
                }
                tx.execute(
                    "DELETE FROM org_discord_discussion_connections WHERE org = ?1",
                    [&org],
                )
                .map_err(internal)?;
                tx.execute(
                    "INSERT INTO org_discord_discussion_connections \
                     (id, org, url, label, strategy, notification_webhook_id, channel_id, guild_id, \
                      notification_provider_webhook_id) \
                     VALUES (?1, ?2, '', ?3, 'notification_thread', ?4, ?5, ?6, ?7)",
                    params![
                        new_connection_id(),
                        org,
                        label,
                        webhook_id,
                        channel_id,
                        guild_id,
                        provider_webhook_id
                    ],
                )
                .map_err(internal)?;
                let summary = connection_row(tx, &org)?
                    .map(DiscussionConnectionRow::summary)
                    .ok_or(AppError::Internal)?;
                Ok((
                    summary,
                    AuditEvent {
                        operation: "discussion.connection.configure".to_owned(),
                        target_type: "organization".to_owned(),
                        target_id: target,
                        result: "success".to_owned(),
                        classification: "notification_thread_connection_configured".to_owned(),
                        revision: None,
                    },
                ))
            })
        })
        .await
    }

    pub async fn connection_summary(
        &self,
        org: &OrgId,
    ) -> Result<Option<DiscussionConnectionSummary>, AppError> {
        let org = trimmed(&org.0);
        db::interact(&self.pool, move |conn| {
            Ok(connection_row(conn, &org)?.map(DiscussionConnectionRow::summary))
        })
        .await
    }

    /// Resolves a bearer URL only for a same-tenant delivery attempt.
    pub async fn connection_for_delivery(
        &self,
        connection_id: &str,
        org: &OrgId,
    ) -> Result<Option<DiscussionConnectionDelivery>, AppError> {
        let connection_id = trimmed(connection_id);
        require_nonempty(&[&connection_id])?;
        let org = trimmed(&org.0);
        let protection = self.protection.clone();
        db::interact(&self.pool, move |conn| {
            connection_row_by_id(conn, &connection_id, &org)?
                .map(|row| {
                    let stored_url = delivery_url_for_connection(conn, &row)?;
                    Ok(DiscussionConnectionDelivery {
                        org: row.org.clone(),
                        label: row.label.clone(),
                        url: protection.reveal(&stored_url)?,
                        strategy: row.strategy,
                        notification_webhook_id: row.notification_webhook_id.clone(),
                        notification_provider_webhook_id: row
                            .notification_provider_webhook_id
                            .clone(),
                        channel_id: row.channel_id.clone(),
                        guild_id: row.guild_id.clone(),
                    })
                })
                .transpose()
        })
        .await
    }

    /// Removes only the organization-level destination. Artifact rows remain durable evidence and
    /// can be surfaced as paused/failed by APP1; deleting an organization cascades all rows.
    pub async fn remove_connection(&self, org: &OrgId) -> Result<bool, AppError> {
        let org = trimmed(&org.0);
        db::interact(&self.pool, move |conn| {
            if has_bound_connection_authority(conn, &org)? {
                return Err(AppError::Conflict(
                    "This connection is retained because Discord discussion history depends on it."
                        .to_owned(),
                ));
            }
            conn.execute(
                "DELETE FROM org_discord_discussion_connections WHERE org = ?1",
                [&org],
            )
            .map_err(internal)
            .map(|changed| changed > 0)
        })
        .await
    }

    pub async fn remove_connection_audited(
        &self,
        org: OrgId,
        audit: MutationAudit,
        audit_key: [u8; 32],
    ) -> Result<bool, AppError> {
        let org = trimmed(&org.0);
        let target = org.clone();
        db::interact(&self.pool, move |conn| {
            let audit = audit.for_target_tenant(&org)?;
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(internal)?;
            if has_bound_connection_authority(&tx, &org)? {
                return Err(AppError::Conflict(
                    "This connection is retained because Discord discussion history depends on it."
                        .to_owned(),
                ));
            }
            let removed = tx
                .execute(
                    "DELETE FROM org_discord_discussion_connections WHERE org = ?1",
                    [&org],
                )
                .map_err(internal)?
                > 0;
            // An absent connection is an idempotent no-op and must not mint a misleading audit event.
            if !removed {
                tx.commit().map_err(internal)?;
                return Ok(false);
            }
            append_audit_then_commit(
                tx,
                &audit_key,
                &audit,
                AuditEvent {
                    operation: "discussion.connection.remove".to_owned(),
                    target_type: "organization".to_owned(),
                    target_id: target,
                    result: "success".to_owned(),
                    classification: "discussion_connection_removed".to_owned(),
                    revision: None,
                },
                true,
            )
        })
        .await
    }

    /// Enables a mirror. This is a persistence primitive; API1 supplies the owner/admin check.
    pub async fn enable_mirror(
        &self,
        artifact_id: &ArtifactId,
        org: &OrgId,
        enabled_by: &str,
    ) -> Result<ArtifactDiscussion, AppError> {
        let artifact_id = trimmed(&artifact_id.0);
        let org = trimmed(&org.0);
        let enabled_by = truncate_utf16(&trimmed(enabled_by), 320);
        db::interact(&self.pool, move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(internal)?;
            let authority = mirror_authority(&tx, &artifact_id, &org)?;
            tx.execute(
                "INSERT INTO artifact_discussions (artifact_id, org, provider, mode, connection_org, connection_id, state, generation, enabled_by, enabled_at, anchor_outbox_id) \
                 VALUES (?1, ?2, 'discord', 'discord_mirror', ?2, ?3, 'pending', 1, ?4, datetime('now'), ?5) \
                 ON CONFLICT(artifact_id) DO UPDATE SET org = excluded.org, provider = 'discord', \
                   mode = 'discord_mirror', connection_org = excluded.connection_org, connection_id = excluded.connection_id, state = 'pending', \
                   generation = artifact_discussions.generation + 1, enabled_by = excluded.enabled_by, enabled_at = datetime('now'), disabled_at = NULL, \
                   thread_id = NULL, root_message_id = NULL, last_synced_at = NULL, last_error = NULL, \
                   anchor_outbox_id = excluded.anchor_outbox_id, updated_at = datetime('now')",
                params![
                    artifact_id,
                    org,
                    authority.connection_id,
                    enabled_by,
                    authority.anchor_outbox_id
                ],
            )
            .map_err(internal)?;
            let row = discussion_row(&tx, &artifact_id, &org)?.ok_or(AppError::NotFound("Artifact not found.".to_owned()))?;
            tx.commit().map_err(internal)?;
            Ok(row)
        })
        .await
    }

    /// Disabling never creates a row, preserving the no-backfill local-only invariant.
    pub async fn disable_mirror(
        &self,
        artifact_id: &ArtifactId,
        org: &OrgId,
    ) -> Result<ArtifactDiscussion, AppError> {
        let artifact_id = trimmed(&artifact_id.0);
        let org = trimmed(&org.0);
        db::interact(&self.pool, move |conn| {
            let changed = conn
                .execute(
                    "UPDATE artifact_discussions SET mode = 'artifact_only', state = 'paused', \
                 disabled_at = datetime('now'), updated_at = datetime('now') \
                 WHERE artifact_id = ?1 AND org = ?2",
                    params![artifact_id, org],
                )
                .map_err(internal)?;
            if changed == 0 {
                return artifact_exists(conn, &artifact_id, &org)?
                    .then(|| ArtifactDiscussion::local_only(ArtifactId(artifact_id), OrgId(org)))
                    .ok_or(AppError::NotFound("Artifact not found.".to_owned()));
            }
            discussion_row(conn, &artifact_id, &org)?.ok_or(AppError::Internal)
        })
        .await
    }

    pub async fn get_discussion(
        &self,
        artifact_id: &ArtifactId,
        org: &OrgId,
    ) -> Result<Option<ArtifactDiscussion>, AppError> {
        let artifact_id = trimmed(&artifact_id.0);
        let org = trimmed(&org.0);
        db::interact(&self.pool, move |conn| {
            discussion_row(conn, &artifact_id, &org)
        })
        .await
    }

    /// Resolve the already-accepted artifact-notification message that an anchored root job
    /// depends on. The query binds every tenant, artifact, connection, and generation dimension
    /// before returning Discord's non-secret message identifier.
    pub async fn notification_anchor_message(
        &self,
        outbox_id: &str,
        artifact_id: &ArtifactId,
        org: &OrgId,
        connection_id: &str,
        generation: u64,
    ) -> Result<Option<String>, AppError> {
        let outbox_id = trimmed(outbox_id);
        let artifact_id = trimmed(&artifact_id.0);
        let org = trimmed(&org.0);
        let connection_id = trimmed(connection_id);
        if generation == 0 {
            return Ok(None);
        }
        let subject = format!("artifact:{artifact_id}:1");
        let expected_event_id =
            stable_delivery_event_id(&OrgId(org.clone()), &WebhookEvent::Published, &subject);
        let generation = i64::try_from(generation).map_err(|_| AppError::Internal)?;
        db::interact(&self.pool, move |conn| {
            if outbox_id.is_empty() {
                return conn
                    .query_row(
                        "SELECT r.recovered_message_id \
                           FROM discord_notification_anchor_recoveries r \
                           JOIN artifact_discussions d \
                             ON d.artifact_id=r.artifact_id AND d.org=r.org \
                           JOIN org_discord_discussion_connections c ON c.id=d.connection_id \
                          WHERE r.artifact_id=?1 AND r.org=?2 AND r.connection_id=?3 \
                            AND d.generation=?4 AND d.mode='discord_mirror' \
                            AND d.anchor_outbox_id IS NULL AND r.state='recovered' \
                            AND r.provenance='exact_selected_webhook_canonical_url' \
                            AND r.notification_webhook_id=c.notification_webhook_id \
                            AND r.provider_webhook_id=c.notification_provider_webhook_id \
                            AND r.guild_id=c.guild_id AND r.channel_id=c.channel_id",
                        params![artifact_id, org, connection_id, generation],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()
                    .map_err(internal);
            }
            conn.query_row(
                "SELECT o.discord_message_id \
                   FROM provider_delivery_outbox o \
                   JOIN artifact_discussions d ON d.anchor_outbox_id = o.id \
                   JOIN org_discord_discussion_connections c ON c.id = d.connection_id \
                  WHERE o.id = ?1 AND o.state = 'accepted' \
                    AND o.provider = 'discord' AND o.delivery_kind = 'event' \
                    AND o.event_type = 'published' AND o.tenant = ?2 \
                    AND d.artifact_id = ?3 AND d.org = ?2 \
                    AND d.connection_id = ?4 AND d.generation = ?5 \
                    AND c.strategy = 'notification_thread' \
                    AND o.target_key = c.notification_webhook_id \
                    AND o.event_id = ?6",
                params![
                    outbox_id,
                    org,
                    artifact_id,
                    connection_id,
                    generation,
                    expected_event_id
                ],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map(|value| value.flatten())
            .map_err(internal)
        })
        .await
    }

    /// Atomically applies a desired mode. Repeated requests observe the current row under the
    /// same write lock and return it unchanged without incrementing generation or auditing a
    /// fictitious mutation.
    pub async fn set_mode_audited(
        &self,
        artifact_id: ArtifactId,
        org: OrgId,
        mode: DiscussionMode,
        enabled_by: String,
        audit: MutationAudit,
        audit_key: [u8; 32],
    ) -> Result<ArtifactDiscussion, AppError> {
        let artifact_id = trimmed(&artifact_id.0);
        let org = trimmed(&org.0);
        let enabled_by = truncate_utf16(&trimmed(&enabled_by), 320);
        let target = artifact_id.clone();
        db::interact(&self.pool, move |conn| {
            let audit = audit.for_target_tenant(&org)?;
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(internal)?;
            let current = discussion_row(&tx, &artifact_id, &org)?;
            let unchanged = matches!((mode, current.as_ref().map(|row| row.mode)),
                (DiscussionMode::ArtifactMcpOnly, None | Some(DiscussionMode::ArtifactMcpOnly)) |
                (DiscussionMode::DiscordMirror, Some(DiscussionMode::DiscordMirror)));
            if unchanged {
                let value = current.unwrap_or_else(|| ArtifactDiscussion::local_only(ArtifactId(artifact_id), OrgId(org)));
                tx.commit().map_err(internal)?;
                return Ok(value);
            }
            let value = match mode {
                DiscussionMode::ArtifactMcpOnly => {
                    tx.execute(
                        "UPDATE artifact_discussions SET mode='artifact_only', state='paused', disabled_at=datetime('now'), updated_at=datetime('now') WHERE artifact_id=?1 AND org=?2",
                        params![artifact_id, org],
                    ).map_err(internal)?;
                    discussion_row(&tx, &artifact_id, &org)?.ok_or(AppError::Internal)?
                }
                DiscussionMode::DiscordMirror => {
                    let authority = mirror_authority(&tx, &artifact_id, &org)?;
                    tx.execute(
                        "INSERT INTO artifact_discussions (artifact_id, org, provider, mode, connection_org, connection_id, state, generation, enabled_by, enabled_at, anchor_outbox_id) VALUES (?1, ?2, 'discord', 'discord_mirror', ?2, ?3, 'pending', 1, ?4, datetime('now'), ?5) ON CONFLICT(artifact_id) DO UPDATE SET org=excluded.org, provider='discord', mode='discord_mirror', connection_org=excluded.connection_org, connection_id=excluded.connection_id, state='pending', generation=artifact_discussions.generation+1, enabled_by=excluded.enabled_by, enabled_at=datetime('now'), disabled_at=NULL, thread_id=NULL, root_message_id=NULL, last_synced_at=NULL, last_error=NULL, anchor_outbox_id=excluded.anchor_outbox_id, updated_at=datetime('now')",
                        params![artifact_id, org, authority.connection_id, enabled_by, authority.anchor_outbox_id],
                    ).map_err(internal)?;
                    discussion_row(&tx, &artifact_id, &org)?.ok_or(AppError::Internal)?
                }
            };
            let event = AuditEvent {
                operation: "discussion.mode.set".to_owned(), target_type: "artifact".to_owned(), target_id: target,
                result: "success".to_owned(), classification: "discussion_mode_updated".to_owned(), revision: None,
            };
            append_audit_then_commit(tx, &audit_key, &audit, event, value)
        }).await
    }

    /// A retry is deliberately narrower than enable: only a failed mirror can mint one new
    /// generation. Existing outbox/dead-letter history is never selected, updated, or replayed.
    pub async fn retry_audited(
        &self,
        artifact_id: ArtifactId,
        org: OrgId,
        enabled_by: String,
        audit: MutationAudit,
        audit_key: [u8; 32],
    ) -> Result<ArtifactDiscussion, AppError> {
        let artifact_id = trimmed(&artifact_id.0);
        let org = trimmed(&org.0);
        let enabled_by = truncate_utf16(&trimmed(&enabled_by), 320);
        let target = artifact_id.clone();
        db::interact(&self.pool, move |conn| {
            let audit = audit.for_target_tenant(&org)?;
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(internal)?;
            let current = discussion_row(&tx, &artifact_id, &org)?
                .ok_or_else(|| AppError::Conflict("Discord discussion mirroring is not enabled.".to_owned()))?;
            if current.mode != DiscussionMode::DiscordMirror {
                return Err(AppError::Conflict("Discord discussion mirroring is not enabled.".to_owned()));
            }
            if current.state != DiscussionState::Failed {
                tx.commit().map_err(internal)?;
                return Ok(current);
            }
            let authority = mirror_authority(&tx, &artifact_id, &org)?;
            tx.execute(
                "UPDATE artifact_discussions SET connection_org=?2, connection_id=?3, state='pending', generation=generation+1, enabled_by=?4, enabled_at=datetime('now'), thread_id=NULL, root_message_id=NULL, last_synced_at=NULL, last_error=NULL, anchor_outbox_id=?5, updated_at=datetime('now') WHERE artifact_id=?1 AND org=?2 AND mode='discord_mirror' AND state='failed'",
                params![
                    artifact_id,
                    org,
                    authority.connection_id,
                    enabled_by,
                    authority.anchor_outbox_id
                ],
            ).map_err(internal)?;
            let value = discussion_row(&tx, &artifact_id, &org)?.ok_or(AppError::Internal)?;
            let event = AuditEvent {
                operation: "discussion.mode.retry".to_owned(), target_type: "artifact".to_owned(), target_id: target,
                result: "success".to_owned(), classification: "discussion_retry_new_generation".to_owned(), revision: None,
            };
            append_audit_then_commit(tx, &audit_key, &audit, event, value)
        }).await
    }

    /// The explicit two-transaction external-test record. This method also stores only a fixed
    /// success/failure marker; provider response/error text cannot become user-visible state.
    pub async fn audit_connection_test(
        &self,
        org: OrgId,
        connection_id: String,
        outcome: Option<bool>,
        audit: MutationAudit,
        audit_key: [u8; 32],
    ) -> Result<(), AppError> {
        let org = trimmed(&org.0);
        let connection_id = trimmed(&connection_id);
        db::interact(&self.pool, move |conn| {
            let audit = audit.for_target_tenant(&org)?;
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(internal)?;
            if outcome.is_none()
                && connection_row_by_id(&tx, &connection_id, &org)?.is_none()
            {
                return Err(AppError::Conflict(
                    "Discord discussion connection changed before testing.".to_owned(),
                ));
            }
            let detached = if let Some(ok) = outcome {
                tx.execute(
                    "UPDATE org_discord_discussion_connections SET last_ok_at=CASE WHEN ?3 THEN datetime('now') ELSE last_ok_at END, last_error=CASE WHEN ?3 THEN NULL ELSE 'delivery_failed' END, updated_at=datetime('now') WHERE org=?1 AND id=?2",
                    params![org, connection_id, ok],
                ).map_err(internal)? != 1
            } else {
                false
            };
            let success = outcome == Some(true) && !detached;
            let event = AuditEvent {
                operation: if outcome.is_some() {
                    "discussion.connection.test.completed"
                } else {
                    "discussion.connection.test.requested"
                }
                .to_owned(),
                target_type: "discussion_connection".to_owned(),
                target_id: connection_id,
                result: if outcome.is_some() && !success { "failure" } else { "success" }.to_owned(),
                classification: match outcome {
                    None => "external_delivery_requested",
                    Some(true) if !detached => "external_delivery_succeeded",
                    Some(_) => "external_delivery_failed",
                }
                .to_owned(),
                revision: None,
            };
            append_in_transaction(&tx, &audit_key, &audit.event_id()?, audit.context(), &event)?;
            tx.commit().map_err(internal)?;
            if detached {
                return Err(AppError::Conflict(
                    "Discord discussion connection changed during testing.".to_owned(),
                ));
            }
            Ok(())
        }).await
    }

    /// Creates a pending mapping exactly once. Retries must retain the original event/outbox IDs.
    pub async fn create_message_link(
        &self,
        input: CreateDiscussionMessageLink,
    ) -> Result<DiscussionMessageLink, AppError> {
        let artifact_id = trimmed(&input.artifact_id.0);
        let org = trimmed(&input.org.0);
        let feedback_id = trimmed(&input.feedback_id.0);
        let connection_id = trimmed(&input.connection_id);
        let event_id = trimmed(&input.delivery_event_id);
        let outbox_id = trimmed(&input.outbox_id);
        let thread = input
            .external_thread_id
            .map(|value| truncate_utf16(&trimmed(&value), 128));
        let generation = i64::try_from(input.generation)
            .map_err(|_| AppError::Validation("invalid discussion generation".to_owned()))?;
        if generation < 1 {
            return Err(AppError::Validation(
                "invalid discussion generation".to_owned(),
            ));
        }
        require_nonempty(&[
            &artifact_id,
            &org,
            &connection_id,
            &feedback_id,
            &event_id,
            &outbox_id,
        ])?;
        db::interact(&self.pool, move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(internal)?;
            tx.execute(
                "INSERT OR IGNORE INTO discussion_message_links \
                 (provider, artifact_id, org, connection_id, feedback_id, delivery_event_id, outbox_id, external_thread_id, generation, state) \
                 VALUES ('discord', ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending')",
                params![artifact_id, org, connection_id, feedback_id, event_id, outbox_id, thread, generation],
            ).map_err(internal)?;
            let row = link_by_outbox(&tx, &outbox_id)?.ok_or(AppError::Internal)?;
            if row.artifact_id.0 != artifact_id
                || row.org.0 != org
                || row.connection_id != connection_id
                || row.delivery_event_id != event_id
                || row.outbox_id != outbox_id
                || row.generation != input.generation
            {
                return Err(AppError::Conflict("discussion delivery idempotency conflict".to_owned()));
            }
            tx.commit().map_err(internal)?;
            Ok(row)
        }).await
    }

    pub async fn message_link_for_feedback(
        &self,
        artifact_id: &ArtifactId,
        org: &OrgId,
        feedback_id: &FeedbackId,
    ) -> Result<Option<DiscussionMessageLink>, AppError> {
        let artifact_id = trimmed(&artifact_id.0);
        let org = trimmed(&org.0);
        let feedback_id = trimmed(&feedback_id.0);
        db::interact(&self.pool, move |conn| {
            link_by_feedback(conn, &artifact_id, &org, &feedback_id)
        })
        .await
    }

    /// Records local deletion without losing the external mapping needed to tombstone a Discord
    /// message. APP1 subsequently enqueues the tombstone operation; this write never does I/O.
    pub async fn mark_feedback_locally_deleted(
        &self,
        artifact_id: &ArtifactId,
        org: &OrgId,
        feedback_id: &FeedbackId,
    ) -> Result<bool, AppError> {
        let artifact_id = trimmed(&artifact_id.0);
        let org = trimmed(&org.0);
        let feedback_id = trimmed(&feedback_id.0);
        db::interact(&self.pool, move |conn| {
            conn.execute(
                "UPDATE discussion_message_links SET state = CASE WHEN external_message_id IS NULL THEN 'local_deleted' ELSE 'tombstone_pending' END, \
                 local_deleted_at = datetime('now'), updated_at = datetime('now') \
                 WHERE provider = 'discord' AND artifact_id = ?1 AND org = ?2 AND feedback_id = ?3 AND local_deleted_at IS NULL",
                params![artifact_id, org, feedback_id],
            )
            .map_err(internal)
            .map(|changed| changed > 0)
        })
        .await
    }

    /// Atomically binds one durable tombstone job to an already-posted feedback mapping. The
    /// original post outbox reference remains immutable so retry, audit, and deletion evidence
    /// cannot be rewritten to point at the tombstone operation.
    pub async fn bind_tombstone_delivery(
        &self,
        input: BindDiscussionTombstone,
    ) -> Result<DiscussionMessageLink, AppError> {
        let artifact_id = trimmed(&input.artifact_id.0);
        let org = trimmed(&input.org.0);
        let feedback_id = trimmed(&input.feedback_id.0);
        let connection_id = trimmed(&input.connection_id);
        let outbox_id = trimmed(&input.outbox_id);
        let generation = i64::try_from(input.generation)
            .map_err(|_| AppError::Validation("invalid discussion generation".to_owned()))?;
        if generation < 1 {
            return Err(AppError::Validation(
                "invalid discussion generation".to_owned(),
            ));
        }
        require_nonempty(&[&artifact_id, &org, &feedback_id, &connection_id, &outbox_id])?;
        db::interact(&self.pool, move |conn| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(internal)?;
            let link = link_by_feedback_generation(&tx, &artifact_id, &org, &feedback_id, generation)?
                .ok_or(AppError::NotFound("discussion message link not found".to_owned()))?;
            if link.connection_id != connection_id
                || link.generation != u64::try_from(generation).map_err(|_| AppError::Internal)?
                || link.state != "tombstone_pending"
                || link.external_thread_id.is_none()
                || link.external_message_id.is_none()
            {
                return Err(AppError::Conflict("stale discussion tombstone authority".to_owned()));
            }
            let (tenant, target_key, secret_ref, kind, ordering_key, dependency): (
                String,
                String,
                String,
                String,
                String,
                Option<String>,
            ) = tx
                .query_row(
                    "SELECT tenant, target_key, secret_ref, delivery_kind, ordering_key, depends_on_outbox_id \
                     FROM provider_delivery_outbox WHERE id = ?1",
                    [&outbox_id],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                        ))
                    },
                )
                .map_err(internal)?;
            if tenant != org
                || target_key != connection_id
                || secret_ref != format!("discussion:{connection_id}")
                || kind != "discussion_tombstone"
                || ordering_key != discussion_ordering_key(&link.artifact_id, link.generation)?
                || dependency.is_none()
            {
                return Err(AppError::Validation("invalid discussion tombstone contract".to_owned()));
            }
            let discussion = discussion_row(&tx, &artifact_id, &org)?
                .ok_or(AppError::Conflict("discussion mirror is no longer active".to_owned()))?;
            if discussion.connection_id.as_deref() != Some(&connection_id)
                || discussion.generation != link.generation
            {
                return Err(AppError::Conflict("stale discussion tombstone authority".to_owned()));
            }
            let changed = tx.execute(
                "UPDATE discussion_message_links SET tombstone_outbox_id = ?1, updated_at = datetime('now') \
                 WHERE provider = 'discord' AND artifact_id = ?2 AND org = ?3 AND feedback_id = ?4 AND generation = ?5 \
                   AND connection_id = ?6 AND state = 'tombstone_pending' \
                   AND (tombstone_outbox_id IS NULL OR tombstone_outbox_id = ?1)",
                params![outbox_id, artifact_id, org, feedback_id, generation, connection_id],
            ).map_err(internal)?;
            if changed != 1 {
                return Err(AppError::Conflict("discussion tombstone idempotency conflict".to_owned()));
            }
            let link = link_by_tombstone_outbox(&tx, &outbox_id)?.ok_or(AppError::Internal)?;
            tx.commit().map_err(internal)?;
            Ok(link)
        }).await
    }

    /// Atomically accepts a claimed PBI-056 outbox row and writes the corresponding Discord IDs.
    /// A stale lease changes neither side. APP1 can call this after validating Discord's response.
    pub async fn accept_delivery_and_record_message(
        &self,
        input: AcceptedDiscussionDelivery,
    ) -> Result<bool, AppError> {
        validate_accepted(&input)?;
        db::interact(&self.pool, move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(internal)?;
            let link = link_by_outbox(&tx, &input.outbox_id)?.ok_or(AppError::Internal)?;
            let (kind, ordering_key): (String, String) = tx.query_row(
                "SELECT delivery_kind, ordering_key FROM provider_delivery_outbox WHERE id = ?1",
                [&input.outbox_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            ).map_err(internal)?;
            if !matches!(kind.as_str(), "discussion_thread" | "discussion_message")
                || ordering_key != discussion_ordering_key(&link.artifact_id, link.generation)?
            {
                return Err(AppError::Validation("invalid discussion delivery contract".to_owned()));
            }
            let discussion = discussion_row(&tx, &link.artifact_id.0, &link.org.0)?;
            let Some(discussion) = discussion else {
                return Err(AppError::Conflict("discussion mirror is no longer active".to_owned()));
            };
            // A re-enable can race a provider-accepted old-generation attempt. That old
            // receipt must be finalized for audit/idempotency, but may never update the new
            // generation's thread or status.
            let current = discussion.connection_id.as_deref() == Some(&link.connection_id)
                && discussion.generation == link.generation;
            if kind != "discussion_thread"
                && current
                && discussion.thread_id.as_deref() != Some(&input.external_thread_id)
                && link.external_thread_id.as_deref() != Some(&input.external_thread_id)
            {
                return Err(AppError::Conflict("stale discussion reply thread".to_owned()));
            }
            let accepted = accept_outbox_in_transaction(&tx, &input)?;
            if !accepted {
                return Ok(false);
            }
            if tx.execute(
                "UPDATE discussion_message_links SET external_thread_id = ?1, external_message_id = ?2, \
                 state = CASE WHEN local_deleted_at IS NULL THEN 'posted' ELSE 'tombstone_pending' END, \
                 last_error = NULL, posted_at = datetime('now'), updated_at = datetime('now') \
                 WHERE provider = 'discord' AND outbox_id = ?3",
                params![input.external_thread_id, input.external_message_id, input.outbox_id],
            ).map_err(internal)? != 1 {
                return Err(AppError::Internal);
            }
            if kind == "discussion_thread" && current {
                let updated = tx.execute(
                    "UPDATE artifact_discussions SET thread_id = ?1, root_message_id = ?2, \
                     state = CASE WHEN mode = 'discord_mirror' THEN 'connected' ELSE state END, \
                     last_synced_at = datetime('now'), last_error = NULL, updated_at = datetime('now') \
                     WHERE artifact_id = ?3 AND org = ?4 AND connection_id = ?5 AND generation = ?6",
                    params![input.external_thread_id, input.external_message_id, link.artifact_id.0, link.org.0, link.connection_id, i64::try_from(link.generation).map_err(|_| AppError::Internal)?],
                ).map_err(internal)?;
                if updated != 1 { return Err(AppError::Internal); }
            }
            tx.commit().map_err(internal)?;
            Ok(true)
        }).await
    }

    /// Atomically accepts a leased tombstone job and marks the original retained message mapping
    /// as tombstoned. The post's original outbox and Discord identifiers are deliberately left
    /// intact for audit and recovery evidence.
    pub async fn accept_tombstone_delivery(
        &self,
        input: AcceptedDiscussionDelivery,
    ) -> Result<bool, AppError> {
        validate_accepted(&input)?;
        db::interact(&self.pool, move |conn| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(internal)?;
            let link = link_by_tombstone_outbox(&tx, &input.outbox_id)?.ok_or(AppError::Internal)?;
            let (kind, ordering_key): (String, String) = tx
                .query_row(
                    "SELECT delivery_kind, ordering_key FROM provider_delivery_outbox WHERE id = ?1",
                    [&input.outbox_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .map_err(internal)?;
            if kind != "discussion_tombstone"
                || ordering_key != discussion_ordering_key(&link.artifact_id, link.generation)?
                || link.state != "tombstone_pending"
                || link.external_thread_id.as_deref() != Some(&input.external_thread_id)
                || link.external_message_id.as_deref() != Some(&input.external_message_id)
            {
                return Err(AppError::Conflict("stale discussion tombstone authority".to_owned()));
            }
            // A stale generation still owns its retained external message mapping. Finalize the
            // tombstone receipt without touching a newer artifact discussion row.
            let _discussion = discussion_row(&tx, &link.artifact_id.0, &link.org.0)?
                .ok_or(AppError::Conflict("discussion mirror is no longer active".to_owned()))?;
            if !accept_outbox_in_transaction(&tx, &input)? {
                return Ok(false);
            }
            if tx.execute(
                "UPDATE discussion_message_links SET state = 'tombstoned', last_error = NULL, updated_at = datetime('now') \
                 WHERE provider = 'discord' AND tombstone_outbox_id = ?1 AND state = 'tombstone_pending'",
                [&input.outbox_id],
            ).map_err(internal)? != 1 {
                return Err(AppError::Internal);
            }
            tx.commit().map_err(internal)?;
            Ok(true)
        }).await
    }

    /// Accept a resolved/reopened marker without inventing a message-link row. The caller has
    /// already proved that an original current-generation feedback mapping exists.
    pub async fn accept_marker_delivery(
        &self,
        input: AcceptedDiscussionMarker,
    ) -> Result<bool, AppError> {
        validate_marker(&input)?;
        db::interact(&self.pool, move |conn| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(internal)?;
            let discussion = discussion_row(&tx, &input.artifact_id.0, &input.org.0)?
                .ok_or(AppError::Conflict("discussion mirror is no longer active".to_owned()))?;
            let current = discussion.connection_id.as_deref() == Some(&input.connection_id)
                && discussion.generation == input.generation;
            if current && discussion.thread_id.is_none() {
                return Err(AppError::Conflict("stale discussion marker authority".to_owned()));
            }
            let (kind, tenant, target, secret, ordering): (String, String, String, String, String) = tx
                .query_row(
                    "SELECT delivery_kind, tenant, target_key, secret_ref, ordering_key FROM provider_delivery_outbox WHERE id = ?1",
                    [&input.outbox_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
                )
                .map_err(internal)?;
            if kind != "discussion_message"
                || tenant != input.org.0
                || target != input.connection_id
                || secret != format!("discussion:{}", input.connection_id)
                || ordering != discussion_ordering_key(&input.artifact_id, input.generation)?
            {
                return Err(AppError::Validation("invalid discussion marker contract".to_owned()));
            }
            let accepted = accept_outbox_raw(
                &tx,
                &input.outbox_id,
                &input.worker,
                &input.lease_token,
                input.lease_version,
                &input.message_id,
                input.now_millis,
            )?;
            if !accepted {
                return Ok(false);
            }
            if current {
                tx.execute(
                    "UPDATE artifact_discussions SET last_synced_at = datetime('now'), last_error = NULL, updated_at = datetime('now') \
                     WHERE artifact_id = ?1 AND org = ?2 AND connection_id = ?3 AND generation = ?4",
                    params![input.artifact_id.0, input.org.0, input.connection_id, i64::try_from(input.generation).map_err(|_| AppError::Internal)?],
                )
                .map_err(internal)?;
            }
            tx.commit().map_err(internal)?;
            Ok(true)
        })
        .await
    }

    /// Atomically dead-letters a discussion job and records only state appropriate to its
    /// operation. A stale generation can transition its own outbox but never changes a newer
    /// mirror's visible state.
    pub async fn terminal_delivery(
        &self,
        input: TerminalDiscussionDelivery,
    ) -> Result<bool, AppError> {
        validate_terminal(&input)?;
        db::interact(&self.pool, move |conn| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(internal)?;
            let changed = dead_letter_in_transaction(
                &tx,
                &input.outbox_id,
                &input.worker,
                &input.lease_token,
                input.lease_version,
                DeadLetterTransition {
                    classification: input.classification.clone(),
                    error: input.classification.clone(),
                    duplicate_risk: input.duplicate_risk,
                },
                input.now_millis,
            )?;
            if !changed {
                return Ok(false);
            }
            let current = discussion_row(&tx, &input.artifact_id.0, &input.org.0)?
                .is_some_and(|discussion| {
                    discussion.connection_id.as_deref() == Some(&input.connection_id)
                        && discussion.generation == input.generation
                });
            if current {
                match input.delivery_kind.as_str() {
                    "discussion_thread" => {
                        tx.execute(
                            "UPDATE artifact_discussions SET state = 'failed', last_error = ?1, updated_at = datetime('now') \
                             WHERE artifact_id = ?2 AND org = ?3 AND connection_id = ?4 AND generation = ?5",
                            params![input.classification, input.artifact_id.0, input.org.0, input.connection_id, i64::try_from(input.generation).map_err(|_| AppError::Internal)?],
                        ).map_err(internal)?;
                    }
                    "discussion_message" => {
                        if let Some(feedback_id) = &input.feedback_id {
                            tx.execute(
                                "UPDATE discussion_message_links SET state = 'failed', last_error = ?1, updated_at = datetime('now') \
                                 WHERE provider = 'discord' AND artifact_id = ?2 AND org = ?3 AND feedback_id = ?4 \
                                   AND connection_id = ?5 AND generation = ?6 AND outbox_id = ?7",
                                params![input.classification, input.artifact_id.0, input.org.0, feedback_id.0, input.connection_id, i64::try_from(input.generation).map_err(|_| AppError::Internal)?, input.outbox_id],
                            ).map_err(internal)?;
                        } else {
                            tx.execute(
                                "UPDATE artifact_discussions SET last_error = ?1, updated_at = datetime('now') \
                                 WHERE artifact_id = ?2 AND org = ?3 AND connection_id = ?4 AND generation = ?5",
                                params![input.classification, input.artifact_id.0, input.org.0, input.connection_id, i64::try_from(input.generation).map_err(|_| AppError::Internal)?],
                            ).map_err(internal)?;
                        }
                    }
                    "discussion_tombstone" => {
                        if let Some(feedback_id) = &input.feedback_id {
                            tx.execute(
                                "UPDATE discussion_message_links SET state = 'failed', last_error = ?1, updated_at = datetime('now') \
                                 WHERE provider = 'discord' AND artifact_id = ?2 AND org = ?3 AND feedback_id = ?4 \
                                   AND connection_id = ?5 AND generation = ?6 AND tombstone_outbox_id = ?7",
                                params![input.classification, input.artifact_id.0, input.org.0, feedback_id.0, input.connection_id, i64::try_from(input.generation).map_err(|_| AppError::Internal)?, input.outbox_id],
                            ).map_err(internal)?;
                        }
                    }
                    _ => return Err(AppError::Validation("invalid discussion delivery kind".to_owned())),
                }
            }
            tx.commit().map_err(internal)?;
            Ok(true)
        })
        .await
    }
}

/// Non-secret ordering identity for one artifact discussion generation.
pub fn discussion_ordering_key(
    artifact_id: &ArtifactId,
    generation: u64,
) -> Result<String, AppError> {
    let id = trimmed(&artifact_id.0);
    if id.is_empty() || id.contains('\0') || id.len() > 120 || generation == 0 {
        return Err(AppError::Validation(
            "invalid discussion ordering key".to_owned(),
        ));
    }
    let value = format!("discussion:{id}:{generation}");
    if value.len() > 160 {
        return Err(AppError::Validation(
            "invalid discussion ordering key".to_owned(),
        ));
    }
    Ok(value)
}

/// The generation-scoped authority a feedback transaction may use to plan new work.  This is
/// deliberately transaction-local: an artifact can be disabled or re-enabled between requests,
/// but never in the middle of a single feedback mutation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ActiveDiscussionPlan {
    pub connection_id: String,
    pub generation: u64,
    pub ordering_key: String,
    pub anchor_outbox_id: Option<String>,
    pub notification_webhook_id: Option<String>,
}

/// Return an active mirror only.  Missing, disabled, and paused mirrors are all local-only from
/// the perspective of a new feedback producer; already committed rows drain independently.
pub(crate) fn active_plan_in_transaction(
    tx: &Transaction<'_>,
    artifact_id: &ArtifactId,
    org: &OrgId,
) -> Result<Option<ActiveDiscussionPlan>, AppError> {
    let explicit_local: bool = tx
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM artifact_discussion_overrides \
             WHERE artifact_id=?1 AND org=?2 AND mode='artifact_only')",
            params![artifact_id.0, org.0],
            |row| row.get(0),
        )
        .map_err(internal)?;
    if explicit_local {
        return Ok(None);
    }
    let policy = tx
        .query_row(
            "SELECT outbound_enabled FROM org_discord_threading_policies WHERE org=?1",
            [&org.0],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(internal)?;
    let current = discussion_row(tx, &artifact_id.0, &org.0)?;
    // A migrated PBI-079 mirror remains authoritative while no PBI-081 policy row exists.
    // Once an administrator saves an explicit policy, disabling it is a hard planning boundary.
    if policy == Some(0) {
        return Ok(None);
    }
    let discussion = match current {
        Some(row) if row.mode == DiscussionMode::DiscordMirror => row,
        existing if policy == Some(1) => {
            let authority = match mirror_authority(tx, &artifact_id.0, &org.0) {
                Ok(authority) => authority,
                // Missing/ambiguous historical evidence must never roll back canonical feedback.
                Err(AppError::Conflict(_)) | Err(AppError::Validation(_)) => return Ok(None),
                Err(error) => return Err(error),
            };
            let generation = existing
                .as_ref()
                .map_or(1, |row| row.generation.saturating_add(1).max(1));
            tx.execute(
                "INSERT INTO artifact_discussions \
                 (artifact_id, org, provider, mode, connection_org, connection_id, state, \
                  generation, enabled_by, enabled_at, anchor_outbox_id) \
                 VALUES (?1, ?2, 'discord', 'discord_mirror', ?2, ?3, 'pending', ?4, \
                         'organization-policy', datetime('now'), ?5) \
                 ON CONFLICT(artifact_id) DO UPDATE SET mode='discord_mirror', state='pending', \
                   connection_org=excluded.connection_org, connection_id=excluded.connection_id, \
                   generation=excluded.generation, enabled_by='organization-policy', \
                   enabled_at=datetime('now'), disabled_at=NULL, thread_id=NULL, \
                   root_message_id=NULL, last_synced_at=NULL, last_error=NULL, \
                   anchor_outbox_id=excluded.anchor_outbox_id, updated_at=datetime('now')",
                params![
                    artifact_id.0,
                    org.0,
                    authority.connection_id,
                    i64::try_from(generation).map_err(|_| AppError::Internal)?,
                    authority.anchor_outbox_id
                ],
            )
            .map_err(internal)?;
            discussion_row(tx, &artifact_id.0, &org.0)?.ok_or(AppError::Internal)?
        }
        _ => return Ok(None),
    };
    if discussion.mode != DiscussionMode::DiscordMirror || discussion.generation == 0 {
        return Ok(None);
    }
    let Some(connection_id) = discussion.connection_id else {
        return Ok(None);
    };
    let Some(connection) = connection_row_by_id(tx, &connection_id, &org.0)? else {
        return Ok(None);
    };
    Ok(Some(ActiveDiscussionPlan {
        ordering_key: discussion_ordering_key(artifact_id, discussion.generation)?,
        connection_id,
        generation: discussion.generation,
        anchor_outbox_id: discussion.anchor_outbox_id,
        notification_webhook_id: connection
            .strategy
            .eq(&DiscussionConnectionStrategy::NotificationThread)
            .then_some(connection.notification_webhook_id)
            .flatten(),
    }))
}

/// The root may not have reached Discord yet, so this reads its durable outbox identity rather
/// than `artifact_discussions.root_message_id`.  It gives concurrent sequential transactions one
/// root job per generation.
pub(crate) fn root_outbox_in_transaction(
    tx: &Transaction<'_>,
    _artifact_id: &ArtifactId,
    org: &OrgId,
    plan: &ActiveDiscussionPlan,
) -> Result<Option<String>, AppError> {
    tx.query_row(
        "SELECT id FROM provider_delivery_outbox \
         WHERE provider = 'discord' AND tenant = ?1 AND target_key = ?2 \
           AND secret_ref = ?3 AND delivery_kind = 'discussion_thread' \
           AND ordering_key = ?4 \
         ORDER BY created_at ASC, id ASC LIMIT 1",
        params![
            org.0,
            plan.connection_id,
            format!("discussion:{}", plan.connection_id),
            plan.ordering_key,
        ],
        |row| row.get(0),
    )
    .optional()
    .map_err(internal)
}

pub(crate) fn link_for_feedback_generation_in_transaction(
    tx: &Transaction<'_>,
    artifact_id: &ArtifactId,
    org: &OrgId,
    feedback_id: &FeedbackId,
    generation: u64,
) -> Result<Option<DiscussionMessageLink>, AppError> {
    let generation = i64::try_from(generation)
        .map_err(|_| AppError::Validation("invalid discussion generation".to_owned()))?;
    link_by_feedback_generation(tx, &artifact_id.0, &org.0, &feedback_id.0, generation)
}

/// The newest retained link for a feedback item.  Delete planning uses this only to determine
/// whether a paused (but not re-enabled) generation still has authority to tombstone its prior
/// Discord content.
pub(crate) fn latest_link_for_feedback_in_transaction(
    tx: &Transaction<'_>,
    artifact_id: &ArtifactId,
    org: &OrgId,
    feedback_id: &FeedbackId,
) -> Result<Option<DiscussionMessageLink>, AppError> {
    link_by_feedback(tx, &artifact_id.0, &org.0, &feedback_id.0)
}

/// A disabled mirror retains authority to drain and tombstone committed work in its exact
/// generation.  A later re-enable changes the generation, so an old link deliberately receives
/// no plan and cannot send through the new discussion.
pub(crate) fn retained_tombstone_plan_in_transaction(
    tx: &Transaction<'_>,
    link: &DiscussionMessageLink,
) -> Result<Option<ActiveDiscussionPlan>, AppError> {
    let Some(discussion) = discussion_row(tx, &link.artifact_id.0, &link.org.0)? else {
        return Ok(None);
    };
    if discussion.connection_id.as_deref() != Some(&link.connection_id)
        || discussion.generation != link.generation
    {
        return Ok(None);
    }
    Ok(Some(ActiveDiscussionPlan {
        connection_id: link.connection_id.clone(),
        generation: link.generation,
        ordering_key: discussion_ordering_key(&link.artifact_id, link.generation)?,
        anchor_outbox_id: discussion.anchor_outbox_id,
        notification_webhook_id: connection_row_by_id(tx, &link.connection_id, &link.org.0)?
            .and_then(|connection| {
                (connection.strategy == DiscussionConnectionStrategy::NotificationThread)
                    .then_some(connection.notification_webhook_id)
                    .flatten()
            }),
    }))
}

/// Insert the immutable feedback/outbox correlation in the caller's feedback transaction.
/// The unique `(provider, feedback_id, generation)` key makes a retry observe the exact original
/// row instead of rewriting a producer decision.
pub(crate) fn create_link_in_transaction(
    tx: &Transaction<'_>,
    input: &CreateDiscussionMessageLink,
) -> Result<DiscussionMessageLink, AppError> {
    let generation = i64::try_from(input.generation)
        .map_err(|_| AppError::Validation("invalid discussion generation".to_owned()))?;
    if generation < 1 {
        return Err(AppError::Validation(
            "invalid discussion generation".to_owned(),
        ));
    }
    tx.execute(
        "INSERT OR IGNORE INTO discussion_message_links \
         (provider, artifact_id, org, connection_id, feedback_id, delivery_event_id, outbox_id, external_thread_id, generation, state) \
         VALUES ('discord', ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending')",
        params![
            input.artifact_id.0,
            input.org.0,
            input.connection_id,
            input.feedback_id.0,
            input.delivery_event_id,
            input.outbox_id,
            input.external_thread_id,
            generation,
        ],
    )
    .map_err(internal)?;
    let row = link_by_feedback_generation(
        tx,
        &input.artifact_id.0,
        &input.org.0,
        &input.feedback_id.0,
        generation,
    )?
    .ok_or(AppError::Internal)?;
    if row.connection_id != input.connection_id
        || row.delivery_event_id != input.delivery_event_id
        || row.outbox_id != input.outbox_id
    {
        return Err(AppError::Conflict(
            "discussion delivery idempotency conflict".to_owned(),
        ));
    }
    Ok(row)
}

/// Mark retained delivery evidence deleted and bind its already-created tombstone job in the same
/// feedback transaction.  A post that has not yet been accepted stays `local_deleted`; its
/// acceptance path promotes it to `tombstone_pending` rather than resurrecting it as posted.
pub(crate) fn mark_deleted_and_bind_tombstone_in_transaction(
    tx: &Transaction<'_>,
    link: &DiscussionMessageLink,
    tombstone_outbox_id: &str,
) -> Result<(), AppError> {
    let changed = tx.execute(
        "UPDATE discussion_message_links \
         SET state = CASE WHEN external_message_id IS NULL THEN 'local_deleted' ELSE 'tombstone_pending' END, \
             local_deleted_at = COALESCE(local_deleted_at, datetime('now')), \
             tombstone_outbox_id = COALESCE(tombstone_outbox_id, ?1), updated_at = datetime('now') \
         WHERE provider = 'discord' AND artifact_id = ?2 AND org = ?3 AND feedback_id = ?4 \
           AND generation = ?5 AND connection_id = ?6 \
           AND (tombstone_outbox_id IS NULL OR tombstone_outbox_id = ?1)",
        params![
            tombstone_outbox_id,
            link.artifact_id.0,
            link.org.0,
            link.feedback_id.0,
            i64::try_from(link.generation).map_err(|_| AppError::Internal)?,
            link.connection_id,
        ],
    )
    .map_err(internal)?;
    if changed != 1 {
        return Err(AppError::Conflict(
            "discussion tombstone idempotency conflict".to_owned(),
        ));
    }
    Ok(())
}

fn accept_outbox_in_transaction(
    tx: &Transaction<'_>,
    input: &AcceptedDiscussionDelivery,
) -> Result<bool, AppError> {
    Ok(tx.execute(
        "UPDATE provider_delivery_outbox SET state = 'accepted', lease_owner = NULL, lease_expires_at = NULL, \
         lease_token = NULL, next_attempt_at = ?1, result_classification = 'accepted', duplicate_risk = 0, \
         discord_message_id = ?2, terminal_error = '', updated_at = ?1, completed_at = ?1 \
         WHERE id = ?3 AND state = 'leased' AND lease_owner = ?4 AND lease_token = ?5 AND lease_version = ?6",
        params![input.now_millis, input.external_message_id, input.outbox_id, input.worker, input.lease_token, input.lease_version],
    ).map_err(internal)? == 1)
}

fn accept_outbox_raw(
    tx: &Transaction<'_>,
    outbox_id: &str,
    worker: &str,
    lease_token: &str,
    lease_version: i64,
    message_id: &str,
    now_millis: i64,
) -> Result<bool, AppError> {
    Ok(tx.execute(
        "UPDATE provider_delivery_outbox SET state = 'accepted', lease_owner = NULL, lease_expires_at = NULL, \
         lease_token = NULL, next_attempt_at = ?1, result_classification = 'accepted', duplicate_risk = 0, \
         discord_message_id = ?2, terminal_error = '', updated_at = ?1, completed_at = ?1 \
         WHERE id = ?3 AND state = 'leased' AND lease_owner = ?4 AND lease_token = ?5 AND lease_version = ?6",
        params![now_millis, message_id, outbox_id, worker, lease_token, lease_version],
    )
    .map_err(internal)? == 1)
}

fn validate_marker(input: &AcceptedDiscussionMarker) -> Result<(), AppError> {
    require_nonempty(&[
        &input.outbox_id,
        &input.worker,
        &input.lease_token,
        &input.artifact_id.0,
        &input.org.0,
        &input.connection_id,
        &input.message_id,
    ])?;
    if input.lease_version < 1 || input.generation == 0 || input.now_millis < 0 {
        return Err(AppError::Validation(
            "invalid marker delivery acceptance".to_owned(),
        ));
    }
    Ok(())
}

fn validate_terminal(input: &TerminalDiscussionDelivery) -> Result<(), AppError> {
    require_nonempty(&[
        &input.outbox_id,
        &input.worker,
        &input.lease_token,
        &input.artifact_id.0,
        &input.org.0,
        &input.connection_id,
        &input.classification,
    ])?;
    if input.lease_version < 1 || input.generation == 0 || input.now_millis < 0 {
        return Err(AppError::Validation(
            "invalid discussion terminal delivery".to_owned(),
        ));
    }
    Ok(())
}

#[derive(Clone)]
struct DiscussionConnectionRow {
    id: String,
    org: OrgId,
    url: String,
    encrypted: Option<EncryptedUrl>,
    label: String,
    created_at: Timestamp,
    updated_at: Timestamp,
    last_ok_at: Option<Timestamp>,
    last_error: Option<String>,
    strategy: DiscussionConnectionStrategy,
    notification_webhook_id: Option<String>,
    channel_id: Option<String>,
    guild_id: Option<String>,
    notification_provider_webhook_id: Option<String>,
}

impl DiscussionConnectionRow {
    fn stored_url(&self) -> StoredWebhookUrl {
        StoredWebhookUrl {
            url: self.url.clone(),
            encrypted: self.encrypted.clone(),
        }
    }

    fn summary(self) -> DiscussionConnectionSummary {
        let destination = match self.strategy {
            DiscussionConnectionStrategy::ForumWebhook => mask_url(&self.url),
            DiscussionConnectionStrategy::NotificationThread => {
                self.channel_id.as_deref().map_or_else(
                    || "Discord notification channel".to_owned(),
                    |id| format!("Discord channel {id}"),
                )
            }
        };
        DiscussionConnectionSummary {
            id: self.id,
            org: self.org,
            label: self.label,
            destination,
            strategy: self.strategy,
            notification_webhook_id: self.notification_webhook_id,
            created_at: self.created_at,
            updated_at: self.updated_at,
            last_ok_at: self.last_ok_at,
            last_error: self.last_error,
        }
    }

    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        let cipher: Option<String> = row.get("url_cipher")?;
        let nonce: Option<String> = row.get("url_nonce")?;
        let tag: Option<String> = row.get("url_tag")?;
        Ok(Self {
            id: row.get("id")?,
            org: OrgId(row.get("org")?),
            url: row.get("url")?,
            encrypted: cipher.map(|ciphertext| EncryptedUrl {
                ciphertext,
                nonce: nonce.unwrap_or_default(),
                tag: tag.unwrap_or_default(),
            }),
            label: row.get("label")?,
            created_at: Timestamp(row.get("created_at")?),
            updated_at: Timestamp(row.get("updated_at")?),
            last_ok_at: row.get::<_, Option<String>>("last_ok_at")?.map(Timestamp),
            last_error: row.get("last_error")?,
            strategy: DiscussionConnectionStrategy::parse(row.get("strategy")?)?,
            notification_webhook_id: row.get("notification_webhook_id")?,
            channel_id: row.get("channel_id")?,
            guild_id: row.get("guild_id")?,
            notification_provider_webhook_id: row.get("notification_provider_webhook_id")?,
        })
    }
}

fn delivery_url_for_connection(
    conn: &Connection,
    row: &DiscussionConnectionRow,
) -> Result<StoredWebhookUrl, AppError> {
    if row.strategy == DiscussionConnectionStrategy::ForumWebhook {
        return Ok(row.stored_url());
    }
    let webhook_id = row
        .notification_webhook_id
        .as_deref()
        .ok_or(AppError::Internal)?;
    conn.query_row(
        "SELECT url, url_cipher, url_nonce, url_tag FROM org_webhooks \
         WHERE id = ?1 AND org = ?2",
        params![webhook_id, row.org.0],
        |source| {
            let cipher: Option<String> = source.get(1)?;
            let nonce: Option<String> = source.get(2)?;
            let tag: Option<String> = source.get(3)?;
            Ok(StoredWebhookUrl {
                url: source.get(0)?,
                encrypted: cipher.map(|ciphertext| EncryptedUrl {
                    ciphertext,
                    nonce: nonce.unwrap_or_default(),
                    tag: tag.unwrap_or_default(),
                }),
            })
        },
    )
    .optional()
    .map_err(internal)?
    .ok_or(AppError::Internal)
}

fn connection_row(
    conn: &Connection,
    org: &str,
) -> Result<Option<DiscussionConnectionRow>, AppError> {
    conn.query_row(
        &format!(
            "SELECT {CONNECTION_COLUMNS} FROM org_discord_discussion_connections WHERE org = ?1"
        ),
        [org],
        DiscussionConnectionRow::from_row,
    )
    .optional()
    .map_err(internal)
}

fn connection_row_by_id(
    conn: &Connection,
    id: &str,
    org: &str,
) -> Result<Option<DiscussionConnectionRow>, AppError> {
    conn.query_row(
        &format!(
            "SELECT {CONNECTION_COLUMNS} FROM org_discord_discussion_connections WHERE id = ?1 AND org = ?2"
        ),
        params![id, org],
        DiscussionConnectionRow::from_row,
    )
    .optional()
    .map_err(internal)
}

struct MirrorAuthority {
    connection_id: String,
    anchor_outbox_id: Option<String>,
}

fn mirror_authority(
    conn: &Connection,
    artifact_id: &str,
    org: &str,
) -> Result<MirrorAuthority, AppError> {
    let Some(connection) = connection_row(conn, org)? else {
        return Err(AppError::Validation(
            "Discord discussion connection is not configured.".to_owned(),
        ));
    };
    let anchor_outbox_id = if connection.strategy
        == DiscussionConnectionStrategy::NotificationThread
    {
        let webhook_id = connection
            .notification_webhook_id
            .as_deref()
            .ok_or(AppError::Internal)?;
        let subject = format!("artifact:{artifact_id}:1");
        let event_id =
            stable_delivery_event_id(&OrgId(org.to_owned()), &WebhookEvent::Published, &subject);
        let anchor = conn
            .query_row(
                "SELECT id, state FROM provider_delivery_outbox \
                 WHERE provider = 'discord' AND tenant = ?1 AND target_key = ?2 \
                   AND event_id = ?3 AND event_type = 'published' AND delivery_kind = 'event' \
                 ORDER BY created_at ASC, id ASC LIMIT 1",
                params![org, webhook_id, event_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(internal)?;
        match anchor {
            Some((_, state)) if state == "dead_letter" => {
                return Err(AppError::Conflict(
                    "The artifact publication notification failed, so it cannot anchor a Discord thread."
                        .to_owned(),
                ));
            }
            Some((id, _)) => Some(id),
            None => {
                let recovered: bool = conn
                    .query_row(
                        "SELECT EXISTS(\
                           SELECT 1 FROM discord_notification_anchor_recoveries r \
                           WHERE r.artifact_id=?1 AND r.org=?2 AND r.connection_id=?3 \
                             AND r.notification_webhook_id=?4 AND r.guild_id=?5 \
                             AND r.channel_id=?6 AND r.provider_webhook_id=?7 \
                             AND r.state='recovered' \
                             AND r.provenance='exact_selected_webhook_canonical_url'\
                         )",
                        params![
                            artifact_id,
                            org,
                            connection.id,
                            webhook_id,
                            connection.guild_id.as_deref().unwrap_or_default(),
                            connection.channel_id.as_deref().unwrap_or_default(),
                            connection
                                .notification_provider_webhook_id
                                .as_deref()
                                .unwrap_or_default()
                        ],
                        |row| row.get(0),
                    )
                    .map_err(internal)?;
                if !recovered {
                    return Err(AppError::Conflict(
                        "The original publication notification is unavailable. Historical recovery must find one exact selected-webhook match before Discord threading can start."
                            .to_owned(),
                    ));
                }
                None
            }
        }
    } else {
        None
    };
    Ok(MirrorAuthority {
        connection_id: connection.id,
        anchor_outbox_id,
    })
}

fn discussion_row(
    conn: &Connection,
    artifact_id: &str,
    org: &str,
) -> Result<Option<ArtifactDiscussion>, AppError> {
    conn.query_row(
        &format!("SELECT {DISCUSSION_COLUMNS} FROM artifact_discussions WHERE artifact_id = ?1 AND org = ?2"),
        params![artifact_id, org],
        |row| Ok(ArtifactDiscussion {
            artifact_id: ArtifactId(row.get(0)?), org: OrgId(row.get(1)?),
            mode: DiscussionMode::parse(row.get(3)?)?,
            connection_org: row.get::<_, Option<String>>(4)?.map(OrgId), connection_id: row.get(5)?, thread_id: row.get(6)?,
            root_message_id: row.get(7)?,
            state: DiscussionState::parse(row.get(8)?)?,
            generation: row.get::<_, i64>(9)?.unsigned_abs(),
            enabled_by: row.get(10)?,
            enabled_at: row.get::<_, Option<String>>(11)?.map(Timestamp),
            disabled_at: row.get::<_, Option<String>>(12)?.map(Timestamp),
            last_synced_at: row.get::<_, Option<String>>(13)?.map(Timestamp), last_error: row.get(14)?,
            created_at: row.get::<_, Option<String>>(15)?.map(Timestamp),
            updated_at: row.get::<_, Option<String>>(16)?.map(Timestamp),
            anchor_outbox_id: row.get(17)?,
        }),
    ).optional().map_err(internal)
}

fn link_by_feedback(
    conn: &Connection,
    artifact_id: &str,
    org: &str,
    feedback_id: &str,
) -> Result<Option<DiscussionMessageLink>, AppError> {
    conn.query_row(
        &format!("SELECT {LINK_COLUMNS} FROM discussion_message_links WHERE provider = 'discord' AND artifact_id = ?1 AND org = ?2 AND feedback_id = ?3 ORDER BY generation DESC LIMIT 1"),
        params![artifact_id, org, feedback_id],
        |row| Ok(DiscussionMessageLink {
            artifact_id: ArtifactId(row.get(1)?), org: OrgId(row.get(2)?), connection_id: row.get(3)?, feedback_id: FeedbackId(row.get(4)?),
            delivery_event_id: row.get(5)?, outbox_id: row.get(6)?, tombstone_outbox_id: row.get(7)?, external_thread_id: row.get(8)?,
            external_message_id: row.get(9)?, generation: row.get::<_, i64>(11)?.unsigned_abs(),
            state: row.get(12)?, last_error: row.get(13)?,
            local_deleted_at: row.get::<_, Option<String>>(14)?.map(Timestamp),
            created_at: Timestamp(row.get(15)?), updated_at: Timestamp(row.get(16)?),
            posted_at: row.get::<_, Option<String>>(17)?.map(Timestamp),
        }),
    ).optional().map_err(internal)
}

fn link_by_outbox(
    conn: &Connection,
    outbox_id: &str,
) -> Result<Option<DiscussionMessageLink>, AppError> {
    conn.query_row(
        &format!("SELECT {LINK_COLUMNS} FROM discussion_message_links WHERE provider = 'discord' AND outbox_id = ?1"),
        [outbox_id],
        |row| Ok(DiscussionMessageLink {
            artifact_id: ArtifactId(row.get(1)?), org: OrgId(row.get(2)?), connection_id: row.get(3)?, feedback_id: FeedbackId(row.get(4)?),
            delivery_event_id: row.get(5)?, outbox_id: row.get(6)?, tombstone_outbox_id: row.get(7)?, external_thread_id: row.get(8)?,
            external_message_id: row.get(9)?, generation: row.get::<_, i64>(11)?.unsigned_abs(),
            state: row.get(12)?, last_error: row.get(13)?,
            local_deleted_at: row.get::<_, Option<String>>(14)?.map(Timestamp),
            created_at: Timestamp(row.get(15)?), updated_at: Timestamp(row.get(16)?),
            posted_at: row.get::<_, Option<String>>(17)?.map(Timestamp),
        }),
    ).optional().map_err(internal)
}

fn link_by_feedback_generation(
    conn: &Connection,
    artifact_id: &str,
    org: &str,
    feedback_id: &str,
    generation: i64,
) -> Result<Option<DiscussionMessageLink>, AppError> {
    conn.query_row(
        &format!("SELECT {LINK_COLUMNS} FROM discussion_message_links WHERE provider = 'discord' AND artifact_id = ?1 AND org = ?2 AND feedback_id = ?3 AND generation = ?4"),
        params![artifact_id, org, feedback_id, generation],
        discussion_link_row,
    ).optional().map_err(internal)
}

fn link_by_tombstone_outbox(
    conn: &Connection,
    outbox_id: &str,
) -> Result<Option<DiscussionMessageLink>, AppError> {
    conn.query_row(
        &format!("SELECT {LINK_COLUMNS} FROM discussion_message_links WHERE provider = 'discord' AND tombstone_outbox_id = ?1"),
        [outbox_id],
        discussion_link_row,
    ).optional().map_err(internal)
}

fn discussion_link_row(row: &Row<'_>) -> rusqlite::Result<DiscussionMessageLink> {
    Ok(DiscussionMessageLink {
        artifact_id: ArtifactId(row.get(1)?),
        org: OrgId(row.get(2)?),
        connection_id: row.get(3)?,
        feedback_id: FeedbackId(row.get(4)?),
        delivery_event_id: row.get(5)?,
        outbox_id: row.get(6)?,
        tombstone_outbox_id: row.get(7)?,
        external_thread_id: row.get(8)?,
        external_message_id: row.get(9)?,
        generation: row.get::<_, i64>(11)?.unsigned_abs(),
        state: row.get(12)?,
        last_error: row.get(13)?,
        local_deleted_at: row.get::<_, Option<String>>(14)?.map(Timestamp),
        created_at: Timestamp(row.get(15)?),
        updated_at: Timestamp(row.get(16)?),
        posted_at: row.get::<_, Option<String>>(17)?.map(Timestamp),
    })
}

fn artifact_exists(conn: &Connection, artifact_id: &str, org: &str) -> Result<bool, AppError> {
    conn.query_row(
        "SELECT 1 FROM artifacts WHERE id = ?1 AND org = ?2",
        params![artifact_id, org],
        |_| Ok(()),
    )
    .optional()
    .map(|row: Option<()>| row.is_some())
    .map_err(internal)
}

fn org_exists(conn: &Connection, org: &str) -> Result<bool, AppError> {
    conn.query_row("SELECT 1 FROM orgs WHERE name = ?1", [org], |_| Ok(()))
        .optional()
        .map(|row: Option<()>| row.is_some())
        .map_err(internal)
}

fn notification_webhook_is_eligible(
    conn: &Connection,
    org: &str,
    webhook_id: &str,
) -> Result<bool, AppError> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM org_webhooks \
         WHERE id = ?1 AND org = ?2 \
           AND instr(',' || events || ',', ',published,') > 0)",
        params![webhook_id, org],
        |row| row.get(0),
    )
    .map_err(internal)
}

fn valid_discord_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 32 && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn has_bound_connection_authority(conn: &Connection, org: &str) -> Result<bool, AppError> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM artifact_discussions WHERE org = ?1 AND connection_id IS NOT NULL) \
         OR EXISTS(SELECT 1 FROM discussion_message_links WHERE org = ?1)",
        [org],
        |row| row.get(0),
    ).map_err(internal)
}

fn require_nonempty(values: &[&str]) -> Result<(), AppError> {
    if values
        .iter()
        .any(|value| value.is_empty() || value.contains('\0'))
    {
        Err(AppError::Validation(
            "discussion identity fields are required".to_owned(),
        ))
    } else {
        Ok(())
    }
}

fn validate_accepted(input: &AcceptedDiscussionDelivery) -> Result<(), AppError> {
    require_nonempty(&[
        &input.outbox_id,
        &input.worker,
        &input.lease_token,
        &input.external_thread_id,
        &input.external_message_id,
    ])?;
    if input.lease_version < 1 || input.now_millis < 0 {
        return Err(AppError::Validation(
            "invalid delivery acceptance".to_owned(),
        ));
    }
    Ok(())
}

fn trimmed(value: &str) -> String {
    value.trim().to_owned()
}

fn internal(error: rusqlite::Error) -> AppError {
    // SQLite's error does not interpolate parameter values. Keep the operation broad anyway so a
    // bearer URL cannot become part of an operator log through a future query refactor.
    tracing::error!(error = %error, "discord discussion persistence failed");
    AppError::Internal
}

impl std::fmt::Debug for DiscussionStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscussionStore")
            .field("protection", self.protection.as_ref())
            .finish_non_exhaustive()
    }
}
