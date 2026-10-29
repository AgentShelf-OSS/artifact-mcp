//! `DiscussionStore` operations that enable, disable, retry, and read an artifact's mirror.

use rusqlite::{OptionalExtension, TransactionBehavior, params};

use super::{
    DiscussionStore,
    discussion_rows::{artifact_exists, discussion_row, mirror_authority},
    internal,
    support::{append_audit_then_commit, trimmed},
    types::{ArtifactDiscussion, DiscussionMode, DiscussionState},
};
use crate::{
    error::AppError,
    integrations::delivery_envelope::stable_delivery_event_id,
    model::{ArtifactId, OrgId, WebhookEvent},
    persistence::{db, webhooks::truncate_utf16},
    security::audit::{AuditEvent, MutationAudit},
};

impl DiscussionStore {
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
}
