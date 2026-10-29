//! Helpers that plan discussion work inside a caller's feedback transaction.

use rusqlite::{OptionalExtension, Transaction, params};

use super::{
    connection_rows::connection_row_by_id,
    discussion_rows::{discussion_row, mirror_authority},
    inputs::CreateDiscussionMessageLink,
    internal,
    link_rows::{link_by_feedback, link_by_feedback_generation},
    support::trimmed,
    types::{DiscussionConnectionStrategy, DiscussionMessageLink, DiscussionMode},
};
use crate::{
    error::AppError,
    model::{ArtifactId, FeedbackId, OrgId},
};

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
