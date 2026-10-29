//! `DiscussionStore` operations that settle leased delivery jobs against discussion state.

use rusqlite::{Transaction, TransactionBehavior, params};

use super::{
    DiscussionStore,
    discussion_rows::discussion_row,
    inputs::{AcceptedDiscussionDelivery, AcceptedDiscussionMarker, TerminalDiscussionDelivery},
    internal,
    link_rows::{link_by_outbox, link_by_tombstone_outbox},
    planning::discussion_ordering_key,
    support::require_nonempty,
};
use crate::{
    error::AppError,
    persistence::{
        db,
        outbox::{DeadLetterTransition, dead_letter_in_transaction},
    },
};

impl DiscussionStore {
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
