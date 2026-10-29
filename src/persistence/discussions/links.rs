//! `DiscussionStore` operations on feedback message links and tombstone binding.

use rusqlite::{TransactionBehavior, params};

use super::{
    DiscussionStore,
    discussion_rows::discussion_row,
    inputs::{BindDiscussionTombstone, CreateDiscussionMessageLink},
    internal,
    link_rows::{
        link_by_feedback, link_by_feedback_generation, link_by_outbox, link_by_tombstone_outbox,
    },
    planning::discussion_ordering_key,
    support::{require_nonempty, trimmed},
    types::DiscussionMessageLink,
};
use crate::{
    error::AppError,
    model::{ArtifactId, FeedbackId, OrgId},
    persistence::{db, webhooks::truncate_utf16},
};

impl DiscussionStore {
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
}
